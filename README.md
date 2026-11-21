![WARN: LLM GENERATED](https://img.shields.io/badge/WARN-LLM%20GENERATED-FF6347)

# Stream Dock Firmware Updater

> [!WARNING]
> **USE AT YOUR OWN RISK.** A failed or incompatible firmware update can make
> the device unusable. Restoring factory firmware has not been tested.

Firmware updater for compatible MiraBox Stream Dock devices. It looks up
the latest firmware in MiraBox's public and test catalogs and supports
catalog devices with the Stream Dock HID interface
and an ArtInChip USB update image (`AIC.FW`). The N4E (`6603:1007`)
upgrade from `V3.293N4E.01.007` to `V3.Mbox_N4E_HJDF.02.018` was tested
on hardware from Linux.

Catalog upgrades match the installed firmware family, apply the catalog's
minimum version and exclusions, and skip downgrades. If several models
share the same family, pass the exact catalog name with `--model NAME`.
An entry needs a matching version and a checksum before it can be flashed.

The updater uses HTTPS to download MiraBox firmware and verifies the
catalog checksum. MiraBox currently publishes MD5 for these images. MD5
can detect transfer corruption, while authenticity depends on trusting
MiraBox's HTTPS catalog. The tested N4E image also has a pinned SHA-256
hash. Bulk flashing uses a pinned revision of the [ArtInChip SDK][sdk].

## Downloads

Download the executable for your platform from [GitHub Releases][releases].
The release also provides `SHA256SUMS` for verifying the download.

| Platform | Download |
| --- | --- |
| Linux x86-64 | [streamdock-fw-linux-x86_64][linux-x86-64] |
| Linux ARM64 | [streamdock-fw-linux-arm64][linux-arm64] |
| Windows x86-64 | [streamdock-fw-windows-x86_64.exe][windows-x86-64] |
| Windows ARM64 (Windows 11) | [streamdock-fw-windows-arm64.exe][windows-arm64] |
| macOS Apple Silicon | [streamdock-fw-macos-arm64][macos-arm64] |

The v0.1.0 release uses the original `akp05-updater` name.

## Terminal interface

On Windows, double-click the downloaded `.exe` to open the interface.
On Linux and macOS, open a terminal, mark the downloaded file executable
with `chmod +x`, and run it without arguments. The examples below use
`streamdock-fw` as the executable name.

The picker shows each dock's installed version and whether an update is
available. Select a dock with the arrow keys and press Enter to prepare
its upgrade. Confirm **Update** only when you're ready to leave the dock
connected through flashing and reboot. Press Esc to return to the picker
or L from the picker to view the embedded EUPL-1.2 license.

Press R on either screen to refresh the dock list, firmware reads, and
catalogs. Use Ctrl+R while entering a firmware version.

Use `--help` for command-line options and `streamdock-fw devices` to list
connected docks. `streamdock-fw latest --from VERSION`
looks up a firmware family without accessing USB. Use `check` in place of
`latest` to verify the firmware download and inspect the offline burn
plan. The release executable includes the flasher.

## Flash the device

Quit OpenDeck and Stream Dock before flashing. Connect a single dock
directly to the host. Keep the cable connected while it is flashing.
The bulk USB updater overwrites the bootloader and firmware. The N4E HID
updater writes the firmware and resources, preserving the bootloader,
settings, and data. Restoring factory firmware has not been tested.

### Linux

Both Linux updater release binaries are statically linked. Make sure the
updater can access the dock after it re-enumerates in firmware mode.

```sh
sudo ./streamdock-fw version
sudo ./streamdock-fw flash
```

With udev access to the dock's hidraw interface and its updater, run
without `sudo`. The bulk updater uses USB device `33c3:6677`.
N4E firmware with the HID updater requires hidraw access to `33c3:8899`.
On NixOS, use `nix run .` to open the updater from a checkout.

To install a local vendor N4E image through the HID updater on Linux,
including an older release, first validate the file and then run the
same command without `--dry-run`:

```sh
streamdock-fw flash --file /path/to/firmware.img --dry-run
streamdock-fw flash --file /path/to/firmware.img
```

The command reads the installed version, asks for confirmation, checks
the dock's live flash layout and verifies both partitions after writing.
The dry run checks the file without accessing the dock. On Linux, the N4E
passed a HID round trip from `02.018` to `02.014` and back through the TUI.
Both flashes passed partition readback and firmware version verification
after reboot.

### Windows

If Windows cannot open the ArtInChip USB updater, use [Zadig][zadig]
to install WinUSB for USB device **`33c3:6677`** while the dock is in
updater mode. You can also enter that mode with
`streamdock-fw enter-updater`. Reconnect the dock and rescan before
retrying the update.

Read the current version under **Stream Dock → Settings → Device**, then
quit Stream Dock. Select your dock in the updater and enter the exact
firmware version you saw.

After the dock restarts, enter the version shown in Stream Dock to
confirm the update.

### macOS

Run from Terminal on Apple Silicon.

```sh
./streamdock-fw version
./streamdock-fw flash
```

If macOS cannot read the firmware report, use `flash --from VERSION`
with the exact version from Stream Dock and verify the new version there.

## Build from source

Run `cargo build --release --locked --target-dir target` for a native
build. To create the standalone release binaries from x86-64 Linux with Nix
and rustup installed, use the same command as GitHub Actions.

```sh
nix develop .#release -c bash scripts/release.sh
```

CI builds the binaries for pushes and pull requests, and publishes them
on GitHub Releases when a `v*` tag is pushed.
On Linux, the macOS build uses a pinned SDK for the IOKit and Foundation
linker stubs.

The updater is licensed under the [EUPL-1.2](LICENSE).

## AI assistance

Most of this updater's code and documentation was written with AI
assistance. The project is experimental and has not had an independent
security audit. Hardware testing covered the MiraBox N4E on Linux.

[sdk]: https://github.com/boa-z/artinchip-flash/tree/3945a945dbf7711d205e74db58086021f9474141/crates/artinchip-flash
[releases]: https://github.com/ambiso/streamdock-firmware-updater/releases/latest
[linux-x86-64]: https://github.com/ambiso/streamdock-firmware-updater/releases/latest/download/streamdock-fw-linux-x86_64
[linux-arm64]: https://github.com/ambiso/streamdock-firmware-updater/releases/latest/download/streamdock-fw-linux-arm64
[windows-x86-64]: https://github.com/ambiso/streamdock-firmware-updater/releases/latest/download/streamdock-fw-windows-x86_64.exe
[windows-arm64]: https://github.com/ambiso/streamdock-firmware-updater/releases/latest/download/streamdock-fw-windows-arm64.exe
[macos-arm64]: https://github.com/ambiso/streamdock-firmware-updater/releases/latest/download/streamdock-fw-macos-arm64
[zadig]: https://zadig.akeo.ie/
