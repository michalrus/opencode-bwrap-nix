# uBlock Origin (manifest v2) as an unpacked Chromium extension, prepared for
# `programs.opencode-bwrap.playwright.extensions`. Current Chromium refuses
# manifest v2 unless started with `--enable-features=AllowLegacyMV2Extensions`,
# which only exempts unpacked extensions; `playwright.allowManifestV2` adds it.
{
  lib,
  stdenvNoCC,
  fetchzip,
}:
stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "ublock-origin";
  version = "1.75.0";

  src = fetchzip {
    url = "https://github.com/gorhill/uBlock/releases/download/${finalAttrs.version}/uBlock0_${finalAttrs.version}.chromium.zip";
    hash = "sha256-i4IiHZGuA2lZCt6zLiVxvktrT6+ITD6nBpZtvcnF6Qg=";
    stripRoot = false;
  };

  dontConfigure = true;
  dontBuild = true;

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    cp -r uBlock0.chromium/. $out/
    runHook postInstall
  '';

  meta = {
    description = "uBlock Origin as an unpacked Chromium extension";
    homepage = "https://github.com/gorhill/uBlock";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.all;
  };
})
