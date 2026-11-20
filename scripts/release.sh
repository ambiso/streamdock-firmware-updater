#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="$PWD/target"

toolchain=1.100.0
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
  headers=$(readelf -l -d "target/$target/release/akp05-updater")
  if [[ $headers == *INTERP* || $headers == *NEEDED* ]]; then
    echo "Linux release for $target must be statically linked." >&2
    exit 1
  fi
done
"target/$linux/release/akp05-updater" --help

stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
dest="$PWD/dist"
mkdir -p "$dest"
cp LICENSE README.md "$stage/"

install -m755 "target/$linux/release/akp05-updater" "$stage/akp05-updater"
tar -czf "$dest/akp05-updater-linux-x86_64.tar.gz" -C "$stage" akp05-updater LICENSE README.md

install -m755 "target/$linux_arm/release/akp05-updater" "$stage/akp05-updater"
tar -czf "$dest/akp05-updater-linux-arm64.tar.gz" -C "$stage" akp05-updater LICENSE README.md

install -m755 "target/$macos/release/akp05-updater" "$stage/akp05-updater"
tar -czf "$dest/akp05-updater-macos-arm64.tar.gz" -C "$stage" akp05-updater LICENSE README.md

for target in "$windows" "$windows_arm"; do
  architecture=${target%%-*}
  architecture=${architecture/aarch64/arm64}
  install -m755 "target/$target/release/akp05-updater.exe" "$stage/akp05-updater.exe"
  (
    cd "$stage"
    zip -q windows.zip akp05-updater.exe LICENSE README.md
  )
  mv "$stage/windows.zip" "$dest/akp05-updater-windows-$architecture.zip"
done

cd "$dest"
sha256sum akp05-updater-{linux-{x86_64,arm64}.tar.gz,macos-arm64.tar.gz,windows-{x86_64,arm64}.zip} > SHA256SUMS
