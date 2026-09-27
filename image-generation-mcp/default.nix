{
  lib,
  pkgs,
  ...
}:
pkgs.rustPlatform.buildRustPackage {
  pname = "image-generation-mcp";
  version = "0.1.0";
  src = lib.sources.sourceFilesBySuffices (lib.cleanSource ./.) [".rs" "Cargo.toml" "Cargo.lock" "guidance.md"];
  cargoHash = "sha256-toWi7AEp8e+JtJSUx190e9dP07bEiBu1QXUzN5BO72o=";

  meta = {
    description = "Image generation and editing MCP for CLIProxyAPI with model discovery and art-direction guidance";
    license = lib.licenses.asl20;
    platforms = lib.platforms.linux;
    mainProgram = "image-generation-mcp";
  };
}
