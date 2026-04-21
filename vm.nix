{ ... }:
{
  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  networking.useDHCP = true;

  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "yes";
      PasswordAuthentication = false;
    };
  };

  users.users.root = {
    isSystemUser = true;
    openssh.authorizedKeys.keyFiles = [
      ./keys/id_ecdsa.pub
    ];
  };

  system.stateVersion = "25.11";
}
