import importlib.util
from pathlib import Path
import re
import unittest

import nginx_map

SPEC = importlib.util.spec_from_file_location("gateway", Path(__file__).parents[1] / "gateway.py")
gateway = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gateway)

TABLE = {"sites": [], "internal": ["/internal/pulse", "/metrics"], "node": [], "aliases": [],
         "deploy": ["/entities", "/content/entities"]}


def route(host, request_uri):
    cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1", "GATEWAY_DOMAIN": "example.test"})
    return nginx_map.route(gateway.render(TABLE, cfg, "/mime.types"), host, request_uri)


class HostAliasTest(unittest.TestCase):
    def test_existing_aliases_keep_their_routes(self):
        self.assertEqual(route("abcdn.example.test", "/x"), ("/ab-cdn/x", "/x"))
        self.assertEqual(route("peer.example.test", "/about"), ("/about", "/about"))
        self.assertEqual(route("places.example.test", "/api/x"), ("/places/api/x", "/api/x"))
        self.assertEqual(route("gateway.example.test", "/peer/about?x=1"), ("/about", "/about?x=1"))
        self.assertEqual(route("world-storage.example.test", "/values?limit=10"),
                         ("/world-storage/values", "/values?limit=10"))

    def test_storage_subdomain_routes_to_world_storage(self):
        self.assertEqual(route("storage.example.test", "/values/k"), ("/world-storage/values/k", "/values/k"))

    def test_storage_gateway_prefix_routes_to_world_storage(self):
        self.assertEqual(route("gateway.example.test", "/storage/values/k"), ("/world-storage/values/k", "/values/k"))

    def test_storage_subdomain_does_not_double_prefix(self):
        self.assertEqual(route("storage.example.test", "/world-storage/values/k"),
                         ("/world-storage/values/k", "/world-storage/values/k"))

    def test_preview_gatekeeper_routes_to_comms_gatekeeper(self):
        self.assertEqual(route("comms-gatekeeper-local.example.test", "/get-scene-adapter"),
                         ("/comms-gatekeeper/get-scene-adapter", "/get-scene-adapter"))
        self.assertEqual(route("gateway.example.test", "/comms-gatekeeper-local/get-scene-adapter"),
                         ("/comms-gatekeeper/get-scene-adapter", "/get-scene-adapter"))

    def test_badges_host_carries_upstreams_whole_surface(self):
        # <node>/badges is the badges base: upstream's own /badges/... paths
        # are prefixed on this host too. The node root aliases only the
        # wallet reads (root-aliases.nix).
        for path in ["/badges", "/badges/traveler", "/badges/traveler/tiers",
                     "/badges/halloweeks_2026/backfill", "/categories", "/status",
                     "/users/0x1234567890abcdef1234567890abcdef12345678/preview"]:
            with self.subTest(path=path):
                self.assertEqual(route("badges.example.test", path), ("/badges" + path, path))
                self.assertEqual(route("gateway.example.test", "/badges" + path), ("/badges" + path, path))

    def test_badges_host_keeps_already_prefixed_paths(self):
        # A second segment the badges router mounts is taken as the /badges
        # mount, never as a badge id.
        for path in ["/badges/badges", "/badges/badges/traveler/tiers", "/badges/categories",
                     "/badges/status", "/badges/assets/traveler/2d.png",
                     "/badges/users/0x1234567890abcdef1234567890abcdef12345678/badges"]:
            with self.subTest(path=path):
                self.assertEqual(route("badges.example.test", path), (path, path))

    def test_badges_mounts_match_the_badges_router(self):
        router = (Path(__file__).parents[3] / "services/catalyst/social/src/badges/mod.rs").read_text()
        mounts = set(re.findall(r'\.route\(\s*"/([^/"]+)', router))
        self.assertEqual(mounts, set(gateway.BADGES_MOUNTS))

    def test_worlds_host_reaches_the_prefixed_worlds_routes(self):
        for path in ["/world/w.dcl.eth/settings", "/livekit-webhook", "/health/live",
                     "/contents/bafy", "/entities"]:
            with self.subTest(path=path):
                self.assertEqual(route("worlds-content-server.example.test", path),
                                 ("/worlds-content-server" + path, path))


class AccountHostTest(unittest.TestCase):
    def test_account_host_reaches_the_sites_confirmation_pages(self):
        root = Path(__file__).parents[3]
        sites = sorted(path.name for path in (root / "apps/sites/src/routes/routes").iterdir())
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1", "GATEWAY_DOMAIN": "example.test"})
        config = gateway.render({**TABLE, "sites": sites}, cfg, "/mime.types")
        uri = "/confirm-email-challenge/abc?address=0x1&source=account"
        self.assertEqual(nginx_map.route(config, "account.example.test", uri),
                         ("/account/confirm-email-challenge/abc", uri))
        self.assertIn("location /account/ {\nproxy_pass http://sites;", config)
        # The confirm request itself stays the node's on the apex host.
        self.assertEqual(nginx_map.route(config, "example.test", "/notifications/confirm-email")[0],
                         "/notifications/confirm-email")
