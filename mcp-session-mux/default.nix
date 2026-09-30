{
  lib,
  pkgs,
  ...
}:
pkgs.rustPlatform.buildRustPackage {
  pname = "mcp-session-mux";
  version = "0.1.0";
  src = lib.sources.sourceFilesBySuffices (lib.cleanSource ./.) [".rs" "Cargo.toml" "Cargo.lock"];
  cargoHash = "sha256-IwNHTpzU3m8WfgUdqilyrl2fwJlpLl+Vol5wIOzWfYc=";

  meta = {
    description = "Stdio MCP proxy that runs one server process per session ID";
    license = lib.licenses.asl20;
    platforms = lib.platforms.linux;
    mainProgram = "mcp-session-mux";
  };
}
