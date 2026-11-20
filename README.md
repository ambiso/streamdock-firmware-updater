# MiraBox N4E firmware updater

Updates a MiraBox N4E (USB `6603:1007`) to `V3.Mbox_N4E_HJDF.02.018`.
The upgrade from `V3.293N4E.01.007` was tested on hardware from Linux. The
updater accepts the `V3.293N4E` firmware family from `00.004` through
`02.017`, based on MiraBox's update catalog.

The program downloads the firmware from MiraBox's CDN and a native
CLI from the [upstream ArtInChip flasher release][flasher]. It verifies both
against SHA-256 hashes pinned in the source before using either file.
Downloads are kept in a temporary directory for that run.

## Downloads

- [Linux x86-64][linux]
- [Windows x86-64][windows]
- [macOS Apple Silicon][macos]
- [SHA-256 checksums][checksums]

Extract the archive. The updater uses `curl` and `tar` from the host.
Run `akp05-updater check` to check the downloads and the offline burn
plan, even if the deck is disconnected.

## Flash the device

Quit OpenDeck and Stream Dock before flashing. Connect a single N4E
directly to the host. Keep the cable connected until the command exits.
The update overwrites the bootloader and firmware. Restoring factory
firmware has not been tested.

### Linux

The downloaded flasher needs glibc 2.39 or newer, libusb 1.0, and
libudev (`libusb-1.0-0` and `libudev1` on Debian and Ubuntu). Run on the
host so the flasher can reopen the device after USB re-enumeration.

```sh
sudo ./akp05-updater version
sudo ./akp05-updater flash
```

With udev access to the N4E hidraw interface and the updater
(`33c3:6677`), run without `sudo`. On NixOS, build with `nix build`
and use `./result/bin/akp05-updater` to get the flasher's runtime
libraries.

### Windows

Install the [Microsoft Visual C++ x64 redistributable][vcredist]
if the flasher cannot find `VCRUNTIME140.dll`. Use the inbox `curl.exe`
and `tar.exe` from a recent Windows installation.

If the ArtInChip USB updater lacks a WinUSB driver, run
`.akp05-updater.exe enter-updater`, then use [Zadig][zadig] to install
WinUSB for USB device **`33c3:6677`**. Reconnect the deck to return to
normal mode before flashing.

Read the current version under **Stream Dock → Settings → Device**, then
quit Stream Dock. Windows HID does not expose the N4E firmware report,
so supply the exact version you saw.

```powershell
.\akp05-updater.exe flash --from V3.293N4E.01.007
```

After the N4E restarts, confirm `V3.Mbox_N4E_HJDF.02.018` in Stream Dock.

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
build. To create all three release archives from x86-64 Linux with Nix
and rustup installed, use the same command as GitHub Actions.

```sh
nix develop .#release -c bash scripts/release.sh
```

CI builds the archives for pushes and pull requests, and publishes the
download links above when a `v*` tag is pushed.
On Linux, the macOS build uses a pinned SDK for the IOKit and Foundation
linker stubs.

The updater is licensed under the [EUPL-1.2](LICENSE).

[flasher]: https://github.com/boa-z/artinchip-flash/releases/tag/v0.2.0
[linux]: https://github.com/ambiso/akp05-updater/releases/latest/download/akp05-updater-linux-x86_64.tar.gz
[windows]: https://github.com/ambiso/akp05-updater/releases/latest/download/akp05-updater-windows-x86_64.zip
[macos]: https://github.com/ambiso/akp05-updater/releases/latest/download/akp05-updater-macos-arm64.tar.gz
[checksums]: https://github.com/ambiso/akp05-updater/releases/latest/download/SHA256SUMS
[zadig]: https://zadig.akeo.ie/
[vcredist]: https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist
