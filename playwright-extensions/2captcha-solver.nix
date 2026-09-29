# The 2Captcha browser extension as an unpacked Chromium extension. `src` is
# the CRX3 package from the Chrome Web Store: the GitHub releases and Git tags
# lag several versions behind.
#
# Supported widgets in this version: image captchas, reCAPTCHA v2/v2
# invisible/v3/audio, GeeTest v3/v4, KeyCAPTCHA, Arkose Labs (FunCaptcha),
# Lemin, Yandex, Capy, Amazon WAF, Cloudflare Turnstile, MTCaptcha, CaptchaFox.
# hCaptcha support was removed upstream in 2024.
{
  lib,
  stdenvNoCC,
  runCommandLocal,
  unzip,
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
    "autoSolveCaptchaFox"
  ],
}: let
  # A CRX3 file is a 12-byte header (magic, format version, little-endian
  # header length), the signed header, then a plain zip. Chromium refuses to
  # load an unpacked extension that contains the store-signed `_metadata`.
  unpacked = runCommandLocal "2captcha-solver-unpacked" {nativeBuildInputs = [unzip];} ''
    [ "$(head -c 4 ${src})" = "Cr24" ]
    headerLength=$(od -An -tu4 -j8 -N4 ${src} | tr -d ' ')
    tail -c +$((13 + headerLength)) ${src} >extension.zip
    unzip -q extension.zip -d "$out"
    rm -r "$out/_metadata"
  '';
in
  stdenvNoCC.mkDerivation {
    pname = "2captcha-solver";
    inherit (builtins.fromJSON (builtins.readFile "${unpacked}/manifest.json")) version;

    src = unpacked;

    nativeBuildInputs = [jq];

    dontConfigure = true;
    dontBuild = true;

    # The defaults from `common/config.js` are merged under whatever the user
    # saved in `chrome.storage.local`, so a fresh profile picks them up as is.
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
      '';

    installPhase = ''
      runHook preInstall
      mkdir -p $out
      cp -r . $out/
      runHook postInstall
    '';

    passthru = {inherit apiKeyPlaceholder;};

    meta = {
      description = "2Captcha solver as an unpacked Chromium extension";
      homepage = "https://github.com/rucaptcha/2captcha-solver";
      license = lib.licenses.mit;
      platforms = lib.platforms.all;
    };
  }
