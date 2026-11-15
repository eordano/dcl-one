import importlib.util
import json
from pathlib import Path
import re
import tempfile
import unittest

import nginx_map

SPEC = importlib.util.spec_from_file_location("gateway", Path(__file__).parents[1] / "gateway.py")
gateway = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gateway)


class ConfigurationTest(unittest.TestCase):
    def test_rejects_configuration_injection_before_starting_nginx(self):
        bad = {
            "GATEWAY_DOMAIN": ["example.org;\ninclude /tmp/x", "*.example.org", "https://example.org", "a..b"],
            "GATEWAY_HOST_ALIASES": ["example.org;root /"],
            "GATEWAY_APIS_UPSTREAMS": ["http://apis:5210", "apis:0", "apis:65536", "apis:5210;"],
            "GATEWAY_SITES_UPSTREAMS": ["unix:/tmp/socket", "sites:5212/path"],
            "GATEWAY_ADMIN_CIDRS": ["allow all;", "example.org", "203.0.113.1/33", "fe80::1%eth0/128"],
            "GATEWAY_TRUSTED_PROXY_CIDRS": ["0.0.0.0/0;"],
            "GATEWAY_TRUSTED_SYNC_PEER_IPS": ["203.0.113.1;", "peer.example", "fe80::1%eth0"],
            "GATEWAY_RESOLVERS": ["127.0.0.1;include", "resolver.example", "fe80::1%eth0"],
            "GATEWAY_STORAGE_ROOT": ["relative", "/data;include", "/data\nroot"],
        }
        for key, values in bad.items():
            for value in values:
                with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                    gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1", key: value})

    def test_admin_and_forwarded_identity_are_closed_by_default(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        self.assertEqual(cfg["admins"], [])
        self.assertEqual(cfg["trusted"], [])
        self.assertEqual(cfg["storage"], "")

    def test_operator_paths_keep_the_telemetry_dash_gate(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        text = gateway.render({"sites": [], "internal": [], "node": [], "aliases": [], "deploy": []},
                              cfg, "/mime.types")
        maps = nginx_map.parse(text)
        gated = ["/admin/api/jobs", "/server.data", "/metabase/", "/telemetry/dash/sql",
                 "/telemetry/dash/SQL.data", "/telemetry/dash/flag", "/telemetry/dash/group/target.data",
                 "/telemetry/dash/admin", "/telemetry/dash/admin.data", "/telemetry/dash/admin/users",
                 "/rpc/admin/rpc/config", "/v1/admin/names/approve-mana", "/shop-api/v1/admin/names/approve-mana",
                 "/shop-api/admin/campaigns/1/collections", "/marketplace-api/v1/admin/audit",
                 "/transactions-api/v1/admin/relayer", "/credits/admin/flags", "/CREDITS/Admin.data",
                 "/social-rpc/admin/x", "/places/api/admin/x", "/events/v1/admin/x",
                 "/market/v1/admin/audit", "/camera-reel/admin/images/1", "/scene-state/admin/scene/x/reset",
                 "/worlds-content-server/gc", "/WORLDS-CONTENT-SERVER/reprocess-ab"]
        public = ["/telemetry/dash", "/telemetry/dash/overview", "/telemetry/dash/sqlx",
                  "/telemetry/dash/administrator", "/telemetry", "/events/admin/pending-events",
                  "/events/admin/users", "/docs/creator/images/editor/admin/activate.webp",
                  "/credits/administrator", "/v1/moderation/communities", "/shop-api/v1/users"]
        for uri, expected in [(uri, "1") for uri in gated] + [(uri, "0") for uri in public]:
            with self.subTest(uri=uri):
                self.assertEqual(nginx_map.Request(maps, uri=uri)["operator_path"], expected)

    def test_canonicalizes_networks_and_supports_multiple_upstreams(self):
        cfg = gateway.settings({
            "GATEWAY_RESOLVERS": "::1,127.0.0.1",
            "GATEWAY_ADMIN_CIDRS": "203.0.113.17/24,2001:db8::1/64",
            "GATEWAY_APIS_UPSTREAMS": "apis-a:5210,[::1]:5211",
        })
        self.assertEqual(cfg["admins"], ["203.0.113.0/24", "2001:db8::/64"])
        self.assertEqual(cfg["apis"], ["apis-a:5210", "[::1]:5211"])
        self.assertEqual(cfg["resolvers"], "[::1] 127.0.0.1")

    def test_sync_peers_get_an_uncounted_limit_key(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1",
                                "GATEWAY_TRUSTED_SYNC_PEER_IPS": "203.0.113.7, 2001:db8::1"})
        self.assertEqual(cfg["sync_peers"], ["203.0.113.7/32", "2001:db8::1/128"])
        text = gateway.render({"sites": [], "internal": [], "node": [], "aliases": [], "deploy": []},
                              cfg, "/mime.types")
        self.assertIn("geo $sync_peer { default 0; 203.0.113.7/32 1; 2001:db8::1/128 1; }", text)
        self.assertIn("limit_req_zone $binary_remote_addr zone=read:10m rate=100r/s;", text)
        self.assertIn("limit_req_zone $content_limit_key zone=content:10m rate=100r/s;", text)
        key_map = re.search(r'map "\$sync_peer:\$request_method:\$uri" \$content_limit_key \{(.*?)\}', text, re.S)[1]
        exempt = re.search(r'"(~[^"]+)" ""', key_map)[1][1:]
        # Upstream exempts sync peers on GET /contents/{hash} only.
        for subject in ["1:GET:/content/contents/bafy1", "1:HEAD:/contents/bafy1", "1:GET:/contents/bafy1/"]:
            with self.subTest(subject=subject):
                self.assertTrue(re.search(exempt, subject))
        for subject in ["0:GET:/content/contents/bafy1", "1:POST:/content/contents/bafy1",
                        "1:GET:/content/entities/active", "1:GET:/content/contents/bafy1/active-entities",
                        "1:GET:/worlds-content-server/contents/bafy1", "1:GET:/lambdas/profiles"]:
            with self.subTest(subject=subject):
                self.assertIsNone(re.search(exempt, subject))
        self.assertIn("limit_req_zone $binary_remote_addr zone=deploy:10m rate=5r/s;", text)

    def test_uses_container_dns_without_a_fixed_host_address(self):
        with tempfile.TemporaryDirectory() as folder:
            resolv = Path(folder) / "resolv.conf"
            resolv.write_text("nameserver 127.0.0.1\nnameserver ::1\n")
            self.assertEqual(gateway.settings({}, resolv)["resolvers"], "127.0.0.1 [::1]")

    def test_sites_routes_keep_api_exceptions_and_prefix_boundaries(self):
        self.assertEqual(gateway.site_prefixes([
            "_index.tsx", "$slug.tsx", "places_.$id.tsx", "api.screens.v1.play.ts",
            "api.marketplace-screen.ts", "internal.pair.tsx", "profile.$id.tsx",
        ]), ["/api/marketplace-screen", "/api/screens", "/internal", "/places", "/profile"])

    def test_sites_cannot_claim_internal_api_paths(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        for name in ["metrics.ts", "content.ts", "ws.ts"]:
            table = {"sites": [name], "internal": ["/metrics", "/internal/pulse"]}
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "reserved"):
                gateway.render(table, cfg, "/mime.types")

    def test_cast_page_is_exact_and_leaves_the_cast_api_prefix(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        table = {"sites": ["cast._index.tsx"], "internal": ["/metrics"], "node": [], "deploy": [],
                 "aliases": [{"key": "^~ /cast/", "to": "/comms-gatekeeper$uri"}]}
        text = gateway.render(table, cfg, "/mime.types")
        self.assertIn("location = /cast {", text)
        self.assertEqual(text.count("location ^~ /cast/ {"), 1)
        self.assertNotIn("location /cast/ {", text)

    def test_worlds_host_deploys_get_the_deploy_body_cap(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        table = {"sites": [], "internal": ["/metrics"], "node": [], "aliases": [],
                 "deploy": ["/entities", "/content/entities"]}
        text = gateway.render(table, cfg, "/mime.types")
        deploy = re.search(r'location ~\* "([^"]+)" \{\n[^}]*client_max_body_size 248m;', text)[1]
        for path in ["/entities", "/content/entities", "/content/Entities", "/ENTITIES/"]:
            with self.subTest(path=path):
                # ~* matches ignoring case, as the content server's rewrite does.
                self.assertTrue(re.search(deploy, path, re.IGNORECASE))
        self.assertIsNone(re.search(deploy, "/worlds-content-server/entities", re.IGNORECASE))
        worlds = re.search(r'location ~ "([^"]+)" \{\n[^}]*client_max_body_size 360m; '
                           r'client_body_timeout 300s; limit_req zone=deploy', text)[1]
        for path in ["/worlds-content-server/entities", "/worlds-content-server/entities/"]:
            with self.subTest(path=path):
                self.assertTrue(re.search(worlds, path))
        for path in ["/worlds-content-server/entities/active", "/entities"]:
            with self.subTest(path=path):
                self.assertIsNone(re.search(worlds, path))

    def test_worlds_deploy_limits_answer_the_services_json(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        table = {"sites": [], "internal": ["/metrics"], "node": [], "aliases": [],
                 "deploy": ["/entities", "/content/entities"]}
        text = gateway.render(table, cfg, "/mime.types")
        worlds = re.search(r'location ~ "\^/worlds-content-server/entities/\*\$" \{\n(.*?)\n\}', text, re.S)[1]
        # limit_conn is raised as an internal 503 so its 429 can carry its own text.
        self.assertIn("limit_req zone=deploy burst=4 nodelay;", worlds)
        self.assertIn("limit_conn_status 503;", worlds)
        self.assertIn("error_page 413 = @worlds_deploy_too_large; error_page 429 = @worlds_deploy_rate;\n"
                      "error_page 503 = @worlds_deploy_busy;", worlds)
        self.assertNotIn("proxy_intercept_errors", text)
        self.assertIsNone(re.search(r"^\s*charset ", text, re.M))
        expected = {
            "@worlds_deploy_too_large": (413, None, "Payload Too Large",
                                         "The multipart request is too large."),
            "@worlds_deploy_rate": (429, "1", "Too Many Requests",
                                    "Too many deploy requests from this client, please retry shortly."),
            "@worlds_deploy_busy": (429, "5", "Too Many Requests",
                                    "Too many uploads in progress from this client, please retry shortly."),
        }
        for name, (status, retry, error, message) in expected.items():
            with self.subTest(location=name):
                body = re.search(rf"location {name} \{{\n(.*?)\n\}}", text, re.S)[1]
                # The app's deploy answers are axum Json: application/json, no charset.
                self.assertIn("default_type application/json;", body)
                for header in ["X-Content-Type-Options nosniff",
                               "Referrer-Policy strict-origin-when-cross-origin",
                               "Cross-Origin-Opener-Policy $coop_fallback",
                               "Access-Control-Allow-Origin $cors_fallback"]:
                    self.assertIn(f"add_header {header} always;", body)
                if retry:
                    self.assertIn(f"add_header Retry-After {retry} always;", body)
                else:
                    self.assertNotIn("Retry-After", body)
                answer = re.search(r"return (\d+) '(.*)';", body)
                self.assertEqual(int(answer[1]), status)
                self.assertEqual(json.loads(answer[2]), {"error": error, "message": message})

    def test_worlds_deploy_texts_are_the_services(self):
        source = (Path(__file__).parents[3] / "services/catalyst/worlds/src/core/upload_limits.rs").read_text()
        texts = {body["message"] for _, _, _, body in gateway.WORLDS_DEPLOY_ERRORS}
        self.assertIn('"The multipart request is too large."', source)
        self.assertIn('"Too many uploads in progress from this client, please retry shortly."', source)
        self.assertTrue({"The multipart request is too large.",
                         "Too many uploads in progress from this client, please retry shortly."} <= texts)

    def test_active_entities_take_upstreams_ten_mib_body_cap(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        table = {"sites": [], "internal": ["/metrics"], "node": [], "aliases": [],
                 "deploy": ["/entities", "/content/entities"]}
        text = gateway.render(table, cfg, "/mime.types")
        active = re.search(r'location ~\* "([^"]+)" \{\n[^}]*client_max_body_size 10m;', text)[1]
        for path in ["/content/entities/active", "/content/Entities/Active", "/entities/active/"]:
            with self.subTest(path=path):
                self.assertTrue(re.search(active, path, re.IGNORECASE))
        for path in ["/content/entities/active/collections/urn", "/content/entities/scene"]:
            with self.subTest(path=path):
                self.assertIsNone(re.search(active, path, re.IGNORECASE))

    def test_worlds_caps_match_the_service_wire_limits(self):
        worlds = Path(__file__).parents[3] / "services/catalyst/worlds/src/core"

        def mib(path, name):
            source = (worlds / path).read_text()
            expr = re.search(rf"const {name}: usize = ([^;]+);", source)[1]
            for ref, value in re.findall(r"const (\w+): usize = ([^;]+);", source):
                expr = expr.replace(ref, f"({value})")
            self.assertRegex(expr, r"^[\d\s()*+]+$")
            total = eval(expr)
            self.assertEqual(total % (1024 * 1024), 0, name)
            return f"{total // (1024 * 1024)}m"

        self.assertEqual(gateway.WORLDS_DEPLOY_CAP, mib("deploy.rs", "MAX_UPLOAD_WIRE_SIZE_BYTES"))
        self.assertEqual(gateway.WORLDS_SETTINGS_CAP, mib("settings.rs", "MAX_SETTINGS_UPLOAD_WIRE_BYTES"))

    def test_badges_backfill_cap_matches_the_route_limit(self):
        source = (Path(__file__).parents[3] / "services/catalyst/social/src/badges/mod.rs").read_text()
        expr = re.search(r"const MAX_BACKFILL_BODY_BYTES: usize = ([^;]+);", source)[1]
        self.assertRegex(expr, r"^[\d\s*]+$")
        self.assertEqual(gateway.BADGES_BACKFILL_CAP, f"{eval(expr) // (1024 * 1024)}m")
        _, regexes = self.worlds_locations()
        for path in ["/badges/badges/halloweeks_2026/backfill", "/badges/badges/traveler/backfill"]:
            with self.subTest(path=path):
                body = next(body for regex, body in regexes if regex.search(path))
                self.assertIn(f"client_max_body_size {gateway.BADGES_BACKFILL_CAP};", body)
                self.assertIn(f"limit_conn deploy_uploads {gateway.DEPLOY_CONNECTIONS_PER_ADDRESS};", body)
        for path in ["/badges/badges/traveler", "/badges/users/0xab/badges/traveler",
                     "/badges/badges/x/backfill/y", "/x/badges/badges/x/backfill"]:
            with self.subTest(path=path):
                self.assertFalse(any("64m" in body for regex, body in regexes if regex.search(path)))

    def test_the_content_deploy_cap_reaches_the_apps_route_limit(self):
        root = Path(__file__).parents[3] / "services/catalyst"
        source = (root / "content/src/create_entity.rs").read_text()
        consts = dict(re.findall(r"const (\w+): usize = ([^;]+);", source))

        def value(name):
            expr = consts[name]
            for ref in re.findall(r"[A-Z_]{2,}", expr):
                expr = expr.replace(ref, f"({value(ref)})")
            self.assertRegex(expr, r"^[\d\s()*+]+$")
            return eval(expr)

        formula = re.search(r"fn max_deployment_body_bytes\(max_fields: usize\) -> usize \{\s*([^}]+?)\s*\}",
                            source)[1]
        self.assertEqual(formula, "MAX_DEPLOYMENT_BYTES + (MAX_DEPLOY_FILES + max_fields + 1) * "
                                  "MULTIPART_PART_FRAMING_BYTES")
        config = (root / "config/src/content.rs").read_text()
        fields = eval(re.search(r'env = "MAX_UPLOAD_FIELD_COUNT", default_value_t = ([\d\s*+]+)\)', config)[1])
        route_limit = (value("MAX_DEPLOYMENT_BYTES")
                       + (value("MAX_DEPLOY_FILES") + fields + 1) * value("MULTIPART_PART_FRAMING_BYTES"))
        cap = int(gateway.CONTENT_DEPLOY_CAP.removesuffix("m")) * 1024 * 1024
        self.assertGreaterEqual(cap, route_limit)
        self.assertLess(cap - route_limit, 1024 * 1024, "rounded up to the next MiB")

    def test_deploy_routes_cap_concurrent_uploads_per_address(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        table = {"sites": [], "internal": ["/metrics"], "node": [], "aliases": [],
                 "deploy": ["/entities", "/content/entities"]}
        text = gateway.render(table, cfg, "/mime.types")
        self.assertIn("limit_conn_zone $binary_remote_addr zone=deploy_uploads:10m;", text)
        self.assertIn("limit_conn_status 429;", text)
        bodies = [m[1] for m in re.finditer(r'location ~\*? "[^"]+" \{\n(.*?)\n\}', text, re.S)
                  if "zone=deploy " in m[1]]
        self.assertEqual(len(bodies), 2)
        for body in bodies:
            self.assertIn(f"limit_conn deploy_uploads {gateway.DEPLOY_CONNECTIONS_PER_ADDRESS};", body)

    def test_protected_storage_keeps_the_apps_content_headers(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1", "GATEWAY_STORAGE_ROOT": "/content"})
        table = {"sites": [], "internal": ["/metrics"], "node": [], "aliases": [], "deploy": ["/entities"]}
        text = gateway.render(table, cfg, "/mime.types")
        body = re.search(r"location \^~ /__protected_storage/ \{\n(.*?)\n\}", text, re.S)[1]
        # The app's Cache-Control passes through the redirect; a second one would double it.
        self.assertNotIn("Cache-Control", body)
        self.assertIn("add_header Access-Control-Expose-Headers "
                      "$upstream_http_access_control_expose_headers always;", body)
        # Ranges the app does not answer itself get the whole file, as upstream.
        self.assertIn("max_ranges 0;", body)

    def test_report_evidence_cap_matches_the_route_limit(self):
        root = Path(__file__).parents[3] / "services/catalyst"
        source = (root / "comms/src/gatekeeper/reports.rs").read_text()
        expr = re.search(r"const MAX_FILE_BYTES: usize = ([^;]+);", source)[1]
        self.assertRegex(expr, r"^[\d\s*]+$")
        self.assertEqual(gateway.REPORT_EVIDENCE_CAP, f"{eval(expr) // (1024 * 1024)}m")
        text, regexes = self.worlds_locations()
        report = "6f1b4f8a-6a1e-4c2f-9b3d-0d9a2c7b1e55"
        for path in [f"/report-user/api/reports/players/{report}/evidence/0-shot.png",
                     f"/report-user/api/reports/{report}/evidence/0-shot.data"]:
            with self.subTest(path=path):
                body = next(body for regex, body in regexes if regex.search(path))
                self.assertIn("proxy_pass http://apis;", body)
                self.assertIn(f"client_max_body_size {gateway.REPORT_EVIDENCE_CAP};", body)
                self.assertIn("limit_conn deploy_uploads", body)
        for path in ["/report-user/api/reports/players", "/report-user/api/reports/players/presign",
                     f"/report-user/api/reports/players/{report}/evidence/0-shot.png/more",
                     f"/report-user/api/reports/{report}/evidence/",
                     f"/comms-gatekeeper/reports/players/{report}/evidence/0-shot.png",
                     f"/x/report-user/api/reports/players/{report}/evidence/0-shot.png"]:
            with self.subTest(path=path):
                self.assertFalse(any(gateway.REPORT_EVIDENCE_CAP in body
                                     for regex, body in regexes if regex.search(path)))
        self.assertNotIn("location /report-user", text)

    def worlds_locations(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1", "GATEWAY_DOMAIN": "example.test"})
        table = {"sites": [], "internal": ["/metrics"], "node": [], "aliases": [],
                 "deploy": ["/entities", "/content/entities"]}
        text = gateway.render(table, cfg, "/mime.types")
        return text, [(re.compile(m[1].strip('"')), m[2]) for m in
                      re.finditer(r'location ~ ("[^"]+"|\S+) \{\n(.*?)\n\}', text, re.S)]

    def test_worlds_settings_get_the_service_settings_cap(self):
        _, regexes = self.worlds_locations()
        for path in ["/world/w.dcl.eth/settings", "/worlds-content-server/world/w.dcl.eth/settings",
                     "/worlds-content-server/world/w.dcl.eth/settings/"]:
            with self.subTest(path=path):
                body = next(body for regex, body in regexes if regex.search(path))
                self.assertIn("client_max_body_size 12m;", body)
        for path in ["/world/w.dcl.eth/settings/x", "/x/world/w.dcl.eth/settings", "/world/settings"]:
            with self.subTest(path=path):
                self.assertFalse(any("12m" in body for regex, body in regexes if regex.search(path)))

    def test_worlds_reads_are_not_rate_limited_but_writes_and_admin_are(self):
        # Upstream worlds-content-server limits no route by rate (WR-37).
        text, regexes = self.worlds_locations()
        self.assertIn("limit_req_zone $worlds_limit_key zone=worlds:10m rate=30r/s;", text)
        key_map = re.search(r'map "\$request_method:\$uri" \$worlds_limit_key \{(.*?)\}', text, re.S)[1]
        rules = [(re.compile(m[1][1:]), m[2]) for m in re.finditer(r'"(~[^"]+)" (\S+);', key_map)]

        def key(subject):
            return next((value for regex, value in rules if regex.search(subject)), "$binary_remote_addr")
        for subject in ["GET:/worlds-content-server/status", "HEAD:/world/w.dcl.eth/about",
                        "OPTIONS:/worlds-content-server/world/w.dcl.eth/permissions", "GET:/index"]:
            with self.subTest(subject=subject):
                self.assertEqual(key(subject), '""')
        for subject in ["POST:/worlds-content-server/world/w.dcl.eth/permissions/deployment",
                        "DELETE:/worlds-content-server/entities/w.dcl.eth", "POST:/world/w.dcl.eth/scenes",
                        "GET:/worlds-content-server/admin/worlds", "POST:/worlds-content-server/gc",
                        "GET:/worlds-content-server/reprocess-ab"]:
            with self.subTest(subject=subject):
                self.assertEqual(key(subject), "$binary_remote_addr")
        exact = {key: re.search(r"\nlocation " + re.escape(key) + r" \{\n(.*?)\n\}", text, re.S)[1]
                 for key in ["= /worlds-content-server", "/worlds-content-server/", "= /index", "= /worlds",
                             "= /worlds/status"]}
        for name, body in exact.items():
            with self.subTest(location=name):
                self.assertIn("limit_req zone=worlds burst=60 nodelay;", body)
        for path in ["/worlds-content-server/contents/bafy", "/worlds-content-server/ipfs/bafy",
                     "/worlds-content-server/world/w.dcl.eth/settings", "/world/w.dcl.eth/settings",
                     "/world/w.dcl.eth/about"]:
            with self.subTest(path=path):
                body = next(body for regex, body in regexes if regex.search(path))
                self.assertIn("limit_req zone=worlds burst=60 nodelay;", body)
                self.assertNotIn("zone=content", body)
        settings = next(body for regex, body in regexes if regex.search("/world/w.dcl.eth/settings"))
        self.assertIn("limit_conn worlds_uploads 10;", settings)
        deploy = next(body for regex, body in regexes if regex.search("/worlds-content-server/entities"))
        self.assertIn("limit_req zone=deploy burst=4 nodelay;", deploy)

    def test_the_deploy_zone_sits_above_the_content_servers_burst_limit(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        table = {"sites": [], "internal": ["/metrics"], "node": [], "aliases": [],
                 "deploy": ["/entities"]}
        text = gateway.render(table, cfg, "/mime.types")
        rate = int(re.search(r"zone=deploy:10m rate=(\d+)r/s;", text)[1])
        burst = int(re.search(r"limit_req zone=deploy burst=(\d+) nodelay;", text)[1])
        # The content server answers 200 per 60 s per client itself (JSON 429).
        self.assertGreaterEqual(burst, 200)
        self.assertGreater(rate * 60, 200)

    def test_a_webhook_route_takes_its_own_rate_zone(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        table = {"sites": [], "internal": [], "aliases": [], "deploy": [],
                 "node": [{"path": "/events-notifier", "probe": "/events-notifier/forward", "webhook": True},
                          {"path": "/places", "probe": "/places"}]}
        text = gateway.render(table, cfg, "/mime.types")
        # Segment's deliveries, one POST per event, above the sink's 280 inserts/s sizing.
        rate = int(re.search(r"zone=webhook:10m rate=(\d+)r/s;", text)[1])
        self.assertGreaterEqual(rate, 280)
        for key in ["= /events-notifier", "^~ /events-notifier/"]:
            body = re.search(r"location " + re.escape(key) + r" \{\n(.*?)\n\}", text, re.S)[1]
            self.assertIn("limit_req zone=webhook burst=", body)
        body = re.search(r"location \^~ /places/ \{\n(.*?)\n\}", text, re.S)[1]
        self.assertNotIn("zone=webhook", body)

    def test_social_cards_keep_exact_backend_routes_without_shadowing_discovery_pages(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        table = {"sites": ["places.tsx", "places_.place.$position.tsx", "places_.world.$name.tsx"],
                 "internal": [], "deploy": ["/entities"], "aliases": [],
                 "node": [{"path": "/places/api"}, {"path": "/places/place", "exact": True},
                          {"path": "/places/world", "exact": True}]}
        config = gateway.render(table, cfg, "/mime.types")
        for path in ["/places/place", "/places/world"]:
            self.assertIn(f"location = {path} {{\nproxy_pass http://apis;", config)
            self.assertIn(f"location = {path}/ {{\nproxy_pass http://apis;", config)
            self.assertNotIn(f"location ^~ {path}/", config)
        self.assertIn("location ^~ /places/api/ {\nproxy_pass http://apis;", config)
        self.assertIn("location /places/ {\nproxy_pass http://sites;", config)

    def test_legal_documents_route_to_sites_without_claiming_api_paths(self):
        root = Path(__file__).parents[3]
        legal_fixtures = root / "apps/sites/src/data/legal/fixtures"
        canonical_paths = {
            json.loads(path.read_text())["path"] for path in legal_fixtures.glob("*.json")
        }
        public_paths = canonical_paths | {"/content-policy", "/rewards", "/referral"}
        # The route table production renders: every Sites route file, and no node row for
        # /credits, which reaches the APIs through `location /`.
        sites = sorted(path.name for path in (root / "apps/sites/src/routes/routes").iterdir())
        table = {"sites": sites, "internal": [], "aliases": [], "node": [], "deploy": []}
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        config = gateway.render(table, cfg, "/mime.types")

        for path in public_paths - {"/content"}:
            with self.subTest(path=path):
                self.assertIn(f"location = {path} {{\nproxy_pass http://sites;", config)
        self.assertIn("location = /content {\nproxy_pass http://$content_root;", config)
        self.assertIn("location /content/ {\nproxy_pass http://apis;", config)
        self.assertIn("location / {\nproxy_pass http://apis;", config)
        sites_locations = re.findall(r"location (= |\^~ )?(/\S*) \{\nproxy_pass http://sites;", config)
        for path in ("/credits", "/credits/", "/credits/seasons"):
            with self.subTest(path=path):
                self.assertEqual([], [
                    (kind, prefix) for kind, prefix in sites_locations
                    if (path == prefix if kind == "= " else path.startswith(prefix)) and prefix != "/"
                ])

    def test_content_page_is_sites_only_on_the_apex_host(self):
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1", "GATEWAY_DOMAIN": "example.test",
                                "GATEWAY_HOST_ALIASES": "www.example.test"})
        table = {"sites": [], "internal": [], "aliases": [], "node": [], "deploy": []}
        maps = nginx_map.parse(gateway.render(table, cfg, "/mime.types"))
        root = lambda host: nginx_map.Request(maps, host=host)["content_root"]
        self.assertEqual(root("example.test"), "sites")
        self.assertEqual(root("www.example.test"), "sites")
        for host in ("peer.example.test", "worlds.example.test", "content.example.test"):
            with self.subTest(host=host):
                self.assertEqual(root(host), "apis")

    def test_bundled_event_image_uses_api_without_claiming_other_event_assets(self):
        path = "/events/assets/event-default.webp"
        routes = (Path(__file__).parents[1] / "routes.nix").read_text()
        self.assertRegex(routes, r'path = "/events/assets/event-default\.webp";\s+probe = "/events/assets/event-default\.webp";\s+exact = true;')
        cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})
        table = {"sites": ["events.tsx"], "internal": [], "deploy": [], "aliases": [],
                 "node": [{"path": path, "exact": True}]}
        config = gateway.render(table, cfg, "/mime.types")
        self.assertIn(f"location = {path} {{\nproxy_pass http://apis;", config)
        self.assertNotIn("location ^~ /events/assets/", config)
        self.assertIn("location /events/ {\nproxy_pass http://sites;", config)
