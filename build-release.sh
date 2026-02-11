#!/bin/bash
# Build release binaries for distribution
# Run this on the target platform

set -e

VERSION="1.0.2"
PLATFORM=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

# Map architecture names
case $ARCH in
    x86_64) ARCH_NAME="x64" ;;
    aarch64) ARCH_NAME="arm64" ;;
    *) ARCH_NAME=$ARCH ;;
esac

echo "Building EquiForge v${VERSION} for ${PLATFORM}-${ARCH_NAME}..."

cargo build --release

BINARY_NAME="equiforge-${PLATFORM}-${ARCH_NAME}"
cp target/release/equiforge "release/${BINARY_NAME}"

echo "Built: release/${BINARY_NAME}"
echo ""
echo "To create Windows binary, run on Windows:"
echo "  cargo build --release"
echo "  copy target\\release\\equiforge.exe equiforge-windows-x64.exe"
