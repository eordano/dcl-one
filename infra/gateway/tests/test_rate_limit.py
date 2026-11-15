import importlib.util
import json
from pathlib import Path
import re
import unittest

import nginx_map

SPEC = importlib.util.spec_from_file_location("gateway", Path(__file__).parents[1] / "gateway.py")
gateway = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gateway)

TABLE = {"sites": [], "internal": ["/internal/pulse", "/metrics"], "node": [], "aliases": [],
         "deploy": ["/entities", "/content/entities"]}


def render(**env):
    cfg = gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1", "GATEWAY_DOMAIN": "example.test", **env})
    return gateway.render(TABLE, cfg, "/mime.types")


def location(text, key):
    match = re.search(r"\nlocation " + re.escape(key) + r" \{\n(.*?)\n\}", text, re.S)
    return match and match[1]


class StorageRateLimitTest(unittest.TestCase):
    def setUp(self):
        self.text = render()

    def test_storage_has_its_own_zone(self):
        body = location(self.text, "^~ /world-storage/")
        self.assertIsNotNone(body)
        self.assertIn("proxy_pass http://apis;", body)
        self.assertIn("limit_req zone=storage burst=400 nodelay;", body)
        self.assertNotIn("zone=read", body)
        self.assertIn("limit_req_zone $storage_limit_key zone=storage:10m rate=100r/s;", self.text)

    def test_every_storage_host_form_reaches_the_storage_location(self):
        for host, uri in [("example.test", "/world-storage/values/k"),
                          ("world-storage.example.test", "/values/k"),
                          ("world-storage.example.test", "/world-storage/values/k"),
                          ("gateway.example.test", "/world-storage/values/k")]:
            with self.subTest(host=host, uri=uri):
                self.assertEqual(nginx_map.route(self.text, host, uri)[0], "/world-storage/values/k")

    def test_anonymous_clients_are_limited_per_ip(self):
        key = nginx_map.Request(nginx_map.parse(self.text), storage_trusted="0", binary_remote_addr="198.51.100.7")
        self.assertEqual(key["storage_limit_key"], "198.51.100.7")

    def test_bare_storage_path_uses_the_same_budget(self):
        body = location(self.text, "= /world-storage")
        self.assertIsNotNone(body)
        self.assertIn("limit_req zone=storage burst=400 nodelay;", body)

    def test_writes_keep_the_lower_budget(self):
        for trusted in ["0", "1"]:
            for method in ["GET", "HEAD", "OPTIONS", "PUT", "DELETE", "POST"]:
                key = nginx_map.Request(nginx_map.parse(self.text), storage_trusted=trusted,
                                        request_method=method, binary_remote_addr="198.51.100.7")
                expected = "198.51.100.7" if trusted == "0" and method in ["PUT", "DELETE", "POST"] else ""
                self.assertEqual(key["storage_write_key"], expected, (trusted, method))
        self.assertIn("zone=storage_write:10m rate=30r/s", self.text)


class StorageTrustedCidrsTest(unittest.TestCase):
    def setUp(self):
        self.text = render(GATEWAY_STORAGE_TRUSTED_CIDRS="10.1.2.3/8, 2001:db8::1/64")

    def test_trusted_networks_are_canonicalized_into_the_geo(self):
        self.assertIn("geo $storage_trusted { default 0; 10.0.0.0/8 1; 2001:db8::/64 1; }", self.text)

    def test_trusted_sources_use_a_bounded_separate_zone(self):
        key = nginx_map.Request(nginx_map.parse(self.text), storage_trusted="1", binary_remote_addr="10.1.2.3")
        self.assertEqual(key["storage_limit_key"], "")
        self.assertEqual(key["storage_trusted_key"], "10.1.2.3")
        self.assertIn("zone=storage_trusted:10m rate=1000r/s", self.text)
        self.assertIn("limit_req zone=storage_trusted burst=1000 nodelay", self.text)

    def test_rejects_invalid_trusted_networks(self):
        for value in ["10.0.0.0/8;", "allow all", "example.org", "203.0.113.1/33"]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1", "GATEWAY_STORAGE_TRUSTED_CIDRS": value})

    def test_storage_trust_is_closed_by_default(self):
        self.assertEqual(gateway.settings({"GATEWAY_RESOLVERS": "127.0.0.1"})["storage_trusted"], [])

    def test_proxy_and_storage_networks_must_not_overlap(self):
        for proxy, caller in [("10.0.0.0/8", "10.2.0.0/16"),
                              ("10.2.0.0/16", "10.0.0.0/8"),
                              ("2001:db8::/32", "2001:db8:1::/48")]:
            with self.subTest(proxy=proxy, caller=caller), self.assertRaisesRegex(ValueError, "overlap"):
                render(GATEWAY_TRUSTED_PROXY_CIDRS=proxy, GATEWAY_STORAGE_TRUSTED_CIDRS=caller)

    def test_disjoint_networks_and_different_address_families_are_allowed(self):
        render(GATEWAY_TRUSTED_PROXY_CIDRS="10.0.0.0/8,2001:db8::/48",
               GATEWAY_STORAGE_TRUSTED_CIDRS="192.0.2.0/24,2001:db8:1::/48")


class StorageKeyEncodingTest(unittest.TestCase):
    """Encoded `/` and `?` in a storage key reach the service as sent."""

    def setUp(self):
        self.text = render()
        self.maps = nginx_map.parse(self.text)

    def request(self, host, request_uri):
        sub = host.removesuffix(".example.test") if host != "example.test" else ""
        uri = request_uri.split("?")[0].replace("%2F", "/").replace("%2f", "/").replace("%3F", "?").replace("%3f", "?")
        return nginx_map.Request(self.maps, host=host, request_uri=request_uri, uri=uri, gateway_subdomain=sub)

    def test_storage_hosts_forward_the_raw_path_under_the_prefix(self):
        for host, sent, forwarded in [
            ("storage.example.test", "/values/a%2Fb", "/world-storage/values/a%2Fb"),
            ("storage.example.test", "/values/a%3Fb?x=1", "/world-storage/values/a%3Fb?x=1"),
            ("world-storage.example.test", "/players/0xa/values/k%2F1", "/world-storage/players/0xa/values/k%2F1"),
            ("gateway.example.test", "/storage/values/a%2Fb", "/world-storage/values/a%2Fb"),
            ("storage.example.test", "/values/.hidden", "/world-storage/values/.hidden"),
        ]:
            with self.subTest(host=host, sent=sent):
                self.assertEqual(self.request(host, sent)["storage_raw_uri"], forwarded)

    def test_other_requests_keep_the_normalized_rewrite(self):
        for host, sent in [
            ("storage.example.test", "/world-storage/values/a%2Fb"),
            ("storage.example.test", "/values/../admin"),
            ("storage.example.test", "/values/%2E%2e/admin"),
            ("storage.example.test", "/values/./k"),
            ("storage.example.test", "/values//k"),
            ("example.test", "/world-storage/values/a%2Fb"),
            ("gateway.example.test", "/world-storage/values/a%2Fb"),
            ("peer.example.test", "/values/a%2Fb"),
        ]:
            with self.subTest(host=host, sent=sent):
                self.assertEqual(self.request(host, sent)["storage_raw_uri"], "")

    def test_the_raw_form_is_reached_only_by_the_server_rewrite(self):
        server = self.text[self.text.index("server_name example.test"):]
        self.assertLess(server.index("if ($storage_raw_uri) { rewrite ^ /__gateway_storage_raw last; }"),
                        server.index("if ($ambiguous_path) { return 400; }"))
        body = location(self.text, "= /__gateway_storage_raw")
        self.assertTrue(body.startswith("internal;\n"))
        self.assertIn("proxy_pass http://apis$storage_raw_uri;", body)
        self.assertIn("limit_req zone=storage burst=400 nodelay;", body)

    def test_an_encoded_question_mark_is_refused_only_where_a_rewrite_would_split_it(self):
        for host, sent, refused in [
            ("example.test", "/world-storage/values/a%3Fb", 0),
            ("gateway.example.test", "/world-storage/values/a%3Fb", 0),
            ("example.test", "/a%3Fb", 1),
            ("foo.example.test", "/world-storage/a%3Fb", 1),
            ("gateway.example.test", "/peer/a%3fb?x=1", 1),
            ("example.test", "/about?x=a%3Fb", 0),
        ]:
            with self.subTest(host=host, sent=sent):
                self.assertEqual(self.request(host, sent)["ambiguous_path"], str(refused))

    def test_storage_bodies_take_their_cap_and_a_json_413(self):
        for key in ["= /world-storage", "^~ /world-storage/", "= /__gateway_storage_raw"]:
            with self.subTest(key=key):
                body = location(self.text, key)
                self.assertIn(f"client_max_body_size {gateway.STORAGE_BODY_CAP_BYTES};", body)
                self.assertIn("error_page 413 = @storage_too_large;", body)
        named = location(self.text, "@storage_too_large")
        self.assertIn("default_type application/json;", named)
        answer = re.search(r"return (\d+) '(.*)';", named)
        self.assertEqual(answer[1], "413")
        self.assertEqual(json.loads(answer[2]), {
            "error": "Payload Too Large",
            "message": "Request body exceeds the maximum allowed size"})

    def test_storage_uploads_are_capped_per_address(self):
        self.assertIn("limit_conn_zone $storage_write_key zone=storage_uploads:10m;", self.text)
        for key in ["= /world-storage", "^~ /world-storage/", "= /__gateway_storage_raw"]:
            with self.subTest(key=key):
                self.assertIn(f"limit_conn storage_uploads {gateway.STORAGE_UPLOADS_PER_ADDRESS};",
                              location(self.text, key))

    def test_the_storage_cap_is_the_largest_default_route_maximum_plus_headroom(self):
        root = Path(__file__).parents[3] / "services/catalyst"
        config = (root / "config/src/worlds.rs").read_text()
        caps = [int(v.replace("_", "")) for v in re.findall(
            r'env = "(?:WORLD|PLAYER|ENV)_STORAGE_MAX_VALUE_SIZE_BYTES",\s*default_value_t = ([\d_]+)', config)]
        self.assertEqual(len(caps), 3)
        storage = root / "worlds/src/storage"
        slack = int(re.search(r"BODY_ENVELOPE_SLACK_BYTES: i64 = ([\d_]+);",
                              (storage / "handlers.rs").read_text())[1].replace("_", ""))
        export = re.search(r"pub const MAX_BODY_BYTES: u64 = ([\d_ *+]+);",
                           (storage / "export.rs").read_text())[1]
        export_cap = sum(eval_product(term) for term in export.split("+"))
        largest = max(max(caps) + slack, export_cap)
        self.assertEqual(gateway.STORAGE_BODY_CAP_BYTES, largest + gateway.STORAGE_BODY_HEADROOM_BYTES)
        self.assertLessEqual(gateway.STORAGE_BODY_HEADROOM_BYTES, 8 * 1024)


def eval_product(term):
    product = 1
    for factor in term.split("*"):
        product *= int(factor.strip().replace("_", ""))
    return product

if __name__ == "__main__":
    unittest.main()


class ReadRateLimitTest(unittest.TestCase):
    def test_defaults_let_one_explorer_boot_through(self):
        text = render()
        self.assertIn("limit_req_zone $binary_remote_addr zone=read:10m rate=100r/s;", text)
        self.assertIn("limit_req zone=read burst=300 nodelay;", text)

    def test_rate_and_burst_are_configurable(self):
        text = render(GATEWAY_READ_RATE="40", GATEWAY_READ_BURST="80")
        self.assertIn("limit_req_zone $binary_remote_addr zone=read:10m rate=40r/s;", text)
        self.assertIn("limit_req zone=read burst=80 nodelay;", text)

    def test_rejects_anything_but_a_positive_integer(self):
        for value in ["0", "-1", "abc", "1.5", "", "1000000"]:
            with self.assertRaises(ValueError, msg=f"rate {value!r}"):
                render(GATEWAY_READ_RATE=value)
            with self.assertRaises(ValueError, msg=f"burst {value!r}"):
                render(GATEWAY_READ_BURST=value)
