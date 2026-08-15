{
  description = "wren native modal editor workspace";

  inputs = {
    # This exact nixpkgs revision pins the Neovim oracle. Bump it and regenerate
    # version-keyed goldens in the same change.
    nixpkgs.url = "github:NixOS/nixpkgs/0e251e24a4f24e036a084b6b4b2d2491af4167f4";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      perSystem = system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform { cargo = rust; rustc = rust; };
          oracle = pkgs.neovim;
          commonInputs = [
            rust
            pkgs.cargo-deny
            pkgs.cargo-fuzz
            pkgs.cargo-nextest
            pkgs.python3
            pkgs.tree-sitter
            pkgs.openssh
            pkgs.git
            pkgs.ripgrep
            pkgs.bashInteractive
            oracle
          ];
          package = rustPlatform.buildRustPackage {
            pname = "wren";
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "--workspace" ];
            nativeBuildInputs = [
              pkgs.installShellFiles
              pkgs.makeWrapper
              pkgs.bashInteractive
              pkgs.git
              pkgs.ripgrep
            ];
            doCheck = true;
            postFixup = ''
              wrapProgram "$out/bin/wren" \
                --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.openssh pkgs.git pkgs.ripgrep pkgs.bashInteractive ]}
            '';
          };
          mkCargoCheck = name: command: rustPlatform.buildRustPackage {
            pname = "wren-${name}-check";
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = commonInputs;
            buildPhase = ''
              runHook preBuild
              export HOME="$TMPDIR/home"
              mkdir -p "$HOME"
              ${command}
              runHook postBuild
            '';
            installPhase = ''
              mkdir -p "$out"
              touch "$out/passed"
            '';
            doCheck = false;
          };
        in {
          inherit pkgs rust oracle package mkCargoCheck commonInputs;
        };
    in {
      devShells = forAllSystems (system:
        let scope = perSystem system;
        in {
          default = scope.pkgs.mkShell {
            packages = scope.commonInputs;
            NVIM_ORACLE_VERSION = scope.oracle.version;
          };
        });

      packages = forAllSystems (system:
        let scope = perSystem system;
        in {
          default = scope.package;
          wren = scope.package;
          nvim-oracle = scope.oracle;
        });

      homeManagerModules.default = { config, lib, pkgs, ... }:
        let cfg = config.programs.wren;
        in {
          options.programs.wren = {
            enable = lib.mkEnableOption "the Wren modal code editor";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.system}.wren;
              defaultText = lib.literalExpression "inputs.wren.packages.${pkgs.system}.wren";
              description = "Wren package to install.";
            };
          };
          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ];
          };
        };

      checks = forAllSystems (system:
        let scope = perSystem system;
        in {
          package = scope.package;
          fmt = scope.mkCargoCheck "fmt" "cargo fmt --all --check";
          clippy = scope.mkCargoCheck "clippy" "cargo clippy --workspace --all-targets --locked -- -D warnings";
          # Advisory DB refresh needs network and remains part of CI's full
          # `cargo deny check`; the sandboxed flake check covers local policy.
          deny = scope.mkCargoCheck "deny" "cargo deny check bans licenses sources";
          layer = scope.mkCargoCheck "layer" "python3 scripts/layer-check.py";
          nextest = scope.mkCargoCheck "nextest" "cargo nextest run --workspace --locked";
          conformance = scope.mkCargoCheck "conformance" ''
            cargo run -p wren-conformance --locked -- --check-determinism
            cargo run -p wren-conformance --locked -- --check-wren
          '';
          bench-smoke = scope.mkCargoCheck "bench-smoke" ''
            cargo run -p wren-corpus --locked --bin wren-corpus -- generate
            cargo bench -p wren-text --locked --bench textstore -- --test
          '';
          latency = scope.mkCargoCheck "latency" ''
            cargo run -p wren-latency --locked --release -- --iterations 10000 --gate --output "$TMPDIR/latency.json"
            test -s "$TMPDIR/latency.json"
          '';
          startup = scope.mkCargoCheck "startup" ''
            cargo run -p wren-startup --locked --release -- --iterations 1000 --gate --output "$TMPDIR/startup.json"
            test -s "$TMPDIR/startup.json"
          '';
          system-gates = scope.mkCargoCheck "system-gates" ''
            cargo run -p wren-system-gates --locked --release -- --iterations 1000 --gate --output "$TMPDIR/system-gates.json"
            test -s "$TMPDIR/system-gates.json"
          '';
          fuzz-smoke = scope.mkCargoCheck "fuzz-smoke" ''
            cargo fuzz run --fuzz-dir fuzz ex_parse -- -runs=1000
            cargo fuzz run --fuzz-dir fuzz protocol_decode -- -runs=1000
          '';
        });
    };
}
