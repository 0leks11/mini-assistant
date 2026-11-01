#!/bin/bash
# scripts/deploy.sh — Build and deploy mini-assistant to Mini
# Usage: ./scripts/deploy.sh [--host 192.168.x.x] [--key ~/.ssh/key] [--jump user@pi]

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Load .env if exists
[ -f "$PROJECT_DIR/.env" ] && source "$PROJECT_DIR/.env"

MINI_HOST="${MINI_HOST:-192.168.77.110}"
MINI_PORT="${MINI_PORT:-2222}"
MINI_SSH_KEY="${MINI_SSH_KEY:-}"
MINI_JUMP_HOST="${MINI_JUMP_HOST:-}"
MINI_DEPLOY_DIR="/data/opt/mini-assistant"
TARGET="armv7-unknown-linux-musleabihf"

# Parse args
while [[ $# -gt 0 ]]; do
    case $1 in
        --host) MINI_HOST="$2"; shift 2 ;;
        --key)  MINI_SSH_KEY="$2"; shift 2 ;;
        --jump) MINI_JUMP_HOST="$2"; shift 2 ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

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

echo "🔨 Building for ARM32 (musl static)..."
docker run --rm \
  -v "$PROJECT_DIR":/home/rust/src \
  -v "$HOME"/.cargo/registry:/root/.cargo/registry \
  -v "$HOME"/.cargo/git:/root/.cargo/git \
  messense/rust-musl-cross:armv7-musleabihf \
  cargo build --target "$TARGET" --release

BINARY="$PROJECT_DIR/target/$TARGET/release/mini-assistant"
echo "✅ Build done: $(du -sh "$BINARY" | cut -f1) binary"

echo "📦 Deploying to Mini ($MINI_HOST)..."
ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "mkdir -p $MINI_DEPLOY_DIR"
scp "${SCP_OPTS[@]}" "$BINARY" "root@$MINI_HOST:$MINI_DEPLOY_DIR/"
scp "${SCP_OPTS[@]}" "$PROJECT_DIR/config.toml" "root@$MINI_HOST:$MINI_DEPLOY_DIR/" 2>/dev/null || echo "⚠️  No config.toml found"
scp "${SCP_OPTS[@]}" "$PROJECT_DIR/.env" "root@$MINI_HOST:$MINI_DEPLOY_DIR/" 2>/dev/null || echo "⚠️  No .env found"

echo "🔍 Verifying binary on Mini..."
ssh "${SSH_OPTS[@]}" "root@$MINI_HOST" "file $MINI_DEPLOY_DIR/mini-assistant"

echo "✅ Deploy complete!"
echo "   Run on Mini: $MINI_DEPLOY_DIR/mini-assistant"
