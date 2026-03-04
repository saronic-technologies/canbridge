{ self, nixpkgs, flake-utils, ... }:
flake-utils.lib.eachDefaultSystem (system:
  let
    overlays = [ self.overlays.default ];
    pkgs = import nixpkgs {
      inherit system overlays;
    };
  in
    with pkgs;
    {
      packages.canbridge-test = testers.runNixOSTest {
        name = "canbridge-test";
        nodes = let
          canbridge-config = {
            can0 = {
              vcan-dev = "vcan0";
              can-dev = "can0";
              port = 4242;
            };
            can1 = {
              vcan-dev = "vcan1";
              can-dev = "can1";
              port = 4243;
            };
          };
        in {
          client = { config, pkgs, ... }:
            {
              imports = [
                self.nixosModules.canbridge
              ];
              config = {
                users.users.test = {
                  password = "";
                  isNormalUser = true;
                  extraGroups = [ "wheel" ];
                };

                systemd.services.candump-vcan0 = {
                  description = "candump vcan0 (test)";
                  wantedBy = [ "multi-user.target" ];
                  after = [ "network-online.target" ];
                  serviceConfig = {
                    Type = "simple";
                    Restart = "always";
                    RestartSec = "0.2";
                    # Log to a file we can grep in the test.
                    ExecStart = "${pkgs.can-utils}/bin/candump vcan0";
                    StandardOutput = "append:/tmp/candump-vcan0.log";
                    StandardError  = "append:/tmp/candump-vcan0.log";
                  };
                };
                systemd.services.candump-vcan1 = {
                  description = "candump vcan1 (test)";
                  wantedBy = [ "multi-user.target" ];
                  after = [ "network-online.target" ];
                  serviceConfig = {
                    Type = "simple";
                    Restart = "always";
                    RestartSec = "0.2";
                    # Log to a file we can grep in the test.
                    ExecStart = "${pkgs.can-utils}/bin/candump vcan1";
                    StandardOutput = "append:/tmp/candump-vcan1.log";
                    StandardError  = "append:/tmp/candump-vcan1.log";
                  };
                };

                services.canbridge = {
                  client = true;
                  canbridge-cfg = canbridge-config;
                };
              };
            };
          server = { config, pkgs, ... }:
            {
              imports = [
                self.nixosModules.canbridge
              ];
              config = {
                users.users.test = {
                  password = "";
                  isNormalUser = true;
                  extraGroups = [ "wheel" ];
                };

                systemd.services.candump-can0 = {
                  description = "candump can0 (test)";
                  wantedBy = [ "multi-user.target" ];
                  after = [ "network-online.target" ];
                  serviceConfig = {
                    Type = "simple";
                    Restart = "always";
                    RestartSec = "0.2";
                    # Log to a file we can grep in the test.
                    ExecStart = "${pkgs.can-utils}/bin/candump can0";
                    StandardOutput = "append:/tmp/candump-can0.log";
                    StandardError  = "append:/tmp/candump-can0.log";
                  };
                };
                systemd.services.candump-can1 = {
                  description = "candump can1 (test)";
                  wantedBy = [ "multi-user.target" ];
                  after = [ "network-online.target" ];
                  serviceConfig = {
                    Type = "simple";
                    Restart = "always";
                    RestartSec = "0.2";
                    # Log to a file we can grep in the test.
                    ExecStart = "${pkgs.can-utils}/bin/candump can1";
                    StandardOutput = "append:/tmp/candump-can1.log";
                    StandardError  = "append:/tmp/candump-can1.log";
                  };
                };

                services.canbridge = {
                  server = {
                    enable = true;
                    # Since we are in a VM test the server isn't
                    # connecting to a physical CAN bus so we need
                    # canbridge to setup virtual CAN devs for it
                    virtualDevs = true;
                  };
                  canbridge-cfg = canbridge-config;
                };
              };
            };
        };

        testScript = let
          candump-can0-log = pkgs.writeTextFile {
            name = "candump-can0-log";
            text = builtins.readFile ./candump-can0.log;
          };

          candump-can1-log = pkgs.writeTextFile {
            name = "candump-can1-log";
            text = builtins.readFile ./candump-can1.log;
          };

          candump-vcan0-log = pkgs.writeTextFile {
            name = "candump-vcan0-log";
            text = builtins.readFile ./candump-vcan0.log;
          };

          candump-vcan1-log = pkgs.writeTextFile {
            name = "candump-vcan1-log";
            text = builtins.readFile ./candump-vcan1.log;
          };
          
        in ''
            import time
            # Make sure all services come up healthy
            client.wait_for_unit("canbridge-client-can0-setup.service") 
            client.wait_for_unit("canbridge-client-can0.service")
            client.wait_for_unit("canbridge-client-can1-setup.service") 
            client.wait_for_unit("canbridge-client-can1.service")

            server.wait_for_unit("canbridge-server-can0.service")
            server.wait_for_unit("canbridge-server-can0-setup.service")
            server.wait_for_unit("canbridge-server-can1.service")
            server.wait_for_unit("canbridge-server-can1-setup.service")

            client.wait_for_unit("candump-vcan0.service") 
            client.wait_for_unit("candump-vcan1.service")

            server.wait_for_unit("candump-can0.service") 
            server.wait_for_unit("candump-can1.service")

            frame = "123#DEADBEEF"
            fd_frame = "123##1000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F202122232425262728292A2B2C2D2E2F303132333435363738393A3B3C3D3E3F"
            fd_frame_no_brs = "123##0000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F202122232425262728292A2B2C2D2E2F303132333435363738393A3B3C3D3E3F"

            # Send normal CAN frames to each device
            server.succeed(f"cansend can0 {frame}", timeout=10)
            time.sleep(2)
            server.succeed(f"cansend can1 {frame}", timeout=10)
            time.sleep(2)
            client.succeed(f"cansend vcan0 {frame}", timeout=10)
            time.sleep(2)
            client.succeed(f"cansend vcan1 {frame}", timeout=10)
            time.sleep(2)

            # Send CAN-FD frames with BRS to each device
            server.succeed(f"cansend can0 {fd_frame}", timeout=10)
            time.sleep(2)
            server.succeed(f"cansend can1 {fd_frame}", timeout=10)
            time.sleep(2)
            client.succeed(f"cansend vcan0 {fd_frame}", timeout=10)
            time.sleep(2)
            client.succeed(f"cansend vcan1 {fd_frame}", timeout=10)
            time.sleep(2)

            # Send CAN-FD frames with no BRS to each device
            server.succeed(f"cansend can0 {fd_frame_no_brs}", timeout=10)
            time.sleep(2)
            server.succeed(f"cansend can1 {fd_frame_no_brs}", timeout=10)
            time.sleep(2)
            client.succeed(f"cansend vcan0 {fd_frame_no_brs}", timeout=10)
            time.sleep(2)
            client.succeed(f"cansend vcan1 {fd_frame_no_brs}", timeout=10)
            time.sleep(2)

            server.succeed("diff ${candump-can0-log} /tmp/candump-can0.log")
            server.succeed("diff ${candump-can1-log} /tmp/candump-can1.log")
            client.succeed("diff ${candump-vcan0-log} /tmp/candump-vcan0.log")
            client.succeed("diff ${candump-vcan1-log} /tmp/candump-vcan1.log")
        '';
      };
    }
)
