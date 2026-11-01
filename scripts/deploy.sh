#!/bin/bash
# scripts/deploy.sh — Build and deploy mini-assistant to Mini
# Usage: ./scripts/deploy.sh [--host 192.168.x.x] [--key ~/.ssh/key]

set -euo pipefail

# Load .env if exists
[ -f .env ] && source .env

MINI_HOST="${MINI_HOST:-192.168.77.110}"
MINI_PORT="${MINI_PORT:-2222}"
MINI_SSH_KEY="${MINI_SSH_KEY:-~/.ssh/id_ed25519}"
MINI_DEPLOY_DIR="/data/opt/mini-assistant"
TARGET="armv7-unknown-linux-musleabihf"

# Parse args
while [[ $# -gt 0 ]]; do
    case $1 in
        --host) MINI_HOST="$2"; shift 2 ;;
        --key)  MINI_SSH_KEY="$2"; shift 2 ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

SSH_CMD="ssh -p $MINI_PORT -i $MINI_SSH_KEY root@$MINI_HOST"

echo "🔨 Building for ARM32 (musl static)..."
cross build --target $TARGET --release

BINARY="target/$TARGET/release/mini-assistant"
echo "✅ Build done: $(du -sh $BINARY | cut -f1) binary"

echo "📦 Deploying to Mini ($MINI_HOST)..."
$SSH_CMD "mkdir -p $MINI_DEPLOY_DIR"
scp -P $MINI_PORT -i $MINI_SSH_KEY $BINARY root@$MINI_HOST:$MINI_DEPLOY_DIR/
scp -P $MINI_PORT -i $MINI_SSH_KEY config.toml root@$MINI_HOST:$MINI_DEPLOY_DIR/ 2>/dev/null || echo "⚠️  No config.toml found — copy config.example.toml to config.toml first"
scp -P $MINI_PORT -i $MINI_SSH_KEY .env root@$MINI_HOST:$MINI_DEPLOY_DIR/ 2>/dev/null || echo "⚠️  No .env found — copy .env.example to .env first"

echo "🔍 Verifying binary on Mini..."
$SSH_CMD "$MINI_DEPLOY_DIR/mini-assistant --version 2>/dev/null || echo 'Binary deployed (no --version yet)'"
$SSH_CMD "file $MINI_DEPLOY_DIR/mini-assistant"

echo "✅ Deploy complete!"
echo "   Run on Mini: $MINI_DEPLOY_DIR/mini-assistant"
