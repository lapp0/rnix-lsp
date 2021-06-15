{
  description = "A work-in-progress language server for Nix, with syntax checking and basic completion";

  inputs = {
    naersk.url = "github:nmattia/naersk";
    utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, utils, naersk, rust-overlay }:
    utils.lib.eachDefaultSystem (system:
      let
        overlays = [
          rust-overlay.overlay
          (self: super:
            {
              rustc = self.latest.rustChannels.nightly.rust;
              cargo = self.latest.rustChannels.nightly.rust;
            }
          )
        ];
        pkgs = import nixpkgs { inherit overlays system; };
        naersk-lib = pkgs.callPackage naersk {};
      in
      rec {
        packages.rnix-lsp = naersk-lib.buildPackage {
          pname = "rnix-lsp";
          root = ./.;
        };
        defaultPackage = packages.rnix-lsp;

        devShell = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            gitAndTools.pre-commit
          ];
        };

        apps.rnix-lsp = utils.lib.mkApp {
          drv = packages.rnix-lsp;
        };
        defaultApp = apps.rnix-lsp;
      });
}
