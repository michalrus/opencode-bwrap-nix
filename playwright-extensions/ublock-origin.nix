# uBlock Origin as an unpacked Firefox add-on, prepared for
# `programs.opencode-bwrap.playwright.extensions`. `src` is the unpacked
# `.xpi` from the GitHub release. Firefox still supports the full manifest v2
# blocking API that uBlock Origin needs.
{
  lib,
  stdenvNoCC,
  src,
}:
stdenvNoCC.mkDerivation {
  pname = "ublock-origin";
  inherit (builtins.fromJSON (builtins.readFile "${src}/manifest.json")) version;

  inherit src;

  dontConfigure = true;
  dontBuild = true;

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    cp -r . $out/
    runHook postInstall
  '';

  meta = {
    description = "uBlock Origin as an unpacked Firefox add-on";
    homepage = "https://github.com/gorhill/uBlock";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.all;
  };
}
