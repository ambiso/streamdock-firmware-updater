{
  description = "Linux firmware updater for the MiraBox N4E";

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
    in
    {
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "akp05-updater";
        version = "0.1.0";
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./LICENSE
            ./src
          ];
        };
        cargoLock.lockFile = ./Cargo.lock;

        env.AKP05_FLASHER_LOADER = pkgs.stdenv.cc.bintools.dynamicLinker;
        env.AKP05_FLASHER_LIBRARY_PATH = lib.makeLibraryPath [
          pkgs.libusb1
          pkgs.udev
          pkgs.stdenv.cc.cc.libgcc
        ];
        nativeBuildInputs = [ pkgs.makeWrapper ];
        postInstall = ''
          install -Dm644 LICENSE "$out/share/licenses/akp05-updater/LICENSE"
          wrapProgram "$out/bin/akp05-updater" --prefix PATH : ${
            lib.makeBinPath [
              pkgs.curl
              pkgs.gnutar
              pkgs.gzip
            ]
          }
        '';
        meta.license = lib.licenses.eupl12;
      };

      devShells.${system}.release = pkgs.mkShellNoCC {
        packages = [
          cargoZigbuild
          pkgs.zig
          pkgs.zip
          mingw.stdenv.cc
        ];
        SDKROOT = macSdk;
        MACOSX_DEPLOYMENT_TARGET = "11.3";
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${mingw.stdenv.cc.targetPrefix}gcc";
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-C target-feature=+crt-static -L native=${mingw.windows.pthreads}/lib";
      };

      formatter.${system} = pkgs.nixfmt;
    };
}
