# The 2Captcha browser extension as an unpacked Firefox add-on. `src` is the
# unpacked `.xpi` from addons.mozilla.org.
#
# Supported widgets in this version: image captchas, reCAPTCHA v2/v2
# invisible/v3/audio, GeeTest v3/v4, KeyCAPTCHA, Arkose Labs (FunCaptcha),
# Lemin, Yandex, Capy, Amazon WAF, Cloudflare Turnstile, MTCaptcha. hCaptcha
# support was removed upstream in 2024.
{
  lib,
  stdenvNoCC,
  jq,
  src,
  apiKeyPlaceholder ? "@TWOCAPTCHA_API_KEY@",
  # Widgets solved without a click on the extension’s button. reCAPTCHA v3 is
  # left out on purpose: it is invisible and would be billed on every page load.
  autoSolve ? [
    "autoSolveNormal"
    "autoSolveRecaptchaV2"
    "autoSolveInvisibleRecaptchaV2"
    "autoSolveGeetest"
    "autoSolveGeetest_v4"
    "autoSolveKeycaptcha"
    "autoSolveArkoselabs"
    "autoSolveLemin"
    "autoSolveYandex"
    "autoSolveCapyPuzzle"
    "autoSolveAmazonWaf"
    "autoSolveTurnstile"
    "autoSolveMTCaptcha"
  ],
}:
stdenvNoCC.mkDerivation {
  pname = "2captcha-solver";
  inherit (builtins.fromJSON (builtins.readFile "${src}/manifest.json")) version;

  inherit src;

  nativeBuildInputs = [jq];

  dontConfigure = true;
  dontBuild = true;

  # The defaults from `common/config.js` are merged under whatever the user
  # saved in `browser.storage.local`, so a fresh profile picks them up as is.
  # The API key is a placeholder here, filled in at runtime from the
  # environment, so that it never enters the Nix store.
  postPatch =
    ''
      substituteInPlace common/config.js \
        --replace-fail 'apiKey: null,' 'apiKey: "${apiKeyPlaceholder}",'
    ''
    + lib.concatMapStrings (key: ''
      substituteInPlace common/config.js \
        --replace-fail '${key}: false,' '${key}: true,'
    '')
    autoSolve
    + ''
      # Otherwise every fresh profile opens the options page in a new tab.
      jq 'del(.options_ui)' manifest.json >manifest.json.new
      mv manifest.json.new manifest.json

      # The AMO signature covers the original files only.
      rm -r META-INF
    '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    cp -r . $out/
    runHook postInstall
  '';

  passthru = {inherit apiKeyPlaceholder;};

  meta = {
    description = "2Captcha solver as an unpacked Firefox add-on";
    homepage = "https://github.com/rucaptcha/2captcha-solver";
    license = lib.licenses.mit;
    platforms = lib.platforms.all;
  };
}
