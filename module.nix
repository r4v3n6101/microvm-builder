{ inputs }:
{
  config,
  pkgs,
  lib,
  ...
}:
let
  cfg = config.services.microvm-builder;

  vfkit-sock = "/tmp/${cfg.name}-vfkit.sock";
  ssh-key = "${cfg.workingDirectory}/id_ecdsa";
  nix-store-overlay = "${cfg.workingDirectory}/nix-store-overlay.img";

  microvm =
    (lib.nixosSystem {
      system = "aarch64-linux";
      specialArgs = { inherit inputs; };
      modules = [
        inputs.microvm.nixosModules.microvm
        {
          microvm = {
            optimize.enable = true;

            vcpu = cfg.cpu;
            mem = cfg.memory;

            hypervisor = "vfkit";
            vmHostPackages = inputs.nixpkgs.legacyPackages.aarch64-darwin;
            vfkit = {
              rosetta = {
                enable = true;
                install = true;
              };
              extraArgs = [
                "--device"
                "virtio-net,unixSocketPath=${vfkit-sock},mac=5a:94:ef:e4:0c:ee"
              ];
            };

            writableStoreOverlay = "/nix/.rw-store";
            volumes = [
              {
                image = nix-store-overlay;
                mountPoint = "/";
                size = cfg.diskSize;
              }
            ];
          };
        }
        ./vm.nix
      ];
    }).config.microvm.declaredRunner;

  service-script = pkgs.writeShellScript "${cfg.name}-runner" ''
    rm -f ${vfkit-sock}

    ${lib.getExe pkgs.gvproxy} \
      --ssh-port ${toString cfg.port} \
      --listen-vfkit "unixgram://${vfkit-sock}" \
    >/tmp/${cfg.name}-gvproxy.log 2>/tmp/${cfg.name}-gvproxy.err &
    GVPROXY_PID=$!
    trap 'kill $GVPROXY_PID' EXIT

    until [ -S ${vfkit-sock} ]; do sleep 1; done;

    ${lib.getExe pkgs.unixtools.script} -q "/tmp/${cfg.name}.log" ${lib.getExe microvm} &

    wait -n
  '';
in
{
  imports = [ ./options.nix ];

  config = lib.mkMerge [
    (lib.mkIf cfg.enable {
      nix = {
        buildMachines = [
          {
            hostName = cfg.name;
            protocol = "ssh-ng";
            sshUser = "builder";
            sshKey = ssh-key;
            systems = [
              "aarch64-linux"
              "x86_64-linux"
            ];
            supportedFeatures = [
              "benchmark"
              "kvm"
              "nixos-test"
              "big-parallel"
            ];
            maxJobs = cfg.maxJobs;
            speedFactor = cfg.speedFactor;
          }
        ];
        distributedBuilds = true;
        settings.builders-use-substitutes = lib.mkDefault true;
      };

      system.activationScripts.extraActivation.text = lib.mkAfter ''
        mkdir -p ${cfg.workingDirectory}

        cp ${./keys/id_ecdsa} ${ssh-key}
        chmod 600 ${ssh-key}

        if [ ! -f "${nix-store-overlay}" ]; then 
          truncate -s ${toString cfg.diskSize}M ${nix-store-overlay}
          chmod 0777 ${nix-store-overlay}
        fi
      '';

      environment.etc."ssh/ssh_config.d/098-${cfg.name}.conf".text = ''
        Host ${cfg.name}
          User builder
          Hostname localhost
          Port ${toString cfg.port}
          IdentityFile ${ssh-key}
          StrictHostKeyChecking no
          UserKnownHostsFile /dev/null
      '';

      launchd.daemons.${cfg.name} = {
        command = service-script;
        serviceConfig = {
          KeepAlive = true;
          RunAtLoad = true;
          WorkingDirectory = cfg.workingDirectory;
        };
      };
    })

    (lib.mkIf (!cfg.enable) {
      system.activationScripts.extraActivation.text = lib.mkAfter ''
        rm ${ssh-key}
        rm ${nix-store-overlay}
        rm ${cfg.workingDirectory}/.*sock
        rmdir ${cfg.workingDirectory}
      '';
    })
  ];
}
