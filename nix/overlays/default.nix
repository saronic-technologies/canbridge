{ self, ... }:
{
  overlays = {
    default = final: _prev: {
      canbridge = self.packages.${final.stdenv.hostPlatform.system};
    };
  };
}
