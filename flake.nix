{
  description = "xdg-kit-rs, Rust crate for: MIME types, default applications, desktop entries, and more";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    systems.url = "github:nix-systems/default-linux";
    # The reference MIME database and its detection test suite (GPL-2.0),
    # used by the MIME tests; never copied into this repository. Pinned in
    # flake.lock only: update the other inputs with
    # `nix flake update nixpkgs fenix systems`; moving this one changes what
    # the conformance tests expect, so do it on its own and review
    # crates/xdg-kit-mime/tests/conformance-differences.txt with it.
    # `update-mime-database` is built from this input with nixpkgs' own
    # shared-mime-info recipe (see `testTools`), so a nixpkgs update can also
    # break that build; adjust the override there if it does.
    shared-mime-info = {
      url = "git+https://gitlab.freedesktop.org/xdg/shared-mime-info.git";
      flake = false;
    };
    # The Desktop Entry and Desktop Menu specs (the desktop entry crate
    # follows their text at the pinned commit) and desktop-file-utils, whose
    # `desktop-file-validate` (GPL-2.0-or-later) the validation tests compare
    # with.
    # Neither is copied into this repository. Pinned in flake.lock only, like
    # shared-mime-info: move them on their own and review
    # crates/xdg-kit-desktop-entry/tests/validate-differences.txt with them.
    xdg-specs = {
      url = "git+https://gitlab.freedesktop.org/xdg/xdg-specs.git";
      flake = false;
    };
    desktop-file-utils = {
      url = "git+https://gitlab.freedesktop.org/xdg/desktop-file-utils.git";
      flake = false;
    };
    # A nightly Rust for fuzzing only (cargo-fuzz needs -Z flags and the
    # AddressSanitizer runtime, which nixpkgs' stable rustc lacks).
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      systems,
      shared-mime-info,
      xdg-specs,
      desktop-file-utils,
      fenix,
      ...
    }:
    let
      eachSystem = nixpkgs.lib.genAttrs (import systems);
      # The nightly Rust the fuzz targets are built with.
      nightly =
        system:
        fenix.packages.${system}.latest.withComponents [
          "cargo"
          "rustc"
          "rust-std"
        ];
      # Seconds each fuzz target runs in `checks.fuzz`.
      fuzzSeconds = 15;
      # libFuzzer's defaults allow 20 minutes and 2 GB per input.
      fuzzLimits = "-timeout=5 -malloc_limit_mb=64 -rss_limit_mb=1024";
      # `update-mime-database` from the same commit as the conformance data,
      # built with nixpkgs' recipe minus its patches (made for a release).
      # Being a build input, its `share/mime` also lands on XDG_DATA_DIRS in
      # the dev shell and the checks: tests must set XDG_DATA_DIRS themselves
      # rather than read the environment's.
      updateMimeDatabase =
        pkgs:
        pkgs.shared-mime-info.overrideAttrs {
          version = "git-${shared-mime-info.shortRev or "dirty"}";
          src = shared-mime-info;
          patches = [ ];
        };
      # Tools the tests run besides cargo.
      testTools = pkgs: [
        # `xdg-user-dir`, the reference the user folder tests compare with.
        pkgs.xdg-user-dirs
        (updateMimeDatabase pkgs)
        # `desktop-file-validate` from the pinned commit, the reference the
        # validation tests compare with: nixpkgs' recipe minus its patches,
        # like `update-mime-database`. A nixpkgs update can break this build
        # too (its postPatch edits src/install.c); adjust the override then.
        (pkgs.desktop-file-utils.overrideAttrs {
          version = "git-${desktop-file-utils.shortRev or "dirty"}";
          src = desktop-file-utils;
          patches = [ ];
        })
        # `dbus-daemon`, a private bus for the D-Bus activation test.
        pkgs.dbus
        # `gio mime`, the reference the MIME applications tests compare with.
        # GLib from the locked nixpkgs: it changes only with
        # `nix flake update nixpkgs`.
        pkgs.glib.bin
      ];
      # Where the tests find the pinned sources.
      sourceVars = {
        XDG_KIT_SMI_SRC = "${shared-mime-info}";
        XDG_KIT_XDG_SPECS_SRC = "${xdg-specs}";
      };
    in
    {
      devShells = eachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell (
            {
              name = "xdg-kit-rs-dev";
              packages =
                with pkgs;
                [
                  cargo
                  rustc
                  clippy
                  rustfmt
                  rust-analyzer
                  nixfmt-tree
                  typos
                ]
                ++ testTools pkgs;

              RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            }
            // sourceVars
          );

          # `nix develop .#fuzz`: a pinned nightly with cargo-fuzz, for the
          # fuzz targets in fuzz/.
          fuzz = pkgs.mkShell {
            name = "xdg-kit-rs-fuzz";
            packages = [
              (nightly system)
              pkgs.cargo-fuzz
            ];
            # Fuzz targets link the C++ runtime of libFuzzer dynamically.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.stdenv.cc.cc.lib ];
          };
        }
      );

      checks = eachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          workspace = pkgs.stdenv.mkDerivation (
            {
              name = "xdg-kit-rs-checks";
              # Only what cargo needs: copying the whole tree would bring in
              # target/, docs/ and .git, re-hashed on every check.
              src = pkgs.lib.fileset.toSource {
                root = ./.;
                fileset = pkgs.lib.fileset.unions [
                  ./Cargo.toml
                  ./Cargo.lock
                  ./LICENSE
                  # Its examples run as doctests of the xdg-kit crate.
                  ./README.md
                  ./crates
                  ./fuzz/fuzz_targets
                ];
              };
              cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ./Cargo.lock; };
              nativeBuildInputs =
                with pkgs;
                [
                  cargo
                  rustc
                  clippy
                  rustfmt
                  rustPlatform.cargoSetupHook
                ]
                ++ testTools pkgs;
              # The sandbox's build folder cannot hold extended attributes, which
              # the `user.mime_type` test needs; the dev shell runs it.
              XDG_KIT_SKIP_XATTR_TEST = "1";

              buildPhase = ''
                runHook preBuild
                export HOME=$TMPDIR
                cargo fmt --all --check
                cargo clippy --workspace --all-targets --all-features --offline -- -D warnings
                cargo test --workspace --all-features --offline
                RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --offline
                # The fuzz crate is outside the workspace; its targets are
                # formatted like the rest.
                rustfmt --check --edition 2024 fuzz/fuzz_targets/*.rs
                # The fuzzing entry points exist only under `--cfg fuzzing`.
                CARGO_TARGET_DIR=target/fuzzing-lint RUSTFLAGS="--cfg fuzzing" \
                  cargo clippy -p xdg-kit-mime -p xdg-kit-desktop-entry -p xdg-kit-mime-apps --all-targets --all-features --offline -- -D warnings
                runHook postBuild
              '';
              installPhase = ''
                runHook preInstall
                touch $out
                runHook postInstall
              '';
            }
            // sourceVars
          );

          # Each fuzz target for `fuzzSeconds`, from seeds that
          # `fuzz/make-seeds.sh` generates here (they are not committed),
          # with AddressSanitizer, a fixed seed and `fuzzLimits` (an input
          # that is slow or allocates much fails too). On a failure the
          # inputs found are printed as Base64: decode one into a file
          # (`base64 -d`) and replay it with
          # `nix develop .#fuzz -c cargo fuzz run <target> <file> -- <fuzzLimits>`.
          fuzz = pkgs.stdenv.mkDerivation {
            name = "xdg-kit-rs-fuzz";
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./LICENSE
                ./crates
                ./fuzz/Cargo.toml
                ./fuzz/Cargo.lock
                ./fuzz/fuzz_targets
                ./fuzz/make-seeds.sh
                ./fuzz/seed-package.xml
              ];
            };
            cargoRoot = "fuzz";
            cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ./fuzz/Cargo.lock; };
            nativeBuildInputs = [
              (nightly system)
              pkgs.cargo-fuzz
              pkgs.rustPlatform.cargoSetupHook
            ]
            # `update-mime-database` for the seeds.
            ++ [ (updateMimeDatabase pkgs) ];
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.stdenv.cc.cc.lib ];

            buildPhase = ''
              runHook preBuild
              export HOME=$TMPDIR CARGO_NET_OFFLINE=true
              sh fuzz/make-seeds.sh
              targets=$(cargo fuzz list)
              [ -n "$targets" ]
              for target in $targets; do
                [ -d fuzz/seeds/$target ]
                mkdir -p fuzz/corpus/$target
                cargo fuzz run $target fuzz/corpus/$target fuzz/seeds/$target \
                  -- -max_total_time=${toString fuzzSeconds} -seed=1 ${fuzzLimits} \
                  || {
                    for found in fuzz/artifacts/$target/*; do
                      echo "$found"
                      base64 -w0 "$found"
                      echo
                    done
                    exit 1
                  }
              done
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              touch $out
              runHook postInstall
            '';
          };
        }
      );

      formatter = eachSystem (system: nixpkgs.legacyPackages.${system}.nixfmt-tree);
    };
}
