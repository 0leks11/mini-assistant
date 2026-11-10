#!/bin/bash
# scripts/verify_deploy.sh — post-deploy verifier for GPT Mini.
#
# Checks that the running device really exposes every feature from the
# registry in docs/DEVELOPMENT_RULES.md, and that the deployed binary matches
# the local HEAD / build artifact. Prints PASS/FAIL per check, exits 1 on FAIL.
#
# Usage: ./scripts/verify_deploy.sh [--expected-sha SHA256]

set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MINI_DEPLOY_DIR="/data/opt/mini-assistant"
TARGET="armv7-unknown-linux-musleabihf"
EXPECTED_SHA=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --expected-sha) EXPECTED_SHA="$2"; shift 2 ;;
        -h|--help) echo "Usage: $0 [--expected-sha SHA256]"; exit 0 ;;
        *) echo "ERROR: unknown argument" >&2; exit 2 ;;
    esac
done

# Deployment settings come from .env as data (never sourced).
if [ -f "$PROJECT_DIR/.env" ]; then
    MINI_HOST="$(awk -F= '/^MINI_HOST=/{print $2; exit}' "$PROJECT_DIR/.env")"
    MINI_PORT="$(awk -F= '/^MINI_PORT=/{print $2; exit}' "$PROJECT_DIR/.env")"
    MINI_SSH_KEY="$(awk -F= '/^MINI_SSH_KEY=/{print $2; exit}' "$PROJECT_DIR/.env")"
    MINI_JUMP_HOST="$(awk -F= '/^MINI_JUMP_HOST=/{print $2; exit}' "$PROJECT_DIR/.env")"
fi
MINI_HOST="${MINI_HOST:-127.0.0.1}"
MINI_PORT="${MINI_PORT:-2222}"

SSH_OPTS=(-p "$MINI_PORT" -o ConnectTimeout=10 -o BatchMode=yes)
[ -n "${MINI_SSH_KEY:-}" ] && SSH_OPTS+=(-i "$MINI_SSH_KEY")
[ -n "${MINI_JUMP_HOST:-}" ] && SSH_OPTS+=(-J "$MINI_JUMP_HOST")

REMOTE() { ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "$1"; }

PASS=0
FAIL=0
check() {
    local name="$1" result="$2" detail="${3:-}"
    if [ "$result" = "0" ]; then
        PASS=$((PASS + 1))
        printf 'PASS  %s %s\n' "$name" "$detail"
    else
        FAIL=$((FAIL + 1))
        printf 'FAIL  %s %s\n' "$name" "$detail"
    fi
}

echo "== GPT Mini deploy verification ($MINI_HOST:$MINI_PORT) =="

if ! REMOTE "true" >/dev/null 2>&1; then
    echo "FAIL  ssh: cannot reach device"
    exit 1
fi

# 1. Artifact provenance
BUILD_INFO="$(REMOTE "cat $MINI_DEPLOY_DIR/BUILD_INFO 2>/dev/null")"
[ -n "$BUILD_INFO" ]; check "BUILD_INFO present" $? 
REMOTE_COMMIT="$(printf '%s\n' "$BUILD_INFO" | sed -n 's/^commit=//p')"
REMOTE_DIRTY="$(printf '%s\n' "$BUILD_INFO" | sed -n 's/^dirty=//p')"
REMOTE_SHA="$(printf '%s\n' "$BUILD_INFO" | sed -n 's/^binary_sha256=//p')"
LOCAL_COMMIT="$(git -C "$PROJECT_DIR" rev-parse HEAD 2>/dev/null || echo unknown)"

if [ -n "$REMOTE_COMMIT" ] && [ "$REMOTE_COMMIT" = "$LOCAL_COMMIT" ]; then
    check "BUILD_INFO commit == local HEAD" 0 "($REMOTE_COMMIT)"
else
    check "BUILD_INFO commit == local HEAD" 1 "(device=${REMOTE_COMMIT:-none} local=$LOCAL_COMMIT)"
fi
if [ "$REMOTE_DIRTY" = "clean" ]; then
    check "build from clean tree" 0
else
    check "build from clean tree" 1 "(dirty=$REMOTE_DIRTY)"
fi

# 2. Binary hash matches either the build artifact or the explicit expectation
ACTUAL_SHA="$(REMOTE "sha256sum $MINI_DEPLOY_DIR/mini-assistant" | awk '{print $1}')"
LOCAL_ARTIFACT="$PROJECT_DIR/target/$TARGET/release/mini-assistant"
[ -z "$EXPECTED_SHA" ] && [ -f "$LOCAL_ARTIFACT" ] && \
    EXPECTED_SHA="$( (sha256sum "$LOCAL_ARTIFACT" 2>/dev/null || shasum -a 256 "$LOCAL_ARTIFACT") | awk '{print $1}')"
if [ -n "$EXPECTED_SHA" ] && [ "$ACTUAL_SHA" = "$EXPECTED_SHA" ]; then
    check "binary sha256 matches artifact" 0 "($ACTUAL_SHA)"
else
    check "binary sha256 matches artifact" 1 "(device=${ACTUAL_SHA:-none} expected=${EXPECTED_SHA:-unknown})"
fi
REMOTE "test -f $MINI_DEPLOY_DIR/mini-assistant.prev" >/dev/null 2>&1
check "rollback binary present" $?

# 3. Processes: exactly one of each, no duplicates
PROCS="$(REMOTE "ps | grep -v grep")"
count_of() { printf '%s\n' "$PROCS" | grep -c "$1"; }
[ "$(count_of 'opt/mini-assistant/mini-assistant')" -eq 1 ]; check "single mini-assistant" $? \
    "(count=$(count_of 'opt/mini-assistant/mini-assistant'))"
[ "$(count_of 'run-supervisor.sh')" -ge 1 ]; check "supervisor running" $?
[ "$(count_of 'mini_pcm_source')" -ge 1 ]; check "mic capture helper" $?
[ "$(count_of 'soloist-env/soloist')" -ge 1 ]; check "soloist process" $?
[ "$(count_of '/bcm-attach')" -ge 1 ]; check "bcm-attach running" $?

# 4. Spotify Soloist session
STATUS="$(REMOTE "$MINI_DEPLOY_DIR/soloist-ctl status 2>&1")"
printf '%s' "$STATUS" | grep -q "reachable"; check "soloist reachable" $?
printf '%s' "$STATUS" | grep -q "logged in: yes"; check "soloist logged in" $?
printf '%s' "$STATUS" | grep -qE "active device: (yes|no)"; check "soloist device ready" $?

# 5. Hardware sensor + audio device
REMOTE "test \"\$(cat /sys/devices/virtual/input/input0/enable_ps_sensor 2>/dev/null)\" = 1"
check "VL53L0 sensor enabled" $?
REMOTE "grep -q . /proc/asound/card0/pcm0p/sub0/status"; check "ALSA playback PCM exists" $?

# 6. Runtime log markers
LOG_TAIL="$(REMOTE "tail -n 400 $MINI_DEPLOY_DIR/assistant.log 2>/dev/null")"
printf '%s' "$LOG_TAIL" | grep -q "setupComplete confirmed"; check "Gemini Live setupComplete" $?
printf '%s' "$LOG_TAIL" | grep -q "spotify_tools: true"; check "spotify tools registered" $?
printf '%s' "$LOG_TAIL" | grep -q "volume_tools: true"; check "volume tools registered" $?
printf '%s' "$LOG_TAIL" | grep -q "VL53L0 proximity gesture volume reader active"; check "VL53 reader active" $?
printf '%s' "$LOG_TAIL" | grep -q "voice: Callirrhoe"; check "voice Callirrhoe" $?

# 7. No crash loop (filter out intentional SIGTERM exit 143 during service restarts)
CRASHES="$(REMOTE "tail -n 500 $MINI_DEPLOY_DIR/assistant.log 2>/dev/null" | grep -v 'code 143' | grep -v 'code 0' | grep -c 'Daemon exited with code')"
[ "${CRASHES:-0}" -eq 0 ]; check "no daemon crash loop" $? "(unexpected_exits=$CRASHES)"

# 8. Memory headroom (MemAvailable honours page cache)
AVAIL_KB="$(REMOTE "awk '/MemAvailable/{print \$2}' /proc/meminfo")"
[ "${AVAIL_KB:-0}" -gt 40960 ]; check "memory headroom > 40 MB" $? "(available=${AVAIL_KB:-0} kB)"

echo
echo "== summary: PASS=$PASS FAIL=$FAIL =="
[ "$FAIL" -eq 0 ] || exit 1
