{ pkgs, catalyst, sites, gateway, revision }:
let
  inherit (pkgs) lib;
  accounts = pkgs.runCommand "foundation-container-accounts" { } ''
    mkdir -p "$out/etc"
    printf 'root:x:0:0:root:/root:/sbin/nologin\nfoundation:x:1000:1000:Foundation:/data:/sbin/nologin\n' > "$out/etc/passwd"
    printf 'root:x:0:\nfoundation:x:1000:\n' > "$out/etc/group"
    printf 'hosts: files dns\npasswd: files\ngroup: files\n' > "$out/etc/nsswitch.conf"
  '';
  # Sites for a deployment whose Shop settles credits through dcl-one's own CreditsManager on
  # Polygon (services/catalyst/shop/src/policy_entry.rs DCL_ONE_POLYGON_MANAGER); the plain
  # `sites` keeps the registry's manager. Defined here, not in infra/packages, so the plain
  # build and the Bevy artifact's inputs stay as they are.
  sitesPolygonCredits = sites.override {
    shopSettlement = { chainId = 137; creditsManager = "0xe21289022b9324f9b9145a91ee59297ceb040c06"; };
  };
  labels = {
    "org.opencontainers.image.source" = "https://github.com/decentraland/dcl-one";
    "org.opencontainers.image.revision" = revision;
    "org.opencontainers.image.licenses" = "AGPL-3.0";
  };
  layered = build: name: package: config: build {
    inherit name;
    tag = "nix";
    contents = [ package pkgs.cacert accounts ];
    # Give each runtime store path its own stable layer, including Sites'
    # production dependencies and docs. Avoid fromImage: dockerTools would
    # include the runtime closure again on top of that base image.
    maxLayers = 120;
    extraCommands = ''mkdir -p tmp data; chmod 1777 tmp'';
    fakeRootCommands = ''chown 1000:1000 data'';
    config = {
      User = "1000:1000";
      WorkingDir = "/data";
      Env = [ "HOME=/data" "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt" ];
      Labels = labels;
    } // config;
  };
  # The published archive, and the same layers written to stdout for `docker load`: a pull
  # request's smoke test has no archive to compress, copy back or upload.
  image = layered pkgs.dockerTools.buildLayeredImage;
  stream = layered pkgs.dockerTools.streamLayeredImage;
  sitesConfig = package: {
    Entrypoint = [ "${lib.getExe package}" ];
    Env = [
      "HOME=/data" "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
      "PATH=${lib.makeBinPath [ pkgs.nodejs-slim_26 ]}:/bin"
      "HOST=0.0.0.0" "PORT=5212" "NODE_ENV=production" "SITES_STATE_DIR=/data"
    ];
    ExposedPorts."5212/tcp" = { };
  };
  apis = build: build "foundation-apis" catalyst {
    Entrypoint = [ "${lib.getExe catalyst}" ];
    Cmd = [ "serve" ];
    Env = [
      "HOME=/data" "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
      "HTTP_SERVER_HOST=0.0.0.0" "HTTP_SERVER_PORT=5210"
      "STORAGE_ROOT_FOLDER=/data/content" "AB_CDN_CACHE_DIR=/data/ab-cdn"
      "PROFILE_IMAGES_CACHE_DIR=/data/profile-images" "BVIMPOSTERS_STORE_ROOT=/data/imposters"
    ];
    ExposedPorts."5210/tcp" = { };
  };
  web = build: build "foundation-sites" sites (sitesConfig sites);
  # The same Sites image built for dcl-one's Polygon CreditsManager (`sitesPolygonCredits` above),
  # labelled with the settlement it carries.
  webPolygonCredits = build: build "foundation-sites-polygon-credits" sitesPolygonCredits
    (sitesConfig sitesPolygonCredits // {
      Labels = labels // {
        "org.decentraland.shop.settlement-chain-id" = toString sitesPolygonCredits.shopSettlement.chainId;
        "org.decentraland.shop.credits-manager" = sitesPolygonCredits.shopSettlement.creditsManager;
      };
    });
in {
  image-gateway = image "foundation-gateway" gateway {
    Entrypoint = [ "${lib.getExe gateway}" ];
    Env = [
      "HOME=/data" "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
      "PATH=${lib.makeBinPath [ pkgs.python3 pkgs.nginx ]}"
    ];
    ExposedPorts."8080/tcp" = { };
    StopSignal = "SIGQUIT";
  };
  image-apis = apis image;
  image-sites = web image;
  image-sites-polygon-credits = webPolygonCredits image;
  image-apis-stream = apis stream;
  image-sites-stream = web stream;
  image-sites-polygon-credits-stream = webPolygonCredits stream;
  # Its compiled app, saved with the plain one in the `sites-app` image cache (tooling/image-cache.py).
  sites-app-polygon-credits = sitesPolygonCredits.app;
}
