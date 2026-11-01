# For `nix-shell`; with flakes, `nix develop` opens the same shell. Uses the nixpkgs revision
# pinned in flake.lock, without needing flakes enabled or the files tracked by git.
let
  locked = (builtins.fromJSON (builtins.readFile ./flake.lock)).nodes.nixpkgs.locked;
  nixpkgs = fetchTarball {
    url = "https://github.com/${locked.owner}/${locked.repo}/archive/${locked.rev}.tar.gz";
    sha256 = locked.narHash;
  };
in
{
  pkgs ? import nixpkgs { },
}:
pkgs.callPackage ./nix/shell.nix { }
