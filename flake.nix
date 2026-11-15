{
  description = "Foundation development environment and production images";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/f9a75f41be3a74eab55d3a7679d34ad6cc0da977";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/42076d1c6a0fb405ed04f9a674e4a8f69de62ba3";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane/v0.24.0";
  };

  outputs = { self, nixpkgs, rust-overlay, crane, ... }:
    let
      systems = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      rustPackages = system:
        let
          pkgs = import nixpkgs { inherit system; overlays = [ (import rust-overlay) ]; };
          rust = pkgs.rust-bin.fromRustupToolchainFile ./services/catalyst/rust-toolchain.toml;
        in { inherit pkgs; craneLib = (crane.mkLib pkgs).overrideToolchain rust; };
      # Three chained layers: third-party crates, Bevy and the crates that use it, the Explorer.
      bevy-explorer = import ./infra/packages/bevy-explorer.nix (rustPackages "x86_64-linux");
    in {
      packages = nixpkgs.lib.genAttrs [ "aarch64-linux" "x86_64-linux" ] (system:
        import ./infra/packages (rustPackages system // {
          revision = self.rev or self.dirtyRev or "unversioned";
        })) // {
          # The tool environments alone: the macOS CI legs take ffmpeg from here.
          aarch64-darwin = import ./infra/packages/tools.nix { pkgs = import nixpkgs { system = "aarch64-darwin"; }; };
        };

      # `nix build .#bevy-explorer` and `.#bevy-explorer.tests`. An evaluation of the Explorer
      # builds its patched sources, and `nix flake check` evaluates every attribute of
      # `packages` and `checks`, so the Explorer is in neither.
      legacyPackages.x86_64-linux = {
        inherit bevy-explorer;
        bevy-explorer-deps = bevy-explorer.deps;
        bevy-explorer-bevy = bevy-explorer.bevy;
      };

      checks.x86_64-linux.dev-service = import ./infra/dev/service-test {
        pkgs = import nixpkgs { system = "x86_64-linux"; };
        devShell = self.devShells.x86_64-linux.default;
        inputs = { inherit nixpkgs rust-overlay crane; };
      };

      devShells = eachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; overlays = [ (import rust-overlay) ]; };
          rust = pkgs.rust-bin.fromRustupToolchainFile ./services/catalyst/rust-toolchain.toml;
          dev = pkgs.writeShellScriptBin "foundation-dev" ''
            exec python3 "''${FOUNDATION_DEV_ROOT:?Enter nix develop first}/tooling/dev.py" "$@"
          '';
          environment = {
            buildInputs = [ pkgs.openssl ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.libiconv ];
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            OPENSSL_NO_VENDOR = "1";
            shellHook = builtins.readFile ./infra/dev/shell-hook.sh;
          } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
            # The prebuilt V8 the image build links (infra/packages/catalyst.nix), so a dev
            # build neither downloads it nor trusts a restored target cache to still hold it.
            RUSTY_V8_ARCHIVE = "${pkgs.callPackage ./infra/packages/librusty-v8.nix { }}";
          };
          default = pkgs.mkShell (environment // {
            packages = with pkgs; [
              rust nodejs_26 (pnpm.override { nodejs-slim = nodejs-slim_26; })
              postgresql_18 process-compose watchexec livekit nats-server python3 bashInteractive
              gitMinimal git-lfs gh ripgrep libarchive pkg-config cmake clang protobuf curl jq ffmpeg-headless dev
            ];
            LOCALE_ARCHIVE = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux "${pkgs.glibcLocales}/lib/locale/locale-archive";
          });
          toolchain = (builtins.fromTOML (builtins.readFile ./services/catalyst/rust-toolchain.toml)).toolchain;
          ci = pkgs.mkShell (environment // {
            packages = with pkgs; [
              (rust-bin.fromRustupToolchain (toolchain // { profile = "minimal"; }))
              postgresql_18 process-compose nats-server python3 bashInteractive gitMinimal
              pkg-config cmake clang protobuf dev
            ];
            LOCALE_ARCHIVE = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux "${
              pkgs.glibcLocales.override { allLocales = false; locales = [ "C.UTF-8/UTF-8" "en_US.UTF-8/UTF-8" ]; }
            }/lib/locale/locale-archive";
          });
          # The web explorer's loop, `just wasm` in rendering/bevy-explorer/source: the nightly its
          # justfile pins, and wasm-bindgen at that workspace's Cargo.lock version, so wasm-pack
          # downloads neither it nor wasm-opt. No services: the shell hook is the default shell's.
          # Node 26 has no corepack, so the shell carries pnpm and the justfile reads PNPM. This
          # pnpm must not fetch the version a package.json pins: that binary cannot start on NixOS.
          wasm = pkgs.mkShell {
            packages = with pkgs; [
              (rust-bin.nightly."2026-04-15".default.override {
                targets = [ "wasm32-unknown-unknown" ];
                extensions = [ "rust-src" ];
              })
              just wasm-pack wasm-bindgen-cli_0_2_114 binaryen nodejs_26 python3 gitMinimal
              (pnpm.override { nodejs-slim = nodejs-slim_26; })
              pkg-config cmake clang protobuf
            ] ++ lib.optionals stdenv.hostPlatform.isLinux [ udev ];
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            PNPM = "pnpm";
            pnpm_config_manage_package_manager_versions = "false";
          };
        in {
          inherit default ci wasm;
          # foundation-dev desktop: the Tauri shell (apps/desktop). Linux needs
          # WebKitGTK; libsoup gets TLS from glib-networking.
          desktop = default.overrideAttrs (old: {
            buildInputs = old.buildInputs ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux
              (with pkgs; [ gtk3 webkitgtk_4_1 libsoup_3 glib-networking ]);
            GIO_EXTRA_MODULES = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux
              "${pkgs.glib-networking}/lib/gio/modules";
            WEBKIT_DISABLE_DMABUF_RENDERER = "1";
            FOUNDATION_DEV_DESKTOP_SHELL = "1";
          });
        });
    };
}
