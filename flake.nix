{
  description = "Simple linux-builder using microvm.nix";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/master";
    microvm = {
      url = "github:microvm-nix/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs: {
    darwinModules.default = import ./module.nix { inherit inputs; };
  };
}
