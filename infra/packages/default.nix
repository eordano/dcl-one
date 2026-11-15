{ pkgs, craneLib, revision }:
let
  catalyst = import ./catalyst.nix { inherit pkgs craneLib; };
  scene-cli = import ./scene-cli.nix { inherit pkgs craneLib; };
  playWeb = pkgs.callPackage ./play.nix { };
  sites = pkgs.callPackage ./sites.nix { inherit playWeb; };
  gateway = pkgs.callPackage ../gateway/package.nix { };
in {
  inherit catalyst scene-cli sites gateway;
  catalyst-deps = catalyst.cargoArtifacts;
  catalyst-vendor = catalyst.cargoVendorDir;
  sites-app = sites.app;
  sites-deps = sites.pnpmDeps;
  sites-runtime-deps = sites.runtimeDeps;
  sites-toolchain = sites.toolchain;
  image-tools = pkgs.buildEnv {
    name = "foundation-image-tools";
    paths = [ pkgs.skopeo pkgs.jq pkgs.python3 pkgs.curl ];
  };
} // import ./tools.nix { inherit pkgs; }
  // import ../containers/images.nix { inherit pkgs catalyst sites gateway revision; }
