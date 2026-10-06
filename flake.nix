{
  description = "xdg-kit-rs, Rust crate for: MIME types, default applications, desktop entries, and more";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    systems.url = "github:nix-systems/default-linux";
    # The reference MIME database and its detection test suite (GPL-2.0),
    # used by the MIME tests; never copied into this repository.
    shared-mime-info = {
      url = "git+https://gitlab.freedesktop.org/xdg/shared-mime-info.git";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      systems,
      shared-mime-info,
      ...
    }:
    let
      eachSystem = nixpkgs.lib.genAttrs (import systems);
      # Tools the tests run besides cargo.
      testTools = pkgs: [
        # `xdg-user-dir`, the reference the user folder tests compare with.
        pkgs.xdg-user-dirs
      ];
    in
    {
      devShells = eachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
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
            XDG_KIT_SMI_SRC = "${shared-mime-info}";
          };
        }
      );

      checks = eachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          workspace = pkgs.stdenv.mkDerivation {
            name = "xdg-kit-rs-checks";
            # Only what cargo needs: copying the whole tree would bring in
            # target/, docs/ and .git, re-hashed on every check.
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./crates
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
            XDG_KIT_SMI_SRC = "${shared-mime-info}";

            buildPhase = ''
              runHook preBuild
              export HOME=$TMPDIR
              cargo fmt --all --check
              cargo clippy --workspace --all-targets --offline -- -D warnings
              cargo test --workspace --offline
              RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --offline
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
