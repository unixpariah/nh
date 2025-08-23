{ pkgs, ... }:
{
  programs.bat.enable = true;
  home.shellAliases.less = "${pkgs.bat}/bin/bat";
}
