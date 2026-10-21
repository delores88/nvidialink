"""Spark-only LAN candidates: no packets, credentials or owner interfaces."""
import ipaddress
import json
import os
from pathlib import Path
import stat
import tempfile
import unittest

import controller as c


class Routes(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.sys = self.root / "sys"
        self.base = self.sys / "class" / "net"
        self.base.mkdir(parents=True)
        self.addresses, self.masks = {}, {}

    def tearDown(self):
        self.temp.cleanup()

    def iface(self, name, address, kind="wired", speed=1000, up=True, prefix=24, member=None):
        path = self.base / name
        path.mkdir()
        if kind != "virtual":
            (path / "device").mkdir()
        if kind == "wifi":
            (path / "phy80211").mkdir()
        for item, value in {"carrier": int(up), "operstate": "up" if up else "down",
                            "type": 1, "speed": speed}.items():
            (path / item).write_text(str(value))
        if member:
            (path / ("lower_" + member)).touch()
        self.addresses[name] = address
        self.masks[name] = str(ipaddress.IPv4Network("0.0.0.0/" + str(prefix)).netmask)

    def probe(self):
        return c.lan_addresses(self.sys, self.addresses.get, self.masks.get, "posix")

    def test_physical_and_stacked_routes_sorted_private_only(self):
        self.iface("eth0", "192.168.20.10")
        self.iface("wlan0", "192.168.20.11", "wifi", 300)
        self.iface("qsfp0", "10.0.0.2", speed=200000)
        self.iface("bond0", "10.1.0.2", "virtual", member="qsfp0")
        self.iface("docker0", "172.17.0.1", "virtual")
        self.iface("veth0", "172.18.0.1", "virtual")
        self.iface("br0", "172.18.0.2", "virtual", member="veth0")
        self.iface("tailscale0", "100.80.0.1", "virtual")
        self.iface("pub0", "8.8.8.8")
        self.iface("cgnat0", "100.64.0.1")
        self.iface("link0", "169.254.0.1")
        self.iface("down0", "10.9.0.1", up=False)
        rows = self.probe()
        self.assertEqual([r["ipv4"] for r in rows], ["10.0.0.2", "10.1.0.2", "192.168.20.10", "192.168.20.11"])
        self.assertEqual(rows[1]["speed_mbps"], 200000)
        self.assertTrue(c.lan_routes_view(self.root, self.probe)["shared_network"])

    def test_deduplicates_caps_and_rejects_host_prefix(self):
        for i in range(12):
            self.iface("eth" + str(i), "10.0.0." + str(i + 1))
        self.iface("same", "10.0.0.1", "wifi")
        self.iface("host", "10.0.1.1", prefix=32)
        rows = self.probe()
        self.assertEqual(len(rows), 8)
        self.assertEqual(len({r["ipv4"] for r in rows}), 8)
        self.assertTrue(all(r["kind"] == "wired" for r in rows))

    def test_epoch_changes_with_address_prefix_kind_only(self):
        row = {"kind": "wired", "ipv4": "10.0.0.1", "prefix": 24, "speed_mbps": 1000}
        def epoch(rows):
            return c.lan_routes_view(self.root, lambda: rows)["epoch"]
        original = epoch([row])
        self.assertEqual(original, epoch([{**row, "speed_mbps": 100}]))
        self.assertNotEqual(original, epoch([{**row, "ipv4": "10.0.0.2"}]))
        self.assertNotEqual(original, epoch([{**row, "prefix": 16}]))
        self.assertNotEqual(original, epoch([{**row, "kind": "wifi"}]))
        salt = self.root / "lan-epoch.salt"
        self.assertEqual(salt.stat().st_size, 16)
        if os.name == "posix":
            self.assertEqual(stat.S_IMODE(salt.stat().st_mode), 0o600)
        other = self.root / "other"
        other.mkdir()
        self.assertNotEqual(original, c.lan_routes_view(other, lambda: [row])["epoch"])

    def test_bad_salt_omits_epoch_and_view_never_leaks_addresses(self):
        (self.root / "lan-epoch.salt").write_bytes(b"short")
        self.assertNotIn("epoch", c.lan_routes_view(self.root, lambda: []))
        for observed in (True, False):
            raw = {"observed": observed, "lan_epoch": "a" * 16, "routes": [{"ipv4": "10.1.2.3"}], "ipv4": "10.1.2.3"}
            view = c.network_view(raw)
            self.assertEqual(view["lan_epoch"], "a" * 16)
            self.assertNotIn("10.1.2.3", json.dumps(view))
        self.assertNotIn("lan_epoch", c.network_view({"observed": False, "lan_epoch": "10.1.2.3"}))


if __name__ == "__main__":
    unittest.main()
