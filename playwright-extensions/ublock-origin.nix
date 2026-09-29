# uBlock Origin (manifest v2) as an unpacked Chromium extension, prepared for
# `programs.opencode-bwrap.playwright.extensions`. Current Chromium refuses
# manifest v2 unless started with `--enable-features=AllowLegacyMV2Extensions`,
# which only exempts unpacked extensions; `playwright.allowManifestV2` adds it.
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
    description = "uBlock Origin as an unpacked Chromium extension";
    homepage = "https://github.com/gorhill/uBlock";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.all;
  };
}
