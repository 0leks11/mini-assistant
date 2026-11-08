#!/bin/bash
# scripts/build.sh — Build for ARM32 (Mini target)
set -euo pipefail

TARGET="armv7-unknown-linux-musleabihf"
PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "🔨 Building mini-assistant for $TARGET using native musl container..."
docker run --rm \
  -v "$PROJECT_DIR":/home/rust/src \
  -v "$HOME"/.cargo/registry:/root/.cargo/registry \
  -v "$HOME"/.cargo/git:/root/.cargo/git \
  messense/rust-musl-cross:armv7-musleabihf \
  cargo build --locked --target "$TARGET" --release

BINARY="$PROJECT_DIR/target/$TARGET/release/mini-assistant"
echo ""
echo "✅ Build successful!"
echo "   Binary: $BINARY"
echo "   Size:   $(du -sh "$BINARY" | cut -f1)"
echo "   Type:   $(file "$BINARY")"
echo ""
echo "Deploy with: ./scripts/deploy.sh"
