#!/bin/bash
# scripts/deploy.sh — Build and deploy mini-assistant to Mini
# Usage: ./scripts/deploy.sh [--host HOST] [--port PORT] [--key PATH] [--jump user@pi]

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

MINI_DEPLOY_DIR="/data/opt/mini-assistant"
TARGET="armv7-unknown-linux-musleabihf"

usage() {
    echo "Usage: $0 [--host HOST] [--port PORT] [--key PATH] [--jump user@pi] [--skip-build]"
}

require_value() {
    if [ "$#" -lt 2 ] || [ -z "$2" ] || [[ "$2" == --* ]]; then
        echo "ERROR: $1 requires a value" >&2
        exit 2
    fi
}

# Parse arguments before reading any configuration; --help has no side effects.
SKIP_BUILD=0
while [[ $# -gt 0 ]]; do
    case $1 in
        --host) require_value "$@"; CLI_HOST="$2"; shift 2 ;;
        --port) require_value "$@"; CLI_PORT="$2"; shift 2 ;;
        --key)  require_value "$@"; CLI_KEY="$2"; shift 2 ;;
        --jump) require_value "$@"; CLI_JUMP="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "ERROR: unknown argument" >&2; usage >&2; exit 2 ;;
    esac
done

# .env is data, never a shell script. Load only deployment settings and do not
# evaluate shell interpolation or load service API credentials into this shell.
if [ -f "$PROJECT_DIR/.env" ]; then
    DEPLOY_VARS="$(mktemp "${TMPDIR:-/tmp}/mini-deploy-vars.XXXXXX")"
    trap 'rm -f "$DEPLOY_VARS"' EXIT
    python3 - "$PROJECT_DIR/.env" > "$DEPLOY_VARS" <<'PY'
import re
import shlex
import sys
from pathlib import Path

assignment = re.compile(r"^\s*(?:export\s+)?(MINI_HOST|MINI_PORT|MINI_SSH_KEY|MINI_JUMP_HOST)\s*=(.*)$")
try:
    lines = Path(sys.argv[1]).read_text().splitlines()
    for line in lines:
        match = assignment.match(line)
        if not match:
            continue
        name, value = match.groups()
        value = value.strip()
        if value.startswith(("'", '"')):
            lexer = shlex.shlex(value, posix=True)
            lexer.whitespace_split = True
            lexer.commenters = "#"
            tokens = list(lexer)
            if len(tokens) != 1:
                raise ValueError("expected one quoted value")
            value = tokens[0]
        else:
            value = re.split(r"\s+#", value, maxsplit=1)[0].rstrip()
        if "\0" in value or "\n" in value or "\r" in value:
            raise ValueError("invalid deployment value")
        sys.stdout.buffer.write(name.encode() + b"\0" + value.encode() + b"\0")
except (OSError, UnicodeError, ValueError):
    # Do not echo the offending line: it may contain credentials.
    raise SystemExit("ERROR: cannot parse deployment settings in .env")
PY
    while IFS= read -r -d '' name && IFS= read -r -d '' value; do
        printf -v "$name" '%s' "$value"
    done < "$DEPLOY_VARS"
    rm -f "$DEPLOY_VARS"
    trap - EXIT
fi

MINI_HOST="${CLI_HOST:-${MINI_HOST:-192.168.77.110}}"
MINI_PORT="${CLI_PORT:-${MINI_PORT:-2222}}"
MINI_SSH_KEY="${CLI_KEY:-${MINI_SSH_KEY:-}}"
MINI_JUMP_HOST="${CLI_JUMP:-${MINI_JUMP_HOST:-}}"

if [[ ! "$MINI_HOST" =~ ^[a-zA-Z0-9_.:-]+$ ]] || [[ "$MINI_HOST" == -* ]]; then
    echo "ERROR: invalid Mini host" >&2
    exit 2
fi
if [[ ! "$MINI_PORT" =~ ^[0-9]{1,5}$ ]] || (( 10#$MINI_PORT < 1 || 10#$MINI_PORT > 65535 )); then
    echo "ERROR: Mini port must be between 1 and 65535" >&2
    exit 2
fi
JUMP_HOST_PATTERN='^[][a-zA-Z0-9_.@,:-]+$'
if [ -n "$MINI_JUMP_HOST" ] && { [[ ! "$MINI_JUMP_HOST" =~ $JUMP_HOST_PATTERN ]] || [[ "$MINI_JUMP_HOST" == -* ]]; }; then
    echo "ERROR: invalid jump host" >&2
    exit 2
fi
if [[ "$MINI_SSH_KEY" == '~/'* ]]; then
    MINI_SSH_KEY="$HOME/${MINI_SSH_KEY:2}"
fi
if [ -n "$MINI_SSH_KEY" ] && [ ! -f "$MINI_SSH_KEY" ]; then
    echo "ERROR: Mini SSH key file does not exist" >&2
    exit 2
fi

SSH_OPTS=("-p" "$MINI_PORT")
SCP_OPTS=("-P" "$MINI_PORT")

if [ -n "$MINI_SSH_KEY" ]; then
    SSH_OPTS+=("-i" "$MINI_SSH_KEY")
    SCP_OPTS+=("-i" "$MINI_SSH_KEY")
fi

if [ -n "$MINI_JUMP_HOST" ]; then
    SSH_OPTS+=("-J" "$MINI_JUMP_HOST")
    SCP_OPTS+=("-o" "ProxyJump=$MINI_JUMP_HOST")
fi

if [ "$SKIP_BUILD" -ne 1 ]; then
    echo "🔨 Building for ARM32 (musl static)..."
    docker run --rm \
      -v "$PROJECT_DIR":/home/rust/src \
      -v "$HOME"/.cargo/registry:/root/.cargo/registry \
      -v "$HOME"/.cargo/git:/root/.cargo/git \
      messense/rust-musl-cross:armv7-musleabihf \
      cargo build --locked --target "$TARGET" --release
else
    echo "⏭️  Skipping build step (--skip-build requested)"
fi

BINARY="$PROJECT_DIR/target/$TARGET/release/mini-assistant"
echo "✅ Build done: $(du -sh "$BINARY" | cut -f1) binary"

echo "📦 Deploying to Mini ($MINI_HOST)..."
ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "umask 077; mkdir -p $MINI_DEPLOY_DIR && chmod 700 $MINI_DEPLOY_DIR"

USE_SCP=1
if ! ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "which scp >/dev/null 2>&1"; then
    USE_SCP=0
fi

upload_file() {
    local src="$1"
    local dst="$2"
    if [ "$USE_SCP" -eq 1 ]; then
        scp "${SCP_OPTS[@]}" "$src" "root@$MINI_HOST:$dst"
    elif ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "which gunzip >/dev/null 2>&1"; then
        gzip -c "$src" | ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "gunzip > '$dst'"
    else
        cat "$src" | ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "cat > $dst"
    fi
}

upload_file "$BINARY" "$MINI_DEPLOY_DIR/mini-assistant.new"

LOCAL_HASH="$(sha256sum "$BINARY" 2>/dev/null | cut -d' ' -f1 || shasum -a 256 "$BINARY" | cut -d' ' -f1)"
REMOTE_OUT="$(ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "sha256sum $MINI_DEPLOY_DIR/mini-assistant.new 2>/dev/null || true")"
REMOTE_HASH="$(echo "$REMOTE_OUT" | awk '{print $1}')"

if [ -z "$REMOTE_HASH" ] || [ "$REMOTE_HASH" != "$LOCAL_HASH" ]; then
    echo "ERROR: binary SHA256 mismatch or empty remote hash (local: $LOCAL_HASH, remote: $REMOTE_HASH)" >&2
    exit 1
fi

# Deliver native playback helper (mini_pcm_sink) if present (R6)
HELPER_CANDIDATES=(
    "$PROJECT_DIR/target/$TARGET/release/mini_pcm_sink"
    "$PROJECT_DIR/mini_pcm_sink"
    "$PROJECT_DIR/../.private/alice-mini/pcm-stream-2026-10-30/mini_pcm_sink"
)
HAVE_HELPER=0
HELPER_PATH=""
for cand in "${HELPER_CANDIDATES[@]}"; do
    if [ -f "$cand" ]; then
        echo "📦 Found native playback helper: $cand"
        upload_file "$cand" "$MINI_DEPLOY_DIR/mini_pcm_sink.new"
        HAVE_HELPER=1
        HELPER_PATH="$cand"
        break
    fi
done

if [ "$HAVE_HELPER" -eq 1 ]; then
    LOCAL_HELPER_HASH="$(sha256sum "$HELPER_PATH" 2>/dev/null | cut -d' ' -f1 || shasum -a 256 "$HELPER_PATH" | cut -d' ' -f1)"
    REMOTE_HELPER_OUT="$(ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "sha256sum $MINI_DEPLOY_DIR/mini_pcm_sink.new 2>/dev/null || true")"
    REMOTE_HELPER_HASH="$(echo "$REMOTE_HELPER_OUT" | awk '{print $1}')"
    if [ -z "$REMOTE_HELPER_HASH" ] || [ "$REMOTE_HELPER_HASH" != "$LOCAL_HELPER_HASH" ]; then
        echo "ERROR: helper SHA256 mismatch or empty remote hash (local: $LOCAL_HELPER_HASH, remote: $REMOTE_HELPER_HASH)" >&2
        exit 1
    fi
fi

# Deliver native capture helper (mini_pcm_source) if present (Milestone H1)
SOURCE_CANDIDATES=(
    "$PROJECT_DIR/target/$TARGET/release/mini_pcm_source"
    "$PROJECT_DIR/mini_pcm_source"
    "$PROJECT_DIR/../.private/alice-mini/pcm-stream-2026-10-30/mini_pcm_source"
    "$PROJECT_DIR/../research/alice-mini-2019/lab/tools/mini_pcm_source"
)
HAVE_SOURCE=0
SOURCE_PATH=""
for cand in "${SOURCE_CANDIDATES[@]}"; do
    if [ -f "$cand" ]; then
        echo "📦 Found native capture helper: $cand"
        upload_file "$cand" "$MINI_DEPLOY_DIR/mini_pcm_source.new"
        HAVE_SOURCE=1
        SOURCE_PATH="$cand"
        break
    fi
done

if [ "$HAVE_SOURCE" -eq 1 ]; then
    LOCAL_SOURCE_HASH="$(sha256sum "$SOURCE_PATH" 2>/dev/null | cut -d' ' -f1 || shasum -a 256 "$SOURCE_PATH" | cut -d' ' -f1)"
    REMOTE_SOURCE_OUT="$(ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "sha256sum $MINI_DEPLOY_DIR/mini_pcm_source.new 2>/dev/null || true")"
    REMOTE_SOURCE_HASH="$(echo "$REMOTE_SOURCE_OUT" | awk '{print $1}')"
    if [ -z "$REMOTE_SOURCE_HASH" ] || [ "$REMOTE_SOURCE_HASH" != "$LOCAL_SOURCE_HASH" ]; then
        echo "ERROR: capture helper SHA256 mismatch or empty remote hash (local: $LOCAL_SOURCE_HASH, remote: $REMOTE_SOURCE_HASH)" >&2
        exit 1
    fi
fi

# Deliver bcm-attach helper if present
BCM_ATTACH_BIN="$PROJECT_DIR/target/$TARGET/release/bcm-attach"
HAVE_BCM_ATTACH=0
if [ -f "$BCM_ATTACH_BIN" ]; then
    echo "📦 Found bcm-attach binary: $BCM_ATTACH_BIN"
    upload_file "$BCM_ATTACH_BIN" "$MINI_DEPLOY_DIR/bcm-attach.new"
    HAVE_BCM_ATTACH=1
fi

# All binaries staged and verified; atomically commit binary bundle
ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "mv -f $MINI_DEPLOY_DIR/mini-assistant.new $MINI_DEPLOY_DIR/mini-assistant && chmod 755 $MINI_DEPLOY_DIR/mini-assistant"

if [ "$HAVE_BCM_ATTACH" -eq 1 ]; then
    ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "mv -f $MINI_DEPLOY_DIR/bcm-attach.new $MINI_DEPLOY_DIR/bcm-attach && chmod 755 $MINI_DEPLOY_DIR/bcm-attach"
fi

if [ "$HAVE_HELPER" -eq 1 ]; then
    ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "mv -f $MINI_DEPLOY_DIR/mini_pcm_sink.new $MINI_DEPLOY_DIR/mini_pcm_sink && chmod 755 $MINI_DEPLOY_DIR/mini_pcm_sink"
fi

if [ "$HAVE_SOURCE" -eq 1 ]; then
    ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "mv -f $MINI_DEPLOY_DIR/mini_pcm_source.new $MINI_DEPLOY_DIR/mini_pcm_source && chmod 755 $MINI_DEPLOY_DIR/mini_pcm_source"
fi

# Deploy config only if remote does not have it, or with explicit intent
REMOTE_HAS_CONFIG="$(ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "[ -f $MINI_DEPLOY_DIR/config.toml ] && echo 1 || echo 0")"
if [ "$REMOTE_HAS_CONFIG" -eq 0 ] && [ -f "$PROJECT_DIR/config.toml" ]; then
    echo "📋 Staging initial config.toml on device..."
    upload_file "$PROJECT_DIR/config.toml" "$MINI_DEPLOY_DIR/config.toml"
else
    echo "ℹ️  Remote config.toml already exists on device; preserving device settings (voice, etc.)"
fi

# NEVER blindly overwrite remote .env with local template/stale settings
REMOTE_HAS_ENV="$(ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "[ -f $MINI_DEPLOY_DIR/.env ] && echo 1 || echo 0")"
if [ "$REMOTE_HAS_ENV" -eq 0 ] && [ -f "$PROJECT_DIR/.env" ]; then
    echo "🔒 Staging initial .env on device..."
    upload_file "$PROJECT_DIR/.env" "$MINI_DEPLOY_DIR/.env"
    ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "chmod 600 $MINI_DEPLOY_DIR/.env"
else
    echo "ℹ️  Remote .env already exists on device; preserving calibrated credentials and thresholds"
fi

echo "🔍 Verifying binary on Mini..."
ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "ls -lh $MINI_DEPLOY_DIR/mini-assistant && sha256sum $MINI_DEPLOY_DIR/mini-assistant"

echo "✅ Deploy complete!"
echo "   Run on Mini: $MINI_DEPLOY_DIR/mini-assistant"
