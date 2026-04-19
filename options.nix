{
  lib,
  ...
}:
{
  options.services.microvm-builder = with lib; {
    enable = mkEnableOption "custom linux builder based on microvm.nix";

    name = mkOption {
      type = types.str;
      default = "microvm-builder";
      example = "linux-builder";
      description = "Service name used for the builder-related units and identifiers.";
    };

    port = mkOption {
      type = types.port;
      default = 2222;
      example = 9000;
      description = "TCP port exposed by the builder service inside the microvm.";
    };

    speedFactor = mkOption {
      type = types.ints.positive;
      default = 1;
      example = 2;
      description = "Relative performance multiplier used by the builder configuration. Higher is faster.";
    };

    cpu = mkOption {
      type = types.ints.positive;
      default = 2;
      example = 4;
      description = "Number of virtual CPUs allocated to the microvm.";
    };

    maxJobs = mkOption {
      type = types.ints.positive;
      default = 1;
      example = 4;
      description = "Maximum number of concurrent jobs the distributed builder should accept.";
    };

    memory = mkOption {
      type = types.ints.positive;
      default = 2048;
      example = 4096;
      description = "Amount of memory for the microvm in MiB.";
    };

    diskSize = mkOption {
      type = types.ints.positive;
      default = 32 * 1024;
      example = 100 * 1024;
      description = "Size of the writable disk image attached to the microvm in MiB.";
    };

    workingDirectory = mkOption {
      type = types.str;
      default = "/var/lib/microvm-builder";
      example = "/var/lib/linux-builder";
      description = "Working directory used by the launchd daemon on the host.";
    };
  };
}
