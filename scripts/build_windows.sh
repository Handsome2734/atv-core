#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "=========================================="
echo "🔨 Building Apple TV Remote for Windows (.exe & .zip)"
echo "=========================================="

cd "$ROOT_DIR"

TARGET="x86_64-pc-windows-gnu"
BUILD_DIR="$ROOT_DIR/build"
OUTPUT_NAME="AppleTVRemote-windows-x86_64.exe"
ZIP_NAME="AppleTVRemote-windows-x86_64.zip"

mkdir -p "$BUILD_DIR"

echo "🦀 Compiling Rust binary (atv-cli) with embedded icon & Windows resources..."
cargo build --release -p atv-cli --target "$TARGET"

SRC_EXE="$ROOT_DIR/target/$TARGET/release/atv-cli.exe"
DEST_EXE="$BUILD_DIR/$OUTPUT_NAME"

cp "$SRC_EXE" "$DEST_EXE"
chmod +x "$DEST_EXE"

echo "📦 Packaging Windows zip..."
(cd "$BUILD_DIR" && rm -f "$ZIP_NAME" && zip -q -9 "$ZIP_NAME" "$OUTPUT_NAME")

echo "=========================================="
echo "✅ Successfully built: $DEST_EXE"
if [ -f "$BUILD_DIR/$ZIP_NAME" ]; then
    echo "📦 Output zip: $BUILD_DIR/$ZIP_NAME"
    echo "📦 File size (zip): $(ls -lh "$BUILD_DIR/$ZIP_NAME" | awk '{print $5}')"
fi
echo "📦 File size (exe): $(ls -lh "$DEST_EXE" | awk '{print $5}')"
echo "=========================================="

