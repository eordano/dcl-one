{ lib, stdenv, runCommand, linkFarm, fetchPnpmDeps, pnpmConfigHook, pnpm, nodejs_26, nodejs-slim_26, makeWrapper, playWeb, shopSettlement ? null }:
let
  nodejs = nodejs_26;
  pnpm' = pnpm.override { nodejs-slim = nodejs-slim_26; };
  # Keep every output together in the binary cache. Restoring only slim's runtime
  # forces a partial Node rebuild whose libv8 output check still references out.
  toolchain = linkFarm "foundation-sites-toolchain" (lib.concatMap (package:
    map (output: {
      name = "${package.pname}-${output}";
      path = package.${output};
    }) package.outputs
  ) [ nodejs nodejs-slim_26 ]);
  root = ../..;
  sites = root + /apps/sites;
  ui = root + /apps/ui3;
  version = (builtins.fromJSON (builtins.readFile (sites + /package.json))).version;
  depsSrc = lib.fileset.toSource {
    root = sites;
    fileset = lib.fileset.unions (map (p: sites + p) [ /package.json /pnpm-lock.yaml /.npmrc /pnpm-workspace.yaml ]);
  };
  pnpmDeps = fetchPnpmDeps {
    pname = "foundation-sites";
    pnpm = pnpm';
    src = depsSrc;
    fetcherVersion = 4;
    hash = "sha256-4rTu6NSjLrRt5ylJ5VGNSwoh97h6uZGXHWF7MBHf3/Y=";
  };
  runtimeDeps = stdenv.mkDerivation {
    pname = "foundation-sites-runtime-deps";
    inherit version pnpmDeps;
    src = depsSrc;
    nativeBuildInputs = [ nodejs-slim_26 pnpmConfigHook pnpm' ];
    pnpmInstallFlags = [ "--prod" ];
    dontBuild = true;
    installPhase = ''
      test -e node_modules/.bin/react-router-serve
      mkdir -p "$out"
      cp -a node_modules "$out/node_modules"
    '';
  };
  docsContent = runCommand "foundation-sites-docs" {
    src = lib.fileset.toSource { root = sites + /content/docs; fileset = sites + /content/docs; };
  } ''
    if grep -rIl '^version https://git-lfs.github.com/spec/v1$' "$src"; then
      echo "Docs media contains Git LFS pointers. Run git lfs pull and build git+file:.?lfs=1#image-sites." >&2
      exit 1
    fi
    test -s "$src/manifest.json"
    mkdir -p "$out"
    cp -a "$src/." "$out/"
  '';
  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (sites + /src) (sites + /scripts)
      (sites + /package.json) (sites + /pnpm-lock.yaml) (sites + /.npmrc) (sites + /pnpm-workspace.yaml)
      (sites + /vite.config.ts) (sites + /react-router.config.ts) (sites + /tsconfig.json)
      (ui + /src) (ui + /public) (ui + /scripts/gen-error-pages.mts)
      (ui + /vite.validate.js) (ui + /package.json)
      (root + /library/catalyst-api) (root + /library/shop)
      (root + /library/ui/index.ts)
      (root + /library/ui/css-modules.d.ts)
      (root + /library/ui/components)
      (lib.fileset.maybeMissing (root + /library/ui/package.json))
      (lib.fileset.maybeMissing (root + /library/ui/tsconfig.json))
      (lib.fileset.maybeMissing (root + /library/ui/foundations))
      (lib.fileset.maybeMissing (root + /library/ui/apps/sites))
      (root + /library/ui/apps/hud) (root + /library/explorer-core)
      (root + /library/ui/text-styles.ts)
    ];
  };
  # A build whose Shop settles credits on one chain through one CreditsManager: the two VITE_*
  # the Shop reads at build time (apps/ui3/src/shop/checkout/chain/config.ts), which apply only to
  # the chain named here. Null is the plain build, unchanged.
  settlement =
    if shopSettlement == null then null
    else
      assert lib.assertMsg (builtins.elem shopSettlement.chainId [ 137 80002 ])
        "shopSettlement.chainId must be 137 (Polygon) or 80002 (Amoy)";
      assert lib.assertMsg (builtins.match "0x[0-9a-f]{40}" shopSettlement.creditsManager != null)
        "shopSettlement.creditsManager must be a lowercase 0x-prefixed 20-byte address";
      shopSettlement;
  settlementSuffix = lib.optionalString (settlement != null) "-chain${toString settlement.chainId}-credits";
  # The compiled server and client alone: neither the Explorer build nor the docs media
  # is an input, so a new Bevy artifact or docs sync leaves this derivation, its cache
  # identity and its image layer unchanged.
  app = stdenv.mkDerivation ({
    pname = "foundation-sites-app${settlementSuffix}";
    inherit version src pnpmDeps;
    sourceRoot = "${src.name}/apps/sites";
    nativeBuildInputs = [ nodejs pnpmConfigHook pnpm' ];
    env = {
      NODE_OPTIONS = "--max-old-space-size=4096";
    } // lib.optionalAttrs (settlement != null) {
      VITE_SHOP_CHAIN_ID = toString settlement.chainId;
      VITE_SHOP_CREDITS_MANAGER = settlement.creditsManager;
    };
    buildPhase = ''
      runHook preBuild
      chmod -R u+w ..
      ln -s "$PWD/node_modules" ../node_modules
      if grep -rIl '^version https://git-lfs.github.com/spec/v1$' ../../library/ui/apps/sites/assets ../../library/ui/foundations/typography/fonts; then
        echo "Sites media or shared fonts contain Git LFS pointers. Run git lfs pull and build git+file:.?lfs=1#image-sites." >&2
        exit 1
      else
        status=$?
        if [ "$status" -ne 1 ]; then
          echo "Unable to inspect Sites media and shared fonts." >&2
          exit "$status"
        fi
      fi
      pnpm run build
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      mkdir -p "$out/src/data"
      cp -r build "$out/build"
      cp package.json "$out/package.json"
      ln -s ${runtimeDeps}/node_modules "$out/node_modules"
      cp -r src/data/fixtures "$out/src/data/fixtures"
      test -s "$out/build/server/index.js"
      runHook postInstall
    '';
  } // lib.optionalAttrs (settlement != null) {
    # Names the settlement the build carries next to the build it shaped, and fails the build if the
    # client bundle doesn't carry the manager: the VITE_* never reached it.
    postInstall = ''
      if ! grep -rqF ${settlement.creditsManager} "$out/build/client"; then
        echo "the client bundle doesn't carry VITE_SHOP_CREDITS_MANAGER (${settlement.creditsManager})" >&2
        exit 1
      fi
      printf '{"chainId":%s,"creditsManager":"%s"}\n' ${toString settlement.chainId} ${settlement.creditsManager} > "$out/shop-settlement.json"
    '';
  });
in runCommand "foundation-sites${settlementSuffix}-${version}" {
  nativeBuildInputs = [ makeWrapper ];
  passthru = { inherit app pnpmDeps runtimeDeps docsContent playWeb toolchain; shopSettlement = settlement; };
  meta.mainProgram = "sites-server";
} ''
  # Assemble the server from its stable parts. Each stays its own store path, so the
  # image keeps one layer per part and only the changed one is rebuilt and transferred.
  shopt -s dotglob nullglob
  mkdir -p "$out/bin" "$out/build/client/play" "$out/content"
  for entry in ${app}/*; do
    [ "''${entry##*/}" = build ] || ln -s "$entry" "$out/"
  done
  ln -s ${app}/build/server "$out/build/server"
  for entry in ${app}/build/client/*; do
    [ "''${entry##*/}" = play ] || ln -s "$entry" "$out/build/client/"
  done
  # /play/ holds the client's own static files (play/assets, play/favicon) next to
  # /play/engine, /play/bridge-scene and /play/service_worker.js from our Bevy build.
  for entry in ${app}/build/client/play/* ${playWeb}/*; do
    if [ -e "$out/build/client/play/''${entry##*/}" ]; then
      echo "/play/''${entry##*/} exists in both the Sites client and the Explorer build" >&2
      exit 1
    fi
    ln -s "$entry" "$out/build/client/play/"
  done
  ln -s ${docsContent} "$out/content/docs"
  # react-router-serve resolves the client directory beside the server path it is given,
  # without following symlinks, so the assembled tree serves the Explorer files.
  makeWrapper ${nodejs-slim_26}/bin/node "$out/bin/sites-server" \
    --add-flags "$out/node_modules/@react-router/serve/bin.cjs $out/build/server/index.js" \
    --set-default SITES_STATE_DIR /data \
    --set PLAY_WEB_DIR "$out/build/client/play" \
    --chdir "$out"
  test -s "$out/build/server/index.js"
  test -s "$out/build/client/play/sites-play.json"
  test -s "$out/content/docs/manifest.json"
''
