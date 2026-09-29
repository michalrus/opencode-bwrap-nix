# uBlock Origin Lite (manifest v3) as an unpacked Chromium extension, prepared
# for `programs.opencode-bwrap.playwright.extensions`. The classic uBlock Origin
# is manifest v2 and no longer loads into current Chromium.
{
  lib,
  stdenvNoCC,
  fetchzip,
}:
stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "ublock-origin-lite";
  version = "2026.926.2202";

  src = fetchzip {
    url = "https://github.com/uBlockOrigin/uBOL-home/releases/download/${finalAttrs.version}/uBOLite_${finalAttrs.version}.chromium.zip";
    hash = "sha256-i/JMXBLXi2P5SQ9Fz0VsfmnVW44aAl2Sa9j/FtmtUKs=";
    stripRoot = false;
  };

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
})
