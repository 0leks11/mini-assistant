#!/bin/bash
# scripts/build_librespot.sh — Build librespot (Spotify Connect) for Mini ARM32
set -euo pipefail

TARGET="armv7-unknown-linux-musleabihf"
WORK_DIR="/tmp/librespot-build"

echo "🎵 Building librespot for $TARGET..."

if [ ! -d "$WORK_DIR" ]; then
    echo "Cloning librespot..."
    git clone --depth 1 https://github.com/librespot-org/librespot.git "$WORK_DIR"
fi

cd "$WORK_DIR"

echo "Compiling with cross..."
cross build --target "$TARGET" --release --no-default-features --features "alsa-backend"

BIN_PATH="$WORK_DIR/target/$TARGET/release/librespot"
echo "✅ librespot built successfully!"
echo "   Binary: $BIN_PATH"
echo "   Size:   $(du -sh "$BIN_PATH" | cut -f1)"

# Copy to artifacts directory
mkdir -p "$OLDPWD/bin"
cp "$BIN_PATH" "$OLDPWD/bin/librespot"
echo "   Copied to: $OLDPWD/bin/librespot"
