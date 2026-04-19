{ config, ... }:
let
  cfg = config.services.microvm-builder;
in
{
  imports = [
    ./options.nix
  ];

  nix.settings = {
    trusted-users = [ "@wheel" ];
    experimental-features = [
      "nix-command"
      "flakes"
    ];
  };

  networking = {
    hostName = cfg.name;
    useDHCP = true;
  };

  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "no";
      PasswordAuthentication = false;
    };
  };

  services.getty.autologinUser = "builder";

  users.users.builder = {
    isNormalUser = true;
    extraGroups = [ "wheel" ];
    openssh.authorizedKeys.keyFiles = [
      ./keys/id_ecdsa.pub
    ];
  };

  system.stateVersion = "25.11";
}
