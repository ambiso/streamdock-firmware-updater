#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="$PWD/target"

toolchain=1.100.0
linux=x86_64-unknown-linux-musl
windows=x86_64-pc-windows-gnu
macos=aarch64-apple-darwin

rustup toolchain install "$toolchain" --profile minimal \
  --target "$linux" --target "$windows" --target "$macos"
rustup run "$toolchain" cargo zigbuild --release --locked --target "$linux"
rustup run "$toolchain" cargo build --release --locked --target "$windows"
rustup run "$toolchain" cargo zigbuild --release --locked --target "$macos"
"target/$linux/release/akp05-updater" --help

stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
dest="$PWD/dist"
mkdir -p "$dest"
cp LICENSE README.md "$stage/"

install -m755 "target/$linux/release/akp05-updater" "$stage/akp05-updater"
tar -czf "$dest/akp05-updater-linux-x86_64.tar.gz" -C "$stage" akp05-updater LICENSE README.md

install -m755 "target/$macos/release/akp05-updater" "$stage/akp05-updater"
tar -czf "$dest/akp05-updater-macos-arm64.tar.gz" -C "$stage" akp05-updater LICENSE README.md

install -m755 "target/$windows/release/akp05-updater.exe" "$stage/akp05-updater.exe"
(
  cd "$stage"
  zip -q windows.zip akp05-updater.exe LICENSE README.md
)
mv "$stage/windows.zip" "$dest/akp05-updater-windows-x86_64.zip"

cd "$dest"
sha256sum akp05-updater-{linux-x86_64.tar.gz,macos-arm64.tar.gz,windows-x86_64.zip} > SHA256SUMS
