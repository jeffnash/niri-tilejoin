# SPDX-License-Identifier: GPL-3.0-or-later
{
  description = "niri with joined tiled panels and multi-monitor display groups";

  inputs = {
    niri.url = "github:niri-wm/niri/feb3e43f1475e0865bb89cbd1e898b34d1d2ccf6";
    nixpkgs.follows = "niri/nixpkgs";
  };

  outputs =
    { niri, nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      buildPatchFiles = [
        ./integration/niri/patches/0001-feat-tilejoin-add-niri-extension-configuration.patch
        ./integration/niri/patches/0002-feat-tilejoin-integrate-the-build-time-output-extens.patch
        ./integration/niri/patches/0003-chore-deps-refresh-audited-transitive-dependencies.patch
      ];
      packages = builtins.listToAttrs (map (system: {
        name = system;
        value =
          let
            pkgs = nixpkgs.legacyPackages.${system};
          in
          {
            niri-tilejoin = niri.packages.${system}.niri.overrideAttrs (old: {
              pname = "niri-tilejoin";
              # The upstream package source filters out wiki documentation, so the
              # documentation-only fourth patch is shipped but not applied here.
              patches = (old.patches or [ ]) ++ buildPatchFiles;
              # Cargo dependencies are evaluated before `patches` run. Import the exact
              # post-remediation lock so Nix vendors the dependency graph the build sees.
              cargoDeps = pkgs.rustPlatform.importCargoLock {
                lockFile = ./integration/niri/Cargo.lock;
                allowBuiltinFetchGit = true;
              };
              postPatch = (old.postPatch or "") + ''
                cp -R ${./extension/niri-tiled} niri-tiled
                chmod -R u+w niri-tiled
              '';
            });
            default = packages.${system}.niri-tilejoin;
          };
      }) systems);
    in
    {
      inherit packages;
      apps = builtins.mapAttrs (_: packageSet: {
        type = "app";
        program = "${packageSet.niri-tilejoin}/bin/niri";
      }) packages;
    };
}
