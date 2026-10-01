# Camoufox, a Firefox build that spoofs its fingerprint in C++ and speaks
# Playwright's Juggler protocol, from the upstream Linux release.
#
# The version comes from the tag of the `camoufox` flake input, so that it is
# updated with the other inputs. The release itself is fetched here rather than
# as a flake input: there is one zip of about 1.3 GB per system, and
# `nix flake lock` would download all of them. The zips' hashes must be updated
# with the version; the store name contains the version, so a stale hash fails
# the build instead of reusing the old zip.
#
# Only the interpreter of the executables is patched. Rewriting the RPATH of
# Mozilla's libraries (as `autoPatchelfHook` does) makes `libxul.so` crash in
# its initializers, so the libraries are found through `LD_LIBRARY_PATH`, as
# in nixpkgs' `firefox-bin`.
#
# The release bundles fonts for Linux, macOS, and Windows, stored once per set
# of systems that use them (`fonts/groups.json`), and a `fonts.conf` that
# names them relative to the working directory. Only the Linux groups are
# kept, and the wrapper points Fontconfig at them by absolute path, so pages
# see the fonts that go with a Linux user agent and no host fonts.
{
  lib,
  stdenv,
  fetchurl,
  unzip,
  jq,
  patchelf,
  makeWrapper,
  alsa-lib,
  atk,
  cairo,
  dbus,
  dbus-glib,
  ffmpeg,
  fontconfig,
  freetype,
  gdk-pixbuf,
  glib,
  gtk3,
  libGL,
  libdrm,
  libpulseaudio,
  libva,
  libxkbcommon,
  mesa,
  pango,
  pciutils,
  speechd-minimal,
  wayland,
  libx11,
  libxcb,
  libxcomposite,
  libxcursor,
  libxdamage,
  libxext,
  libxfixes,
  libxi,
  libxrandr,
  libxrender,
  libxtst,
  zlib,
}: let
  versionInputUrl = (lib.importJSON ../flake.lock).nodes.camoufox.original.url;
  version = lib.head (builtins.match ".*/refs/tags/v([^/]+)/README\\.md" versionInputUrl);

  releases = {
    x86_64-linux = {
      arch = "x86_64";
      hash = "sha256-cw1zEVOyoWysI4oLSn+EnATR/WGBwJ7GrylpWvBAevg=";
    };
    aarch64-linux = {
      arch = "arm64";
      hash = "sha256-h4N6YWgHIl64MoKWWNk5AK5XH6sZkK18b1Vntvmso+E=";
    };
  };

  release = releases.${stdenv.hostPlatform.system} or (throw "camoufox: unsupported system ${stdenv.hostPlatform.system}");

  libs = [
    alsa-lib
    atk
    cairo
    dbus
    dbus-glib
    ffmpeg
    fontconfig
    freetype
    gdk-pixbuf
    glib
    gtk3
    libGL
    libdrm
    libpulseaudio
    libva
    libxkbcommon
    mesa
    pango
    pciutils
    speechd-minimal
    stdenv.cc.cc.lib
    wayland
    libx11
    libxcomposite
    libxcursor
    libxdamage
    libxext
    libxfixes
    libxi
    libxrandr
    libxrender
    libxtst
    libxcb
    zlib
  ];
in
  stdenv.mkDerivation {
    pname = "camoufox";
    inherit version;

    src = fetchurl {
      name = "camoufox-${version}-lin.${release.arch}.zip";
      url = "https://github.com/daijro/camoufox/releases/download/v${version}/camoufox-${version}-lin.${release.arch}.zip";
      inherit (release) hash;
    };

    nativeBuildInputs = [unzip jq patchelf makeWrapper];

    # `unzip` exits with 1 on a warning: some bundled font names are not
    # valid in the archive's declared encoding.
    unpackPhase = ''
      runHook preUnpack
      mkdir source
      unzip -qq "$src" -d source || [ $? -eq 1 ]
      cd source
      runHook postUnpack
    '';

    dontConfigure = true;
    dontBuild = true;
    dontStrip = true;
    dontPatchELF = true;

    installPhase = ''
      runHook preInstall

      dir=$out/lib/camoufox
      mkdir -p "$dir" $out/bin
      cp -r . "$dir"

      for exe in camoufox camoufox-bin gfxtest; do
        patchelf --set-interpreter "$(cat $NIX_CC/nix-support/dynamic-linker)" "$dir/$exe"
      done

      mapfile -t linuxGroups < <(jq -r '.readBy.lin[]' "$dir/fonts/groups.json")
      for group in "$dir"/fonts/*/; do
        group=$(basename "$group")
        printf '%s\n' "''${linuxGroups[@]}" | grep -qxF "$group" || rm -r "$dir/fonts/$group"
      done
      fontDirs=$(printf '<dir>%s</dir>' "''${linuxGroups[@]/#/$dir/fonts/}")
      substitute "$dir/fontconfig/linux/fonts.conf" "$dir/fonts.conf" \
        --replace-fail '<dir prefix="cwd">fonts</dir>' "$fontDirs"
      rm -r "$dir/fontconfig"

      makeWrapper "$dir/camoufox-bin" $out/bin/camoufox \
        --prefix LD_LIBRARY_PATH : "$dir:${lib.makeLibraryPath libs}" \
        --set FONTCONFIG_FILE "$dir/fonts.conf"

      runHook postInstall
    '';

    meta = {
      description = "Firefox build with fingerprint spoofing, driven by Playwright";
      homepage = "https://github.com/daijro/camoufox";
      license = lib.licenses.mpl20;
      sourceProvenance = [lib.sourceTypes.binaryNativeCode];
      platforms = builtins.attrNames releases;
      mainProgram = "camoufox";
    };
  }
