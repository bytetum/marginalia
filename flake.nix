{
  description = "Marginalia: a personal reading library with an agent, REST and MCP";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system (import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      }));

      # The toolchain pinned in rust-toolchain.toml (single source of truth), from official binaries.
      # nixpkgs' own rustc is built against the system LLVM and cannot compile the AVX-512 intrinsics
      # SurrealDB uses (see shell.nix), so it must not be used here.
      toolchainFor = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      packageFor = pkgs:
        let
          toolchain = toolchainFor pkgs;
          rustPlatform = pkgs.makeRustPlatform { cargo = toolchain; rustc = toolchain; };
        in
        rustPlatform.buildRustPackage {
          pname = "marginalia";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          # Only what the build and tests read, so editing docs or deploy files does not rebuild.
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let rel = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
              in !(builtins.elem rel [ "deploy" "skills" "README.md" "DESIGN.md" ".env.example" ".gitignore" "flake.nix" "flake.lock" "shell.nix" ".github" ])
                && pkgs.lib.cleanSourceFilter path type;
          };
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.cmake pkgs.pkg-config ];
          # Tests use in-memory stores and local fake servers only; no network.
          doCheck = true;
          # reqwest (rustls) loads the system CA bundle when a client is built, even if nothing is fetched;
          # the build sandbox has none.
          nativeCheckInputs = [ pkgs.cacert ];
          preCheck = "export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          meta = {
            description = "Personal reading library with an agent, REST and MCP";
            mainProgram = "marginalia";
            platforms = systems;
          };
        };
    in
    {
      packages = forAllSystems (_: pkgs: rec {
        marginalia = packageFor pkgs;
        default = marginalia;
      });

      apps = forAllSystems (system: _: rec {
        marginalia = { type = "app"; program = "${self.packages.${system}.default}/bin/marginalia"; meta.description = "Run marginalia"; };
        default = marginalia;
      });

      devShells = forAllSystems (_: pkgs: {
        default = pkgs.mkShell {
          packages = [
            ((toolchainFor pkgs).override { extensions = [ "rust-src" "clippy" "rustfmt" "rust-analyzer" ]; })
            pkgs.cmake
            pkgs.pkg-config
          ] ++ pkgs.lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.gcc;
        };
      });

      checks = forAllSystems (system: _: {
        default = self.packages.${system}.default;
      });

      formatter = forAllSystems (_: pkgs: pkgs.nixpkgs-fmt);
    };
}
