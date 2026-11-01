{
  description = "Entroximer: a Discord audio engine in Rust, with a TypeScript client";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # the engine relies on Linux (/proc, Unix sockets and file locks); Discord bots run there
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        entroximer = pkgs.callPackage ./nix/package.nix { };
        default = entroximer;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.callPackage ./nix/shell.nix { };
      });

      # building the package runs the engine's test suite
      checks = forAllSystems (pkgs: {
        inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) entroximer;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
