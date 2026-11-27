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

# 3. Processes: exactly one of each, no duplicates, and active exe not deleted
PROCS="$(REMOTE "ps | grep -v grep")"
count_of() { printf '%s\n' "$PROCS" | grep -c "$1"; }
[ "$(count_of 'opt/mini-assistant/mini-assistant')" -eq 1 ]; check "single mini-assistant" $? \
    "(count=$(count_of 'opt/mini-assistant/mini-assistant'))"
LIVE_EXE_CHECK="$(REMOTE "for p in \$(pidof mini-assistant 2>/dev/null); do ls -l /proc/\$p/exe 2>/dev/null; done")"
if [ -n "$LIVE_EXE_CHECK" ] && ! grep -q '(deleted)' <<< "$LIVE_EXE_CHECK"; then
    check "running process uses current binary (not deleted)" 0
else
    check "running process uses current binary (not deleted)" 1 "(${LIVE_EXE_CHECK:-no_pid})"
fi
[ "$(count_of 'run-supervisor.sh')" -ge 1 ]; check "supervisor running" $?
[ "$(count_of 'mini_pcm_source')" -ge 1 ]; check "mic capture helper" $?
[ "$(count_of 'soloist-env/soloist')" -ge 1 ]; check "soloist process" $?
[ "$(count_of '/bcm-attach')" -ge 1 ]; check "bcm-attach running" $?

# 4. Spotify Soloist session
STATUS="$(REMOTE "$MINI_DEPLOY_DIR/soloist-ctl status 2>&1")"
grep -q "reachable" <<< "$STATUS"; check "soloist reachable" $?
grep -qE "logged in: (yes|no)" <<< "$STATUS"; check "soloist session queryable" $?
if grep -q "logged in: yes" <<< "$STATUS"; then
    grep -qE "active device: (yes|no)" <<< "$STATUS"; check "soloist device ready" $?
fi

# 5. Hardware sensor + audio device
REMOTE "test \"\$(cat /sys/devices/virtual/input/input0/enable_ps_sensor 2>/dev/null)\" = 1"
check "VL53L0 sensor enabled" $?
REMOTE "grep -q . /proc/asound/card0/pcm0p/sub0/status"; check "ALSA playback PCM exists" $?

# 6. Runtime log markers
LOG_TAIL="$(REMOTE "tail -n 2000 $MINI_DEPLOY_DIR/assistant.log 2>/dev/null")"
EXPECTED_PROVIDER="$(REMOTE "grep -E '^\s*provider\s*=' $MINI_DEPLOY_DIR/config.toml 2>/dev/null | head -n1 | cut -d'\"' -f2 | tr '[:upper:]' '[:lower:]' | tr -d ' ' || echo gemini")"
[ -z "$EXPECTED_PROVIDER" ] && EXPECTED_PROVIDER="gemini"
if [ "$EXPECTED_PROVIDER" = "openai" ]; then
    grep -q "OpenAI Realtime setup handshake completed successfully" <<< "$LOG_TAIL"; check "OpenAI Realtime setup handshake" $?
else
    grep -q "setupComplete confirmed" <<< "$LOG_TAIL"; check "Gemini Live setupComplete" $?
fi
grep -q "spotify_tools: true" <<< "$LOG_TAIL"; check "spotify tools registered" $?
grep -q "volume_tools: true" <<< "$LOG_TAIL"; check "volume tools registered" $?
grep -q "VL53L0 proximity gesture volume reader active" <<< "$LOG_TAIL"; check "VL53 reader active" $?
EXPECTED_VOICE="$(REMOTE "grep -E '^\s*voice\s*=' $MINI_DEPLOY_DIR/config.toml 2>/dev/null | cut -d'\"' -f2 | tr -d ' ' || echo Callirrhoe")"
[ -z "$EXPECTED_VOICE" ] && EXPECTED_VOICE="Callirrhoe"
grep -q "voice: $EXPECTED_VOICE" <<< "$LOG_TAIL"; check "voice $EXPECTED_VOICE" $?

# 7. No crash loop (filter out intentional SIGTERM/SIGKILL exit 143/137 during service restarts)
CRASHES="$(REMOTE "tail -n 500 $MINI_DEPLOY_DIR/assistant.log 2>/dev/null" | grep -v 'code 143' | grep -v 'code 137' | grep -v 'code 0' | grep -c 'Daemon exited with code')"
[ "${CRASHES:-0}" -eq 0 ]; check "no daemon crash loop" $? "(unexpected_exits=$CRASHES)"

# 8. Memory headroom (MemAvailable honours page cache)
AVAIL_KB="$(REMOTE "awk '/MemAvailable/{print \$2}' /proc/meminfo")"
[ "${AVAIL_KB:-0}" -gt 40960 ]; check "memory headroom > 40 MB" $? "(available=${AVAIL_KB:-0} kB)"

echo
echo "== summary: PASS=$PASS FAIL=$FAIL =="
[ "$FAIL" -eq 0 ] || exit 1
