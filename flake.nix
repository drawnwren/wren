{
  description = "wren native modal editor workspace";

  inputs = {
    # This exact nixpkgs revision pins the Neovim oracle. Bump it and regenerate
    # version-keyed goldens in the same change.
    nixpkgs.url = "github:NixOS/nixpkgs/0e251e24a4f24e036a084b6b4b2d2491af4167f4";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crane, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      perSystem = system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (crane.mkLib pkgs).overrideToolchain rust;
          fuzzRust = pkgs.rust-bin.nightly."2026-08-14".minimal;
          fuzzCraneLib = (crane.mkLib pkgs).overrideToolchain fuzzRust;
          cargoSource = pkgs.lib.cleanSourceWith {
            name = "wren-cargo-source";
            src = ./.;
            filter = path: type:
              let
                sourcePath = toString path;
                cargoAsset = pkgs.lib.any (suffix: pkgs.lib.hasSuffix suffix sourcePath) [
                  ".proto"
                  ".scm"
                  ".wgsl"
                  ".wit"
                ];
                checkAsset = pkgs.lib.any (suffix: pkgs.lib.hasSuffix suffix sourcePath) [
                  "/crates/wren-types/proptest-regressions/lib.txt"
                  "/harness/corpus/documents/unicode.txt"
                  "/scripts/layer-check.py"
                ] || pkgs.lib.hasInfix "/harness/conformance/goldens/" sourcePath;
              in
                craneLib.filterCargoSources path type || cargoAsset || checkAsset;
          };
          oracle = pkgs.neovim;
          # Wren's language profiles are expected to work from the installed
          # package, not only from a project dev shell. Keep every configured
          # server and its external formatter on the wrapper PATH. Project-local
          # tools still win because wrapProgram appends this fallback PATH after
          # the caller's existing PATH.
          languageRuntimeInputs = [
            pkgs.astyle
            pkgs.basedpyright
            pkgs.bash-language-server
            pkgs.cargo
            pkgs.clang
            pkgs.clang-tools
            pkgs.clippy
            pkgs.fourmolu
            pkgs.ghc
            pkgs.go
            pkgs.gopls
            pkgs.haskell-language-server
            pkgs.lua
            pkgs.lua-language-server
            pkgs.nix
            pkgs.nixd
            pkgs.nixfmt
            pkgs.opentofu
            pkgs.pnpm
            pkgs.python3
            pkgs.ruff
            pkgs.rust-analyzer
            pkgs.rustfmt
            pkgs.terraform-ls
            pkgs.typescript
            pkgs.typescript-language-server
          ];
          runtimeInputs = [
            pkgs.openssh
            pkgs.git
            pkgs.lazygit
            pkgs.ripgrep
            pkgs.bashInteractive
          ] ++ languageRuntimeInputs;
          developmentInputs = runtimeInputs ++ [
            pkgs.cargo-deny
            pkgs.cargo-fuzz
            pkgs.cargo-nextest
            pkgs.tree-sitter
            oracle
          ];
          runtimeToolsCheck = pkgs.runCommand "wren-runtime-tools-check" {
            nativeBuildInputs = runtimeInputs;
          } ''
            for tool in \
              astyle basedpyright-langserver bash-language-server cargo clang++ clangd \
              clippy-driver fourmolu ghc ghci go gofmt gopls \
              haskell-language-server-wrapper lua-language-server luac \
              nix-instantiate nixd nixfmt pnpm python3 ruff rust-analyzer rustfmt \
              lazygit terraform-ls tofu tsc typescript-language-server
            do
              if ! command -v "$tool" >/dev/null; then
                echo "missing packaged runtime tool: $tool" >&2
                exit 1
              fi
            done
            mkdir -p "$out"
            touch "$out/passed"
          '';
          commonArgs = {
            pname = "wren";
            version = "0.1.0";
            src = cargoSource;
            strictDeps = true;
            cargoExtraArgs = "--locked --workspace";
          };
          cargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
            pname = "wren-dependencies";
          });
          packageArgs = commonArgs // {
            cargoExtraArgs = "--locked -p wren-tui --bin wren";
          };
          packageCargoArtifacts = craneLib.buildDepsOnly (packageArgs // {
            pname = "wren-package-dependencies";
          });
          package = craneLib.buildPackage (packageArgs // {
            cargoArtifacts = packageCargoArtifacts;
            # The dedicated cargoNextest check below owns the test suite so a
            # package build does not compile and execute every test twice.
            doCheck = false;
            nativeBuildInputs = [
              pkgs.installShellFiles
              pkgs.makeWrapper
            ];
            postFixup = ''
              wrapProgram "$out/bin/wren" \
                --suffix PATH : ${pkgs.lib.makeBinPath runtimeInputs}
            '';
          });
          mkCargoCheck = name: command: craneLib.mkCargoDerivation (commonArgs // {
            pname = "wren-${name}-check";
            inherit cargoArtifacts;
            nativeBuildInputs = developmentInputs;
            preBuild = ''
              export HOME="$TMPDIR/home"
              mkdir -p "$HOME"
            '';
            buildPhaseCargoCommand = command;
            doInstallCargoArtifacts = false;
            installPhaseCommand = ''
              mkdir -p "$out"
              touch "$out/passed"
            '';
            doCheck = false;
          });
          fuzzCargoVendorDir = fuzzCraneLib.vendorCargoDeps {
            src = cargoSource;
            cargoLock = ./fuzz/Cargo.lock;
          };
          fuzzArgs = {
            pname = "wren-fuzz-smoke-check";
            version = "0.1.0";
            src = cargoSource;
            strictDeps = true;
            CARGO_TARGET_DIR = "target";
            cargoToml = ./Cargo.toml;
            cargoLock = ./fuzz/Cargo.lock;
            cargoVendorDir = fuzzCargoVendorDir;
            cargoExtraArgs = "--locked --manifest-path fuzz/Cargo.toml";
            # Crane installs an overridden lockfile at the source root. Mirror
            # it beside the nested fuzz manifest before invoking Cargo there.
            postConfigure = ''
              cp Cargo.lock fuzz/Cargo.lock
            '';
          };
          fuzzDummySrc = fuzzCraneLib.mkDummySrc {
            src = cargoSource;
            cargoLock = ./fuzz/Cargo.lock;
            cleanCargoTomlFilter = path:
              pkgs.lib.lists.hasPrefix [ "package" "metadata" ] path
              || fuzzCraneLib.filters.cargoTomlDefault path;
          };
          fuzzCargoArtifacts = fuzzCraneLib.buildDepsOnly ((builtins.removeAttrs fuzzArgs [ "src" ]) // {
            pname = "wren-fuzz-dependencies";
            dummySrc = fuzzDummySrc;
            nativeBuildInputs = [ pkgs.cargo-fuzz ];
            doCheck = false;
            buildPhaseCargoCommand = ''
              cargo fuzz build --fuzz-dir fuzz ex_parse
              cargo fuzz build --fuzz-dir fuzz protocol_decode
            '';
          });
          fuzzCheck = fuzzCraneLib.mkCargoDerivation (fuzzArgs // {
            cargoArtifacts = fuzzCargoArtifacts;
            nativeBuildInputs = [ pkgs.cargo-fuzz ];
            preBuild = ''
              export HOME="$TMPDIR/home"
              mkdir -p "$HOME"
            '';
            buildPhaseCargoCommand = ''
              cargo fuzz run --fuzz-dir fuzz ex_parse -- -runs=1000
              cargo fuzz run --fuzz-dir fuzz protocol_decode -- -runs=1000
            '';
            doInstallCargoArtifacts = false;
            installPhaseCommand = ''
              mkdir -p "$out"
              touch "$out/passed"
            '';
            doCheck = false;
          });
        in {
          inherit
            pkgs
            rust
            craneLib
            fuzzRust
            fuzzCraneLib
            oracle
            package
            mkCargoCheck
            fuzzCheck
            runtimeToolsCheck
            languageRuntimeInputs
            runtimeInputs
            developmentInputs
            commonArgs
            cargoArtifacts
            ;
        };
    in {
      devShells = forAllSystems (system:
        let scope = perSystem system;
        in {
          default = scope.craneLib.devShell {
            packages = scope.developmentInputs;
            NVIM_ORACLE_VERSION = scope.oracle.version;
          };
          fuzz = scope.fuzzCraneLib.devShell {
            packages = [ scope.pkgs.cargo-fuzz ];
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
          runtime-tools = scope.runtimeToolsCheck;
          fmt = scope.craneLib.cargoFmt (scope.commonArgs // {
            cargoExtraArgs = "--all";
          });
          clippy = scope.craneLib.cargoClippy (scope.commonArgs // {
            cargoArtifacts = scope.cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- -D warnings";
          });
          # Advisory DB refresh needs network and remains part of CI's full
          # `cargo deny check`; the sandboxed flake check covers local policy.
          deny = scope.mkCargoCheck "deny" "cargo deny check bans licenses sources";
          layer = scope.mkCargoCheck "layer" "python3 scripts/layer-check.py";
          nextest = scope.craneLib.cargoNextest (scope.commonArgs // {
            cargoArtifacts = scope.cargoArtifacts;
            cargoNextestExtraArgs = "--test-threads=1";
            nativeBuildInputs = scope.developmentInputs;
            # Keep the fake-LSP latency assertion focused on the client by
            # taking Nix's cold Python/module load out of the measurement.
            preCheck = ''
              python3 -c 'import json, sys, time'
            '';
          });
          conformance = scope.mkCargoCheck "conformance" ''
            cargo run -p wren-conformance --locked -- --check-determinism
            cargo run -p wren-conformance --locked -- --check-wren
          '';
          bench-smoke = scope.mkCargoCheck "bench-smoke" ''
            cargo run -p wren-corpus --locked --bin wren-corpus -- generate
            cargo bench -p wren-text --locked --bench textstore -- --test
            cargo bench -p wren-provider --locked --bench provider_acceleration -- --test
          '';
          latency = scope.mkCargoCheck "latency" ''
            cargo run -p wren-latency --locked --release -- --iterations 10000 --output "$TMPDIR/latency.json"
            test -s "$TMPDIR/latency.json"
          '';
          startup = scope.mkCargoCheck "startup" ''
            cargo run -p wren-startup --locked --release -- --iterations 1000 --output "$TMPDIR/startup.json"
            test -s "$TMPDIR/startup.json"
          '';
          tiling-performance = scope.mkCargoCheck "tiling-performance" ''
            cargo run -p wren-tiling-performance --locked --release -- --iterations 1000 --output "$TMPDIR/tiling-performance.json"
            test -s "$TMPDIR/tiling-performance.json"
          '';
          system-gates = scope.mkCargoCheck "system-gates" ''
            cargo run -p wren-system-gates --locked --release -- --iterations 1000 --output "$TMPDIR/system-gates.json"
            test -s "$TMPDIR/system-gates.json"
          '';
          fuzz-smoke = scope.fuzzCheck;
        });
    };
}
