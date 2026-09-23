{
  description = "A well-stirred shell with Bash-familiar commands, typed data pipelines, and a Lua extension SDK";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
    in
    {
      packages = forAllSystems (system: rec {
        quirl = nixpkgs.legacyPackages.${system}.callPackage ./nix/package.nix { };
        default = quirl;
      });

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
