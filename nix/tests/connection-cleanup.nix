{ self, pkgs, crane, rust-overlay, mode }:
let
  rustPkgs = import pkgs.path {
    inherit (pkgs.stdenv.hostPlatform) system;
    overlays = [ (import rust-overlay) ];
  };
  craneLib = (crane.mkLib rustPkgs).overrideToolchain (p: p.rust-bin.stable.latest.default);
  endpoint = craneLib.buildPackage {
    src = craneLib.cleanCargoSource self;
    cargoExtraArgs = "--locked --tests";
    doCheck = false;
    installPhaseCommand = ''
      mkdir -p $out/bin
      find target/release/deps -maxdepth 1 -type f -executable -name 'connection_cleanup-*' \
        -exec cp {} $out/bin/connection-cleanup \;
    '';
  };
in
pkgs.testers.runNixOSTest {
  name = "canbridge-cleanup-${mode}";
  nodes.machine = { ... }: {
    boot.kernelModules = [ "vcan" ];
    # Only the regression may change the CAN link state.
    networking.dhcpcd.denyInterfaces = [ "vcan*" ];
    environment.systemPackages = [ pkgs.iproute2 endpoint ];
    systemd.services.canbridge-cleanup = {
      serviceConfig = {
        ExecStart = "${self.packages.${pkgs.stdenv.hostPlatform.system}.canbridge}/bin/canbridge --mode ${mode} --addr 127.0.0.1:4242 --iface vcan0";
        Restart = "always";
        RestartSec = "0.2";
      };
    };
  };
  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.succeed("ip link add vcan0 type vcan")
    machine.succeed("ip link set vcan0 up")
    with subtest("${mode}: idle CAN failure closes TCP and recovers"):
        try:
            print(machine.succeed(
                "connection-cleanup --ignored --nocapture ${mode}_idle_can_failure",
                timeout=180,
            ))
        finally:
            print(machine.succeed("journalctl -u canbridge-cleanup --no-pager"))
  '';
}
