{
  lib,
  pkgs,
  ...
}:
pkgs.writeShellApplication {
  name = "preamble-project-instructions-cmd";
  runtimeInputs = with pkgs; [
    coreutils
    gitMinimal
  ];
  text = builtins.readFile ./project-instructions.sh;
  meta = {
    description = "Loads the nearest project instructions for an OpenCode preamble";
    platforms = lib.platforms.all;
  };
}
