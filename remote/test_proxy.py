from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import socket

import proxy
import prepare


class ProxyTests(unittest.TestCase):
    def test_exact_connect_allowlist(self):
        self.assertEqual(proxy.target(b"CONNECT index.crates.io:443 HTTP/1.1\r\nHost: index.crates.io:443\r\n\r\n"), "index.crates.io")
        for value in (b"CONNECT localhost:443 HTTP/1.1\r\n\r\n", b"CONNECT crates.io.evil.com:443 HTTP/1.1\r\n\r\n",
                      b"CONNECT github.com:443 HTTP/1.1\r\n\r\n", b"CONNECT crates.io:80 HTTP/1.1\r\n\r\n",
                      b"GET https://crates.io/ HTTP/1.1\r\n\r\n", b"CONNECT user:pass@crates.io:443 HTTP/1.1\r\n\r\n",
                      b"CONNECT crates.io:443 HTTP/1.1\r\nProxy-Authorization: secret\r\n\r\n"):
            with self.assertRaises(ValueError):
                proxy.target(value)

    def test_private_dns_and_rebinding_targets_refused(self):
        for address in ("127.0.0.1", "192.168.0.1", "169.254.169.254", "10.0.0.1", "::1"):
            with patch("proxy.socket.getaddrinfo", return_value=[(socket.AF_INET, socket.SOCK_STREAM, 6, "", (address, 443))]):
                with self.assertRaises(ValueError):
                    proxy.public_addresses("crates.io")

    def test_locked_commands_and_no_project_install_scripts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text("[package]\nname='test'\nversion='0.1.0'\n")
            (root / "Cargo.lock").write_text("version=4\n")
            command = prepare.commands("rust", root)[0][1]
            self.assertEqual(command[:3], ["cargo", "fetch", "--locked"])
            (root / "package.json").write_text("{}")
            (root / "package-lock.json").write_text("{}")
            self.assertIn("--ignore-scripts", prepare.commands("node", root)[0][1])
            (root / "requirements.lock").write_text("example==1.0 --hash=sha256:" + "a"*64)
            self.assertIn("--require-hashes", prepare.commands("python", root)[0][1])

    def test_missing_lockfile_refused(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(ValueError):
                prepare.commands("rust", Path(temporary))


if __name__ == "__main__":
    unittest.main()
