{ inputs, ... }: {
  perSystem =
    {
      system,
      ...
    }:
    let
      pkgs = import inputs.nixpkgs {
        inherit system;

        overlays = [
          inputs.rust-overlay.overlays.default
        ];
      };

      craneLib = (inputs.crane.mkLib pkgs).overrideToolchain (p: p.rust-bin.stable.latest.default);

      commonArgs = {
        inherit (craneLib.crateNameFromCargoToml { cargoToml = ./Cargo.toml; }) pname version;

        src = craneLib.cleanCargoSource ./.;
        strictDeps = true;
      };
    in
    {
      packages.microvm-builder = craneLib.buildPackage (
        commonArgs
        // {
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        }
      );

      devShells.default = craneLib.devShell { };
    };
}
