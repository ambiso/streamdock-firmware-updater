![WARN: LLM GENERATED](https://img.shields.io/badge/WARN-LLM%20GENERATED-FF6347)

# MiraBox Stream Dock firmware updater

> [!WARNING]
> **USE AT YOUR OWN RISK.** A failed or incompatible firmware update can make
> the device unusable. Restoring factory firmware has not been tested.

Looks up the latest compatible firmware in MiraBox's public and test
catalogs. It supports catalog devices with the Stream Dock HID interface
and an ArtInChip USB update image (`AIC.FW`). The N4E (`6603:1007`)
upgrade from `V3.293N4E.01.007` to `V3.Mbox_N4E_HJDF.02.018` was tested
on hardware from Linux.

The updater matches the installed firmware family, applies the catalog's
minimum version and exclusions, and skips downgrades. If several models
share the same family, pass the exact catalog name with `--model NAME`.
An entry needs a matching version and a checksum before it can be flashed.

The updater uses HTTPS to download MiraBox firmware and verifies the
catalog checksum. MiraBox currently publishes MD5 for these images. MD5
can detect transfer corruption, while authenticity depends on trusting
MiraBox's HTTPS catalog. The tested N4E image and the downloaded
[ArtInChip flasher][flasher] also have pinned SHA-256 hashes.

## Downloads

- [Linux x86-64][linux]
- [Linux ARM64][linux-arm64] (requires a native flasher)
- [Windows x86-64][windows]
- [Windows ARM64][windows-arm64] (Windows 11)
- [macOS Apple Silicon][macos]
- [SHA-256 checksums][checksums]

Extract the archive for your platform. The updater downloads and
extracts the verified flasher without requiring `curl`, `tar` or `gzip`.
Use `akp05-updater latest --from VERSION` to look up a particular firmware
family without accessing USB. Use `check` in place of `latest` to verify
the downloads and the offline burn plan as well.

## Flash the device

Quit OpenDeck and Stream Dock before flashing. Connect a single dock
directly to the host. Keep the cable connected until the command exits.
The update overwrites the bootloader and firmware. Restoring factory
firmware has not been tested.

### Linux

Both Linux updater release binaries are statically linked. The downloaded
flasher needs glibc 2.39 or newer, libusb 1.0, and libudev
(`libusb-1.0-0` and `libudev1` on Debian and Ubuntu). Run on the host so
the flasher can reopen the device after USB re-enumeration.

```sh
sudo ./akp05-updater version
sudo ./akp05-updater flash
```

With udev access to the dock's hidraw interface and the updater
(`33c3:6677`), run without `sudo`. On NixOS, build with `nix build`
and use `./result/bin/akp05-updater` to get the flasher's runtime
libraries.

On Linux ARM64, download the [flasher source][flasher-source] and build
it on ARM64 with `cargo build --release --bin artinchip-flash`. The
build needs pkg-config and the development packages for libusb and
libudev. Pass the resulting executable to
`flash --flasher /path/to/target/release/artinchip-flash`. The upstream
release currently provides a Linux x86-64 binary.

### Windows

Install the [Microsoft Visual C++ x64 redistributable][vcredist]
if the flasher cannot find `VCRUNTIME140.dll`. The ARM64 updater runs
natively on Windows 11 and uses [emulation][win-arm] for the downloaded
x64 flasher. Windows on ARM needs an ARM64 WinUSB driver.

If the ArtInChip USB updater lacks a WinUSB driver, run
`.\akp05-updater.exe enter-updater`, then use [Zadig][zadig] to install
WinUSB for USB device **`33c3:6677`**. Reconnect the deck to return to
normal mode before flashing.

Read the current version under **Stream Dock → Settings → Device**, then
quit Stream Dock. Supply the exact firmware version you saw and the
dock's USB VID:PID. For example, for the tested N4E:

```powershell
.\akp05-updater.exe flash --from V3.293N4E.01.007 --usb 6603:1007
```

After the dock restarts, confirm the new firmware in Stream Dock.

### macOS

The upstream flasher requires Apple Silicon and Homebrew libusb
(`brew install libusb`). Run from Terminal.

```sh
./akp05-updater version
./akp05-updater flash
```

If macOS cannot read the firmware report, use `flash --from VERSION`
with the exact version from Stream Dock and verify the new version there.

## Build from source

Run `cargo build --release --locked --target-dir target` for a native
build. To create the release archives from x86-64 Linux with Nix
and rustup installed, use the same command as GitHub Actions.

```sh
nix develop .#release -c bash scripts/release.sh
```

CI builds the archives for pushes and pull requests, and publishes the
download links above when a `v*` tag is pushed.
On Linux, the macOS build uses a pinned SDK for the IOKit and Foundation
linker stubs.

The updater is licensed under the [EUPL-1.2](LICENSE).

## AI assistance

Most of this updater's code and documentation was written with AI
assistance. The project is experimental and has not had an independent
security audit. Hardware testing covered the MiraBox N4E on Linux.

[flasher]: https://github.com/boa-z/artinchip-flash/releases/tag/v0.2.0
[flasher-source]: https://github.com/boa-z/artinchip-flash/archive/refs/tags/v0.2.0.tar.gz
[linux]: https://github.com/ambiso/akp05-updater/releases/latest/download/akp05-updater-linux-x86_64.tar.gz
[linux-arm64]: https://github.com/ambiso/akp05-updater/releases/latest/download/akp05-updater-linux-arm64.tar.gz
[windows]: https://github.com/ambiso/akp05-updater/releases/latest/download/akp05-updater-windows-x86_64.zip
[windows-arm64]: https://github.com/ambiso/akp05-updater/releases/latest/download/akp05-updater-windows-arm64.zip
[macos]: https://github.com/ambiso/akp05-updater/releases/latest/download/akp05-updater-macos-arm64.tar.gz
[checksums]: https://github.com/ambiso/akp05-updater/releases/latest/download/SHA256SUMS
[zadig]: https://zadig.akeo.ie/
[vcredist]: https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist
[win-arm]: https://learn.microsoft.com/en-us/windows/arm/apps-on-arm-x86-emulation
