{ inputs, self, ... }:
{
  flake.modules.darwin.microvm-builder =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    let
      cfg = config.microvm-builder;

      serviceName = "microvm-builder";

      microvmBuilder = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.microvm-builder;
      gvproxy = lib.getExe' pkgs.gvproxy "gvproxy";
      vfkit = lib.getExe' pkgs.vfkit "vfkit";

      vfkitSocket = "${cfg.runtime.directory}/vfkit.sock";

      microvmModule = {
        imports = [
          inputs.microvm.nixosModules.microvm
        ];

        microvm = {
          optimize.enable = true;

          hypervisor = "vfkit";

          vcpu = cfg.cpu;
          mem = cfg.memory;

          vmHostPackages = pkgs;

          vfkit = {
            rosetta = {
              enable = true;
              install = true;
            };

            extraArgs = [
              "--device"
              "virtio-net,unixSocketPath=${vfkitSocket},mac=${cfg.macAddress}"
            ];
          };

          storeDiskType = "squashfs";
          writableStoreOverlay = "/nix/.rw-store";

          volumes = [
            {
              mountPoint = "/";
              image = cfg.runtime.overlayImage;
              size = cfg.diskSize;
            }
          ];
        };
      };

      builderModule =
        { pkgs, ... }:
        {
          networking.useDHCP = true;

          services.openssh = {
            enable = true;

            settings = {
              PermitRootLogin = "prohibit-password";
              PasswordAuthentication = false;
              KbdInteractiveAuthentication = false;
            };
          };

          system.activationScripts.microvmBuilderSshKey.text = ''
            key="$(
              ${lib.getExe' pkgs.gnused "sed"} \
                -n 's/.* microvm.builder-ssh-key=\([^ ]*\).*/\1/p' \
                /proc/cmdline
            )"

            if [ -n "$key" ]; then
              install -d -m 0700 /root/.ssh

              if printf '%s\n' "$key" | ${lib.getExe' pkgs.coreutils "base64"} -d > /root/.ssh/authorized_keys.tmp; then
                mv /root/.ssh/authorized_keys.tmp /root/.ssh/authorized_keys
                chmod 0600 /root/.ssh/authorized_keys
              else
                rm -f /root/.ssh/authorized_keys.tmp
                echo "Invalid microvm.builder-ssh-key kernel parameter" >&2
                exit 1
              fi
            fi
          '';

          system.stateVersion = "26.11";
        };

      microvmNixos = inputs.nixpkgs.lib.nixosSystem {
        system = "aarch64-linux";
        modules = [
          microvmModule
          builderModule
        ];
      };

      originalMicrovmRunner = lib.getExe microvmNixos.config.microvm.declaredRunner;

      headlessMicrovmRunner = pkgs.writeShellScriptBin "microvm-run" ''
        set -euo pipefail

        public_key=${lib.escapeShellArg cfg.ssh.publicKey}
        runtime_dir=${lib.escapeShellArg cfg.runtime.directory}

        if [[ ! -s "$public_key" ]]; then
          echo "Missing builder public key: $public_key" >&2
          exit 1
        fi

        key_base64="$(
          ${lib.getExe' pkgs.coreutils "base64"} < "$public_key" |
            ${lib.getExe' pkgs.coreutils "tr"} -d '\n'
        )"

        patched_runner="$runtime_dir/microvm-run-patched"

        ${lib.getExe pkgs.gnused} \
          -e 's/--device virtio-serial,stdio //' \
          -e 's/console=hvc0 //' \
          -e "s|cmdline=\"|cmdline=\"microvm.builder-ssh-key=$key_base64 |" \
          ${lib.escapeShellArg originalMicrovmRunner} \
          > "$patched_runner"

        chmod +x "$patched_runner"

        exec "$patched_runner"
      '';

      microvmRunner = lib.getExe headlessMicrovmRunner;
    in
    {
      options.microvm-builder = {
        enable = lib.mkEnableOption "on-demand MicroVM Linux builder";

        ssh = {
          port = lib.mkOption {
            type = lib.types.port;
            default = 2222;
            description = "Host TCP port forwarded to SSH in the builder VM.";
          };

          publicKey = lib.mkOption {
            type = lib.types.str;
            default = "${cfg.runtime.directory}/id_builder.pub";
            description = "Persistent runtime public SSH key authorized inside the builder VM.";
          };

          privateKey = lib.mkOption {
            type = lib.types.str;
            default = "${cfg.runtime.directory}/id_builder";
            description = "Runtime copy of the private SSH key.";
          };
        };

        maxJobs = lib.mkOption {
          type = lib.types.ints.positive;
          default = 8;
          description = "Maximum number of parallel jobs on the builder.";
        };

        speedFactor = lib.mkOption {
          type = lib.types.ints.positive;
          default = 2;
          description = "Relative Nix scheduling preference for this builder.";
        };

        cpu = lib.mkOption {
          type = lib.types.ints.positive;
          default = 8;
          description = "Number of virtual CPUs assigned to the builder.";
        };

        memory = lib.mkOption {
          type = lib.types.ints.positive;
          default = 8192;
          description = "Memory assigned to the builder in MiB.";
        };

        diskSize = lib.mkOption {
          type = lib.types.ints.positive;
          default = 50 * 1024;
          description = "Writable builder disk size in MiB.";
        };

        idleTimeout = lib.mkOption {
          type = lib.types.ints.positive;
          default = 120;
          description = ''
            Number of idle seconds after the final SSH connection closes
            before the VM and gvproxy are stopped.
          '';
        };

        startTimeout = lib.mkOption {
          type = lib.types.ints.positive;
          default = 120;
          description = "Maximum builder startup time in seconds.";
        };

        stopTimeout = lib.mkOption {
          type = lib.types.ints.positive;
          default = 10;
          description = ''
            Grace period in seconds before child process groups receive
            SIGKILL.
          '';
        };

        macAddress = lib.mkOption {
          type = lib.types.str;
          default = "5a:94:ef:e4:0c:ee";
          description = "MAC address assigned to the builder VM.";
        };

        runtime = {
          directory = lib.mkOption {
            type = lib.types.str;
            default = "/Users/${config.system.primaryUser}/.local/state/microvm-builder";
            description = "Runtime directory containing daemon.sock and vfkit.sock.";
          };

          overlayImage = lib.mkOption {
            type = lib.types.str;
            default = "${cfg.runtime.directory}/overlay.img";
            description = "Writable MicroVM overlay image.";
          };

          logFile = lib.mkOption {
            type = lib.types.str;
            default = "${cfg.runtime.directory}/daemon.log";
            description = "Rust daemon stdout and stderr log.";
          };
        };
      };

      config = lib.mkIf cfg.enable {
        nix = {
          distributedBuilds = lib.mkForce true;

          buildMachines = [
            {
              hostName = serviceName;
              protocol = "ssh-ng";

              systems = [
                "aarch64-linux"
                "x86_64-linux"
              ];

              supportedFeatures = [
                "benchmark"
                "big-parallel"
                "kvm"
                "nixos-test"
              ];

              inherit (cfg)
                maxJobs
                speedFactor
                ;
            }
          ];

          settings.builders-use-substitutes = lib.mkDefault true;
        };

        launchd.daemons.${serviceName} = {
          command = ''
            ${microvmBuilder} daemon \
              --runtime-dir ${lib.escapeShellArg cfg.runtime.directory} \
              --gvproxy ${lib.escapeShellArg gvproxy} \
              --vfkit ${lib.escapeShellArg vfkit} \
              --runner ${lib.escapeShellArg microvmRunner} \
              --ssh-port ${toString cfg.ssh.port} \
              --idle-timeout ${toString cfg.idleTimeout} \
              --start-timeout ${toString cfg.startTimeout} \
              --stop-timeout ${toString cfg.stopTimeout}
          '';

          serviceConfig = {
            UserName = config.system.primaryUser;

            WorkingDirectory = cfg.runtime.directory;

            RunAtLoad = true;
            KeepAlive = true;
            ProcessType = "Background";

            StandardOutPath = cfg.runtime.logFile;
            StandardErrorPath = cfg.runtime.logFile;

            EnvironmentVariables = {
              RUST_LOG = "microvm_builder_proxy=debug";
            };
          };
        };

        environment.etc."ssh/ssh_config.d/100-${serviceName}.conf".text = ''
          Host ${serviceName}
            HostName localhost
            User root

            IdentityFile "${cfg.ssh.privateKey}"
            IdentitiesOnly yes

            StrictHostKeyChecking no
            UserKnownHostsFile /dev/null

            ServerAliveInterval 30
            ServerAliveCountMax 4

            ProxyCommand ${microvmBuilder} connect --runtime-dir "${cfg.runtime.directory}"
        '';

        system.activationScripts.preActivation.text = ''
          install -d \
            -m 0700 \
            -o ${lib.escapeShellArg config.system.primaryUser} \
            -g staff \
            ${lib.escapeShellArg cfg.runtime.directory}

          install -d \
            -m 0700 \
            -o ${lib.escapeShellArg config.system.primaryUser} \
            -g staff \
            ${lib.escapeShellArg (builtins.dirOf cfg.runtime.overlayImage)}

          if [[ ! -s ${lib.escapeShellArg cfg.ssh.privateKey} ]]; then
            ${lib.getExe' pkgs.openssh "ssh-keygen"} \
              -t ed25519 \
              -N "" \
              -f ${lib.escapeShellArg cfg.ssh.privateKey} \
              -C microvm-builder

            chown ${lib.escapeShellArg config.system.primaryUser}:staff \
              ${lib.escapeShellArg cfg.ssh.privateKey} \
              ${lib.escapeShellArg cfg.ssh.privateKey}.pub
          fi

          if [[ ! -s ${lib.escapeShellArg cfg.ssh.publicKey} ]]; then
            ${lib.getExe' pkgs.openssh "ssh-keygen"} \
              -y \
              -f ${lib.escapeShellArg cfg.ssh.privateKey} \
              > ${lib.escapeShellArg cfg.ssh.publicKey}

            chown ${lib.escapeShellArg config.system.primaryUser}:staff \
              ${lib.escapeShellArg cfg.ssh.publicKey}
            chmod 0644 ${lib.escapeShellArg cfg.ssh.publicKey}
          fi
        '';
      };
    };
}
