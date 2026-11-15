{ lib, stdenv, fetchurl
  # Must match the exact v8 pin of the workspace that links it; the default is
  # the pin in services/catalyst/Cargo.toml. The names differ from every
  # nixpkgs attribute, so callPackage does not fill them.
, v8Version ? "152.2.0"
, v8Hashes ? {
    x86_64-linux = "sha256-tmg+mvy3f72MssOs9F001SApqyFrMyUiyVF5wS8P7Tw=";
    aarch64-linux = "sha256-xAGWu/GCUy5YP2W32vAZb9uzNU5vrFmzHQU5wg+THz0=";
  }
}:
let
  archive = fetchurl {
    name = "librusty_v8-${v8Version}.a.gz";
    url = "https://github.com/denoland/rusty_v8/releases/download/v${v8Version}/librusty_v8_release_${stdenv.hostPlatform.rust.rustcTarget}.a.gz";
    hash = v8Hashes.${stdenv.hostPlatform.system};
    meta.sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
in stdenv.mkDerivation {
  pname = "librusty_v8";
  version = v8Version;
  dontUnpack = true;
  installPhase = ''gzip -dc ${archive} > "$out"'';
}
