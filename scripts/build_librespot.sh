#!/usr/bin/env bash
# Build an upstream Spotify Connect receiver with raw PCM stdout for Mini.
set -euo pipefail

PROJECT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
TARGET="armv7-unknown-linux-musleabihf"
TAG="v0.8.0"
REVISION="d36f9f1907e8cc9d68a93f8ebc6b627b1bf7267d"
BUILD_IMAGE="messense/rust-musl-cross:armv7-musleabihf"
FEATURES="rustls-tls-webpki-roots,with-libmdns"

usage() {
    printf 'Usage: %s [OUTPUT_FILE]\n' "${0##*/}"
}

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
fi
if (( $# > 1 )) || [[ $# == 1 && -z "$1" ]]; then
    usage >&2
    exit 2
fi

output="${1:-$PROJECT_DIR/target/librespot-arm32/librespot}"
if [[ "$output" != /* ]]; then
    output="$(pwd -P)/$output"
fi
[[ ! -d "$output" && ! -L "$output" && "$output" != */ ]] ||
    fail "Output must be a file path: $output"

command -v git >/dev/null || fail "git is required"
command -v docker >/dev/null || fail "docker is required"
if command -v llvm-readelf >/dev/null; then
    readelf_cmd=llvm-readelf
elif command -v readelf >/dev/null; then
    readelf_cmd=readelf
else
    fail "llvm-readelf or readelf is required for ARM32 ELF verification"
fi
if command -v sha256sum >/dev/null; then
    checksum_cmd=(sha256sum)
elif command -v shasum >/dev/null; then
    checksum_cmd=(shasum -a 256)
else
    fail "sha256sum or shasum is required"
fi

mkdir -p "$PROJECT_DIR/target"
work_dir="$(mktemp -d "$PROJECT_DIR/target/librespot-build.XXXXXXXX")"
stage_path=""
cleanup() {
    if [[ -n "$stage_path" ]]; then
        rm -f -- "$stage_path"
    fi
    rm -rf -- "$work_dir"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

checkout="$work_dir/source"
git clone --depth 1 --branch "$TAG" https://github.com/librespot-org/librespot.git "$checkout"
revision="$(git -C "$checkout" rev-parse HEAD)"
[[ "$revision" == "$REVISION" ]] || fail "Unexpected upstream commit for $TAG: $revision"

# Pipe is always built. Keep Rust TLS and mDNS, and leave ALSA to our renderer.
# Bind only the verified upstream source tree, with no Mini config or credentials.
docker run --rm --mount "type=bind,source=$checkout,target=/home/rust/src" \
    --workdir /home/rust/src "$BUILD_IMAGE" \
    cargo build --locked --release --bin librespot --target "$TARGET" \
    --no-default-features --features "$FEATURES"

artifact="$checkout/target/$TARGET/release/librespot"
[[ -s "$artifact" ]] || fail "Build did not produce librespot"
header="$(LC_ALL=C "$readelf_cmd" -W -h "$artifact")"
grep -Eq '^[[:space:]]*Class:[[:space:]]*ELF32$' <<< "$header" &&
    grep -Eq '^[[:space:]]*Data:.*little endian$' <<< "$header" &&
    grep -Eq '^[[:space:]]*Machine:[[:space:]]*ARM$' <<< "$header" ||
    fail "Expected little-endian ARM ELF32"
flags="$(awk '/^[[:space:]]*Flags:/ {sub(/,$/, "", $2); print $2}' <<< "$header")"
# EABI5 with hard-float set and soft-float clear. LLVM and GNU use the same hex flags.
[[ "$flags" =~ ^0x[[:xdigit:]]+$ ]] &&
    (( (flags & 0xff000600) == 0x05000400 )) ||
    fail "Expected ARM EABI5 hard-float flags"
linkage="$(LC_ALL=C "$readelf_cmd" -W -l -d "$artifact")"
if grep -Eq '^[[:space:]]*INTERP([[:space:]]|$)|\(NEEDED\)' <<< "$linkage"; then
    fail "Expected static executable without PT_INTERP or DT_NEEDED"
fi

digest="$("${checksum_cmd[@]}" < "$artifact")"
digest="${digest%% *}"
[[ "$digest" =~ ^[[:xdigit:]]{64}$ ]] || fail "SHA-256 calculation failed"
mkdir -p "$(dirname -- "$output")"
stage_path="$(mktemp "$(dirname -- "$output")/.librespot.XXXXXXXX")"
cp -- "$artifact" "$stage_path"
chmod 0755 "$stage_path"
mv -f -- "$stage_path" "$output"
stage_path=""

printf 'librespot %s (%s)\n%s\nSHA-256 %s\n' "$TAG" "$REVISION" "$output" "$digest"
