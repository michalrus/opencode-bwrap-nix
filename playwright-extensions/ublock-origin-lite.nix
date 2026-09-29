# uBlock Origin Lite (manifest v3) as an unpacked Chromium extension, prepared
# for `programs.opencode-bwrap.playwright.extensions`. Loads without any
# Chromium switches, unlike the manifest v2 uBlock Origin in `ublock-origin.nix`.
{
  lib,
  stdenvNoCC,
  src,
}:
stdenvNoCC.mkDerivation {
  pname = "ublock-origin-lite";
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
    description = "uBlock Origin Lite as an unpacked Chromium extension";
    homepage = "https://github.com/uBlockOrigin/uBOL-home";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.all;
  };
}
