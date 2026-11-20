#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="$PWD/target"

toolchain=1.100.0
binary=streamdock-fw
linux=x86_64-unknown-linux-musl
linux_arm=aarch64-unknown-linux-musl
windows=x86_64-pc-windows-gnu
windows_arm=aarch64-pc-windows-gnullvm
macos=aarch64-apple-darwin

rustup toolchain install "$toolchain" --profile minimal \
  --target "$linux" --target "$linux_arm" --target "$windows" --target "$windows_arm" --target "$macos"
rustup run "$toolchain" cargo zigbuild --release --locked --target "$linux"
rustup run "$toolchain" cargo zigbuild --release --locked --target "$linux_arm"
rustup run "$toolchain" cargo build --release --locked --target "$windows"
rustup run "$toolchain" cargo zigbuild --release --locked --target "$windows_arm"
rustup run "$toolchain" cargo zigbuild --release --locked --target "$macos"
for target in "$linux" "$linux_arm"; do
  headers=$(readelf -l -d "target/$target/release/$binary")
  if [[ $headers == *INTERP* || $headers == *NEEDED* ]]; then
    echo "Linux release for $target must be statically linked." >&2
    exit 1
  fi
done
"target/$linux/release/$binary" --help

dest="$PWD/dist"
mkdir -p "$dest"
assets=()

install_binary() {
  local target=$1 platform=$2 extension=${3:-}
  local filename="$binary-$platform$extension"
  install -m755 "target/$target/release/$binary$extension" "$dest/$filename"
  assets+=("$filename")
}

install_binary "$linux" linux-x86_64
install_binary "$linux_arm" linux-arm64
install_binary "$windows" windows-x86_64 .exe
install_binary "$windows_arm" windows-arm64 .exe
install_binary "$macos" macos-arm64

cd "$dest"
sha256sum "${assets[@]}" > SHA256SUMS
