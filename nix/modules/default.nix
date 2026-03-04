{ ... }@inputs:
{
  nixosModules = {
    canbridge = import ./canbridge.nix inputs;
  };
}
