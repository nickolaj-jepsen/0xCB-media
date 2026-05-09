{
  description = "0xCB-media — RP2040 media controller firmware + Linux host daemon";

  nixConfig = {
    extra-substituters = [ "https://0xcbmedia.cachix.org" ];
    extra-trusted-public-keys = [
      "0xcbmedia.cachix.org-1:u8PfgqbbO/hjnsA77TCxi5w7hh82dApsqJ4bAgg9Rmo="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, fenix }:
    let
      perSystem = flake-utils.lib.eachDefaultSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };

          # Embedded toolchain for the RP2040 firmware: stock nixpkgs `rustc`
          # doesn't ship `rust-std` for `thumbv6m-none-eabi`, so we pull
          # cargo+rustc+thumbv6m rust-std from fenix and feed them into a
          # dedicated rustPlatform.
          embeddedToolchain = with fenix.packages.${system}; combine [
            stable.cargo
            stable.rustc
            targets.thumbv6m-none-eabi.stable.rust-std
          ];

          embeddedRustPlatform = pkgs.makeRustPlatform {
            cargo = embeddedToolchain;
            rustc = embeddedToolchain;
          };

          host = pkgs.rustPlatform.buildRustPackage {
            pname = "0xcb-media-host";
            version = "0.1.0";
            src = ./.;

            cargoLock.lockFile = ./Cargo.lock;

            cargoBuildFlags = [ "-p" "host" ];
            cargoTestFlags  = [ "-p" "host" ];

            # `pipewire-sys` runs bindgen against the libpipewire / libspa
            # headers, so we need clang at build time. `LIBCLANG_PATH` and
            # `BINDGEN_EXTRA_CLANG_ARGS` are wired up below.
            nativeBuildInputs = [ pkgs.pkg-config pkgs.clang ];
            buildInputs       = [ pkgs.udev pkgs.dbus pkgs.pipewire ];

            LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-I${pkgs.glibc.dev}/include -I${pkgs.pipewire.dev}/include/pipewire-0.3 -I${pkgs.pipewire.dev}/include/spa-0.2";

            meta = {
              description = "0xCB-media Linux daemon: streams PipeWire volume + an audio visualizer to the macropad over USB CDC ACM.";
              license     = pkgs.lib.licenses.gpl2Plus;
              mainProgram  = "0xcb-media-host";
              platforms   = pkgs.lib.platforms.linux;
            };
          };

          firmware = embeddedRustPlatform.buildRustPackage {
            pname = "0xcb-media-firmware";
            version = "0.1.0";
            src = ./.;

            cargoLock.lockFile = ./Cargo.lock;

            doCheck   = false;
            auditable = false;

            nativeBuildInputs = [ pkgs.elf2uf2-rs ];

            # Override buildPhase to avoid `cargoBuildHook` auto-injecting
            # `--target x86_64-unknown-linux-gnu`, which combined with our
            # thumbv6m target would trigger an unwanted multi-target build.
            buildPhase = ''
              runHook preBuild
              cargo build \
                --offline \
                --frozen \
                --release \
                -p firmware \
                --target thumbv6m-none-eabi \
                -j $NIX_BUILD_CORES
              runHook postBuild
            '';

            installPhase = ''
              runHook preInstall
              mkdir -p $out/bin
              cp target/thumbv6m-none-eabi/release/firmware $out/bin/firmware.elf
              elf2uf2-rs $out/bin/firmware.elf $out/bin/firmware.uf2
              runHook postInstall
            '';

            meta = {
              description = "0xCB-1337 rev5.0 media controller firmware (RP2040, Embassy).";
              license     = pkgs.lib.licenses.gpl2Plus;
              platforms   = pkgs.lib.platforms.all;
            };
          };
        in {
          packages = {
            inherit host firmware;
            default = host;
          };

          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              # Rust toolchain (rustup honors rust-toolchain.toml in repo root)
              rustup

              # Embedded toolchain
              probe-rs-tools
              elf2uf2-rs

              # Host-side build deps
              pkg-config
              udev          # libudev for serialport / probe-rs
              dbus          # libdbus — pipewire pulls it in transitively
              systemd       # systemd headers for the user unit + libudev
              pipewire      # libpipewire-0.3 for the audio visualizer
              # pipewire-sys uses bindgen — libclang must be discoverable.
              clang
              libclang.lib

              # Convenience
              cargo-binutils
              picocom
            ];

            # Make `cargo` see system libraries
            shellHook = ''
              export PKG_CONFIG_PATH="${pkgs.udev.dev}/lib/pkgconfig:${pkgs.dbus.dev}/lib/pkgconfig:${pkgs.pipewire.dev}/lib/pkgconfig:$PKG_CONFIG_PATH"
              export LIBCLANG_PATH="${pkgs.libclang.lib}/lib"
              echo "0xCB-media devShell ready."
              echo "  rustup show          → toolchain info"
              echo "  cargo run -p firmware → flash via probe-rs"
              echo "  cargo run -p host    → run Linux daemon"
            '';
          };
        });
    in
      perSystem // {
        nixosModules.default = { config, lib, pkgs, ... }:
          let
            cfg = config.services."0xcb-media-host";
          in {
            options.services."0xcb-media-host" = {
              enable = lib.mkEnableOption "0xCB-media host daemon (per-user systemd unit streaming volume + visualizer to the macropad)";

              package = lib.mkOption {
                type        = lib.types.package;
                default     = self.packages.${pkgs.stdenv.hostPlatform.system}.host;
                defaultText = lib.literalExpression "0xCB-media.packages.\${pkgs.stdenv.hostPlatform.system}.host";
                description = "The host daemon package to run.";
              };

              serialDevice = lib.mkOption {
                type        = lib.types.str;
                default     = "/dev/ttyACM0";
                description = ''
                  CDC ACM serial device the macropad enumerates as. Exposed to the
                  daemon via the OXCB_MEDIA_SERIAL environment variable. The user
                  running the unit must have read/write access (typically by being
                  in the `dialout` group, or via a udev rule).
                '';
              };

              extraArgs = lib.mkOption {
                type        = lib.types.listOf lib.types.str;
                default     = [ ];
                description = "Extra command-line arguments passed to the daemon.";
              };
            };

            # Note: this module deliberately does NOT manage the `dialout`
            # group. To grant the user running the unit access to the CDC ACM
            # device, add it yourself, e.g.:
            #
            #   users.users.alice.extraGroups = [ "dialout" ];
            #
            # (or install a udev rule that chmods the device.)
            config = lib.mkIf cfg.enable {
              systemd.user.services."0xcb-media-host" = {
                description = "0xCB-media host daemon (volume + visualizer → macropad)";
                wantedBy    = [ "default.target" ];
                after       = [ "graphical-session.target" ];
                partOf      = [ "graphical-session.target" ];

                serviceConfig = {
                  ExecStart = "${lib.getExe cfg.package} ${lib.escapeShellArgs cfg.extraArgs}";
                  Restart    = "on-failure";
                  RestartSec = 2;
                  Environment = [ "OXCB_MEDIA_SERIAL=${cfg.serialDevice}" ];

                  # Hardening — daemon talks to a USB serial device and the
                  # session D-Bus, nothing else.
                  NoNewPrivileges        = true;
                  ProtectSystem          = "strict";
                  ProtectHome            = "read-only";
                  PrivateTmp             = true;
                  RestrictAddressFamilies = [ "AF_UNIX" ];
                };
              };
            };
          };
      };
}
