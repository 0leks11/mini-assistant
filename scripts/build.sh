#!/bin/bash
# scripts/build.sh — Build for ARM32 (Mini target)
set -euo pipefail

TARGET="armv7-unknown-linux-musleabihf"

echo "🔨 Building mini-assistant for $TARGET..."
cross build --target $TARGET --release

BINARY="target/$TARGET/release/mini-assistant"
echo ""
echo "✅ Build successful!"
echo "   Binary: $BINARY"
echo "   Size:   $(du -sh $BINARY | cut -f1)"
echo "   Type:   $(file $BINARY)"
echo ""
echo "Deploy with: ./scripts/deploy.sh"
