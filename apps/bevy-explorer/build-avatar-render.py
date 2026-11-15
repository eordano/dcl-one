#!/usr/bin/env python3
"""Build the engine's headless avatar renderer and leave it, wrapped, at dist/avatar_render.

The node's profile images (social::profile_images, PROFILE_IMAGES_AVATAR_RENDER_BIN) run it to render
the face and body of profiles whose snapshot files the node does not hold. The node runs with a clean
environment, so the wrapper carries the library path and Vulkan settings of the shell this ran in: the
GPU driver's, on NixOS under /run/opengl-driver as well.

The wrapper holds absolute paths of the host that builds it: the binary in the source tree's target
directory and that shell's library directories. It runs on that host only. dist/ is ignored by Git;
build the wrapper again on each host and after a change of the build shell, and do not copy it.
"""
import os
import shlex
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
SOURCE = ROOT / 'rendering/bevy-explorer/source'
DIST = HERE / 'dist'
# The bin's own feature (media decoding, the scene inspector and the social service client, no HUD).
FEATURES = 'avatar_render'
CARRIED = ('LD_LIBRARY_PATH', 'VK_ICD_FILENAMES', 'VK_DRIVER_FILES', 'XDG_DATA_DIRS', 'WGPU_BACKEND')
DRIVER_LIBS = Path('/run/opengl-driver/lib')
# where NixOS keeps the Vulkan ICD manifests the loader looks up through XDG_DATA_DIRS
DRIVER_SHARE = Path('/run/opengl-driver/share')


def environment():
    env = {name: os.environ[name] for name in CARRIED if os.environ.get(name)}
    for name, directory in (('LD_LIBRARY_PATH', DRIVER_LIBS), ('XDG_DATA_DIRS', DRIVER_SHARE)):
        if directory.is_dir():
            env[name] = ':'.join(p for p in (env.get(name, ''), str(directory)) if p)
    env.setdefault('WGPU_BACKEND', 'vulkan')
    return env


def main():
    # Cargo reads Bevy and the forked crates from trees that are not tracked: write the ones that
    # are missing or stale.
    subprocess.run((sys.executable, str(ROOT / 'tooling/patched-sources.py'), 'materialise', '--quiet'), check=True)
    # The renderer starts the scene sidecar that is beside its binary, as the headless engine does.
    subprocess.run(['cargo', 'build', '--release', '--locked', '-p', 'dcl_deno_ipc'], cwd=SOURCE, check=True)
    subprocess.run(['cargo', 'build', '--release', '--locked', '--no-default-features', f'--features={FEATURES}',
                    '--bin', 'avatar_render'], cwd=SOURCE, check=True)
    binary = SOURCE / 'target/release/avatar_render'
    DIST.mkdir(parents=True, exist_ok=True)
    wrapper = DIST / 'avatar_render'
    exports = ''.join(f'export {name}={shlex.quote(value)}\n' for name, value in environment().items())
    wrapper.write_text(f'#!/usr/bin/env bash\n{exports}exec {shlex.quote(str(binary))} "$@"\n')
    wrapper.chmod(0o755)
    print(wrapper)


if __name__ == '__main__':
    main()
