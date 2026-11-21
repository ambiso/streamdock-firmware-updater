{
  description = "Firmware updater for compatible MiraBox Stream Dock devices";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs";

  outputs =
    { nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      inherit (pkgs) lib;
      mingw = pkgs.pkgsCross.mingwW64;
      macSdk = pkgs.fetchzip {
        url = "https://github.com/joseluisq/macosx-sdks/releases/download/14.5/MacOSX14.5.sdk.tar.xz";
        hash = "sha256-YABLNOdte3WE8mnUm32zSlQ3lyDFQNnxXTW9gj51MX0=";
      };
      cargoZigbuild = pkgs.cargo-zigbuild.overrideAttrs (old: {
        patches = (old.patches or [ ]) ++ [ ./nix/cargo-zigbuild-macos-version.patch ];
      });
      windowsArmSynchronization = pkgs.runCommand "windows-arm-synchronization" { } ''
        mkdir -p "$out/lib"
        # Rust links synchronization.lib for WaitOnAddress. Use Zig's Windows API set.
        ${pkgs.zig}/bin/zig dlltool -m arm64 \
          -d ${pkgs.zig}/lib/zig/libc/mingw/lib-common/api-ms-win-core-synch-l1-2-0.def \
          -l "$out/lib/libsynchronization.a"
      '';
    in
    {
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "streamdock-firmware-updater";
        version = "0.2.0";
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./LICENSE
            ./licenses
            ./src
          ];
        };
        cargoLock = {
          lockFile = ./Cargo.lock;
          outputHashes."aic-upg-core-0.2.0" = "sha256-t9KMQVIcvjPr+KpPjD/5r/IQG8iu8OAfyJCSsv4V2u0=";
        };

        postInstall = ''
          install -Dm644 LICENSE "$out/share/licenses/streamdock-firmware-updater/LICENSE"
          install -Dm644 licenses/artinchip-flash.txt "$out/share/licenses/streamdock-firmware-updater/artinchip-flash.txt"
        '';
        meta = {
          license = lib.licenses.eupl12;
          mainProgram = "streamdock-fw";
        };
      };

      devShells.${system} = {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
          ];
        };

        release = pkgs.mkShellNoCC {
          packages = [
            cargoZigbuild
            pkgs.binutils
            pkgs.zig
            mingw.stdenv.cc
          ];
          SDKROOT = macSdk;
          MACOSX_DEPLOYMENT_TARGET = "11.3";
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${mingw.stdenv.cc.targetPrefix}gcc";
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-C target-feature=+crt-static -L native=${mingw.windows.pthreads}/lib";
          CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_RUSTFLAGS = "-C target-feature=+crt-static -C link-arg=-lunwind -L native=${windowsArmSynchronization}/lib";
        };
      };

      formatter.${system} = pkgs.nixfmt;
    };
}
