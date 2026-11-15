{
  # Measurement only: the packages the Test Scenes job installs with apt, the protoc script and
  # the clang action, from the nixpkgs pin of the repository.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/f9a75f41be3a74eab55d3a7679d34ad6cc0da977";
  outputs = { nixpkgs, ... }:
    let pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in {
      devShells.x86_64-linux.explorer = pkgs.mkShell {
        packages = with pkgs; [
          pkg-config cmake ninja protobuf llvmPackages_21.clang curl
          alsa-lib udev openssl ffmpeg_6-headless glib libdrm libgbm libGL
          libx11 libxext libxfixes libxdamage libxrandr libxcomposite libxkbcommon
          xvfb-run mesa pulseaudio alsa-utils alsa-plugins
        ];
        LIBCLANG_PATH = "${pkgs.llvmPackages_21.libclang.lib}/lib";
      };
    };
}
