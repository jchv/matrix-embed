{
  description = "A flake for building the matrix-embed bot";

  inputs = {
    nixpkgs.url = "github:Nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    let
      matrixEmbedModule =
        { pkgs, lib, ... }:
        {
          imports = [ ./nix/module.nix ];
          services.matrix-embed.package = lib.mkDefault self.packages.${pkgs.system}.default;
        };
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        lib = nixpkgs.lib;
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = manifest.name;
          version = manifest.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoLock.outputHashes = {
            "matrix-sdk-0.16.0" = "sha256-9PWcs+ZK93uJrBoDCqCpIGfCr71AL2ByMsHnKqPQuMU=";
          };
          doCheck = false;

          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.makeWrapper
          ];
          nativeCheckInputs = [
            pkgs.ffmpeg
            pkgs.cacert
          ];
          buildInputs = [
            pkgs.openssl
            pkgs.sqlite
          ];
          fixupPhase = ''
            wrapProgram $out/bin/matrix-embed \
              --prefix PATH : ${lib.makeBinPath [ pkgs.ffmpeg ]}
          '';
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
            pkg-config
            openssl
            sqlite
            ffmpeg
          ];
        };
      }
    )
    // {
      nixosModules.matrix-embed = matrixEmbedModule;
      nixosModules.default = matrixEmbedModule;
    };
}
