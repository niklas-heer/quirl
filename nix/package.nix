{
  lib,
  rustPlatform,
  versionCheckHook,
}:

let
  workspace = (lib.importTOML ../Cargo.toml).workspace.package;
in
rustPlatform.buildRustPackage {
  pname = "quirl";
  inherit (workspace) version;

  src = lib.cleanSource ../.;

  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [
    "--package"
    "quirl-cli"
  ];

  # CI runs the full suite; the package build only proves the binary starts.
  doCheck = false;
  doInstallCheck = true;
  nativeInstallCheckInputs = [ versionCheckHook ];

  meta = {
    description = "A well-stirred shell with Bash-familiar commands, typed data pipelines, and a Lua extension SDK";
    homepage = workspace.repository;
    license = lib.licenses.mit;
    mainProgram = "quirl";
    platforms = lib.platforms.unix;
  };
}
