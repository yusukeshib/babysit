{
  description = "Wrap a shell command in a PTY and expose it to external AI agents (Claude / Codex) via subcommands";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # Keep the package version in lockstep with Cargo.toml.
        manifest = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package;

        babysit = pkgs.rustPlatform.buildRustPackage {
          pname = manifest.name;
          version = manifest.version;

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          meta = with pkgs.lib; {
            description = manifest.description;
            homepage = "https://github.com/yusukeshib/babysit";
            license = licenses.mit;
            maintainers = [ ];
            mainProgram = "babysit";
          };
        };
      in
      {
        packages = {
          default = babysit;
          babysit = babysit;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = babysit;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
          ];
        };
      }
    );
}
