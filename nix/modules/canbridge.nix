{ ... }:
{ config, lib, pkgs, ... }:
with lib;
let
  cfg = config.services.canbridge;
in
{
  options.services.canbridge = {
    canbridge-cfg = mkOption {
      description = "CAN configuration keyed by interface name (e.g. can0, can1).";
      default = {};
      example = {
        can0 = {
          vcan-dev = "vcan0";
          can-dev = "can0";
          port = 4242;
        };
      };

      type = types.attrsOf (types.submodule ({ name, ... }: {
        options = {
          "vcan-dev" = mkOption {
            type = types.str;
            description = "VCAN device for ${name}.";
          };

          "can-dev" = mkOption {
            type = types.str;
            description = "CAN device for ${name}.";
          };

          port = mkOption {
            type = types.port;
            description = "Port used by ${name}.";
          };
        };
      }));
    };

    client = mkEnableOption "CANbridge Client";

    server = {
      enable = mkEnableOption "CANbridge Server";
      virtualDevs = mkEnableOption "CAN devs are virtual";
    };
  };

  config = let
    # Client setup service: creates the vcan interface
    mkClientSetup = name: devCfg: {
      name = "canbridge-client-${name}-setup";
      value = {
        description = "Setup vcan interface for canbridge client ${name}";
        wantedBy = [ "multi-user.target" ];
        before = [ "canbridge-client-${name}.service" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        path = [ pkgs.iproute2 pkgs.kmod ];
        script = ''
          modprobe vcan || true
          ip link add dev ${devCfg.vcan-dev} type vcan 2>/dev/null || true
          ip link set ${devCfg.vcan-dev} up
        '';
      };
    };

    # Client main service: runs canbridge in connect mode
    mkClientService = name: devCfg: {
      name = "canbridge-client-${name}";
      value = {
        description = "CAN bridge client for ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [ "canbridge-client-${name}-setup.service" "network-online.target" ];
        requires = [ "canbridge-client-${name}-setup.service" ];
        wants = [ "network-online.target" ];
        serviceConfig = {
          Type = "simple";
          Restart = "always";
          RestartSec = "1";
          ExecStart = "${pkgs.canbridge.canbridge}/bin/canbridge --mode connect --addr server:${toString devCfg.port} --iface ${devCfg.vcan-dev}";
        };
      };
    };

    # Server setup service: creates the can interface (virtual if virtualDevs)
    mkServerSetup = name: devCfg: {
      name = "canbridge-server-${name}-setup";
      value = {
        description = "Setup CAN interface for canbridge server ${name}";
        wantedBy = [ "multi-user.target" ];
        before = [ "canbridge-server-${name}.service" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        path = [ pkgs.iproute2 pkgs.kmod ];
        script = if cfg.server.virtualDevs then ''
          modprobe vcan || true
          ip link add dev ${devCfg.can-dev} type vcan 2>/dev/null || true
          ip link set ${devCfg.can-dev} up
        '' else ''
          # For physical CAN interfaces, just bring them up
          # The interface should already exist
          ip link set ${devCfg.can-dev} up || true
        '';
      };
    };

    # Server main service: runs canbridge in listen mode
    mkServerService = name: devCfg: {
      name = "canbridge-server-${name}";
      value = {
        description = "CAN bridge server for ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [ "canbridge-server-${name}-setup.service" "network-online.target" ];
        requires = [ "canbridge-server-${name}-setup.service" ];
        wants = [ "network-online.target" ];
        serviceConfig = {
          Type = "simple";
          Restart = "always";
          RestartSec = "1";
          ExecStart = "${pkgs.canbridge.canbridge}/bin/canbridge --mode listen --addr 0.0.0.0:${toString devCfg.port} --iface ${devCfg.can-dev}";
        };
      };
    };

    # Generate all client services
    clientSetups = mapAttrs' mkClientSetup cfg.canbridge-cfg;
    clientServices = mapAttrs' mkClientService cfg.canbridge-cfg;
    allClientServices = clientSetups // clientServices;

    # Generate all server services
    serverSetups = mapAttrs' mkServerSetup cfg.canbridge-cfg;
    serverServices = mapAttrs' mkServerService cfg.canbridge-cfg;
    allServerServices = serverSetups // serverServices;

  in mkIf (cfg.client || cfg.server.enable) {
    # Add can-utils to system packages for testing with cansend/candump
    environment.systemPackages = [ pkgs.can-utils ];

    # Open firewall ports for server mode
    networking.firewall.allowedTCPPorts = lib.mkIf cfg.server.enable
      (map (devCfg: devCfg.port) (attrValues cfg.canbridge-cfg));

    systemd.services =
      (if cfg.client then allClientServices else {}) //
      (if cfg.server.enable then allServerServices else {});
  };
}
