{
  pkgs,
  lib,
  ...
}:
{
  nix.settings = {
    substituters = lib.mkForce [ ];
    hashed-mirrors = null;
    connect-timeout = 1;
    experimental-features = [
      "nix-command"
      "flakes"
    ];
  };

  environment.systemPackages = [ pkgs.hello ];

  system.includeBuildDependencies = true;

  virtualisation = {
    cores = 2;
    memorySize = 4096;
  };
}
