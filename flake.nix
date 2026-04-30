{
  description = "hwrng — CLI wrapper for the Linux hwrng subsystem";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    let
      overlay = final: _prev: {
        hwrng = final.rustPlatform.buildRustPackage {
          pname = "hwrng";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta = {
            description = "CLI wrapper for the Linux hwrng subsystem";
            mainProgram = "hwrng";
            platforms = final.lib.platforms.linux;
          };
        };
      };

      nixosModule = { config, lib, pkgs, ... }:
        let
          cfg = config.services.hwrng-watch;
        in
        {
          options.services.hwrng-watch = {
            enable = lib.mkEnableOption
              "hwrng-watch — keeps a chosen hwrng as the active /sys/class/misc/hw_random/rng_current";

            rng = lib.mkOption {
              type = lib.types.str;
              example = "infnoise";
              description = ''
                Name (or unique prefix) of the hwrng to keep active. Matched
                against /sys/class/misc/hw_random/rng_available the same way
                the `hwrng switch` command does it.
              '';
            };

            interval = lib.mkOption {
              type = lib.types.either lib.types.float lib.types.int;
              default = 2.0;
              description = "Poll interval in seconds.";
            };

            package = lib.mkOption {
              type = lib.types.package;
              default = pkgs.hwrng;
              defaultText = lib.literalExpression "pkgs.hwrng";
              description = "The hwrng package providing the watcher binary.";
            };
          };

          config = lib.mkIf cfg.enable {
            nixpkgs.overlays = lib.mkBefore [ overlay ];

            systemd.services.hwrng-watch = {
              description = "Pin the active Linux hwrng to ${cfg.rng}";
              wantedBy = [ "multi-user.target" ];
              after = [ "systemd-udevd.service" ];

              serviceConfig = {
                Type = "simple";
                ExecStart = lib.concatStringsSep " " [
                  "${cfg.package}/bin/hwrng"
                  "watch"
                  (lib.escapeShellArg cfg.rng)
                  "--interval"
                  (toString cfg.interval)
                ];

                Restart = "on-failure";
                RestartSec = "5s";

                # Writing /sys/class/misc/hw_random/rng_current needs uid 0
                # because the file is mode 0644 owned by root — capabilities
                # do not help. Drop everything else.
                User = "root";
                CapabilityBoundingSet = "";
                AmbientCapabilities = "";
                NoNewPrivileges = true;

                # Sandboxing. ProtectKernelTunables must stay off — it makes
                # /sys read-only, which would block the rng_current write.
                ProtectSystem = "strict";
                ProtectHome = true;
                PrivateTmp = true;
                PrivateNetwork = true;
                ProtectKernelTunables = false;
                ProtectKernelModules = true;
                ProtectKernelLogs = true;
                ProtectControlGroups = true;
                ProtectClock = true;
                ProtectHostname = true;
                RestrictAddressFamilies = [ "AF_UNIX" ];
                RestrictNamespaces = true;
                RestrictRealtime = true;
                RestrictSUIDSGID = true;
                LockPersonality = true;
                MemoryDenyWriteExecute = true;
                SystemCallArchitectures = "native";
                SystemCallFilter = [ "@system-service" "~@privileged" "~@resources" ];
              };
            };
          };
        };
    in
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; overlays = [ overlay ]; };
      in
      {
        packages = {
          default = pkgs.hwrng;
          hwrng = pkgs.hwrng;
        };

        apps.default = {
          type = "app";
          program = "${pkgs.hwrng}/bin/hwrng";
        };

        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rust-analyzer
            pkgs.rustfmt
          ];
        };

        checks.build = pkgs.hwrng;
      }
    ) // {
      overlays.default = overlay;
      nixosModules.default = nixosModule;
      nixosModules.hwrng-watch = nixosModule;
    };
}
