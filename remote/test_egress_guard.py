"""Observed public-egress safety, with local sockets and injected HTTPS only."""
import contextlib
import socket
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

import proxy


class EgressGuardTests(unittest.TestCase):
    def guard(self, address="9.9.9.9", clock=time.monotonic):
        guard = proxy.EgressGuard(probe=lambda: address, clock=clock)
        self.assertTrue(guard.refresh())
        return guard

    def test_startup_refuses_without_verified_public_address(self):
        for outcome in ("192.168.1.1", "::1", None):
            guard = proxy.EgressGuard(probe=lambda: outcome)
            self.assertFalse(guard.refresh())
            with self.assertRaises(proxy.Refused):
                guard.check()
        with patch.dict(proxy.os.environ, {"SPARKDEV_PROXY_MODE": "public"}), patch.object(proxy, "load_extra", return_value=()), patch.object(proxy, "EgressGuard") as guard, patch.object(proxy, "PublicServer") as server:
            guard.return_value.refresh.return_value = False
            self.assertEqual(proxy.main(), 4)
            server.assert_not_called()

    def test_observed_egress_literal_and_dns_are_denied_before_connect(self):
        guard = self.guard()
        for name in ("9.9.9.9", "9.9.9.9.nip.io"):
            answers = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("9.9.9.9", 443))]
            with patch.object(proxy.socket, "getaddrinfo", return_value=answers), patch.object(proxy.socket, "socket") as connect:
                with self.assertRaises(proxy.Refused) as refused:
                    proxy.open_upstream(name, 443, guard, guard.check())
                self.assertEqual(refused.exception.code, 403)
                connect.assert_not_called()

    def test_guard_checked_again_after_socket_connect(self):
        guard = self.guard()
        epoch = guard.check()
        answers = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("8.8.8.8", 443))]
        attempt = Mock()
        attempt.connect.side_effect = lambda _: guard.invalidate()
        with patch.object(proxy.socket, "getaddrinfo", return_value=answers), patch.object(proxy.socket, "socket", return_value=attempt):
            with self.assertRaises(proxy.Refused):
                proxy.open_upstream("example.com", 443, guard, epoch)
            attempt.close.assert_called_once()

    def test_change_failure_expiry_and_recovery_never_reuse_old_epoch(self):
        now = [100.]
        guard = self.guard(clock=lambda: now[0])
        epoch = guard.check()
        self.assertTrue(guard.refresh())
        self.assertEqual(guard.check(), epoch)
        guard.probe = lambda: "8.8.8.8"
        self.assertTrue(guard.refresh())
        with self.assertRaises(proxy.Refused):
            guard.check(epoch)
        for address in ("9.9.9.9", "8.8.8.8"):
            with self.assertRaises(proxy.Refused) as denied:
                guard.check(address=address)
            self.assertEqual(denied.exception.code, 403)
        epoch = guard.check()
        guard.probe = Mock(side_effect=OSError())
        self.assertFalse(guard.refresh())
        with self.assertRaises(proxy.Refused):
            guard.check()
        guard.probe = lambda: "8.8.8.8"
        self.assertTrue(guard.refresh())
        with self.assertRaises(proxy.Refused):
            guard.check(epoch)
        epoch = guard.check()
        now[0] += proxy.EGRESS_MAX_AGE
        with self.assertRaises(proxy.Refused):
            guard.check()
        self.assertTrue(guard.refresh())
        with self.assertRaises(proxy.Refused):
            guard.check(epoch)

    def test_stuck_dns_is_bounded_and_never_spawns_second_probe(self):
        finish = threading.Event()
        calls = []
        def stuck():
            calls.append(1)
            finish.wait(2)
            return "9.9.9.9"
        guard = proxy.EgressGuard(probe=stuck)
        try:
            with patch.object(proxy, "EGRESS_PROBE_TIMEOUT", .05):
                start = time.monotonic()
                self.assertFalse(guard.refresh())
                self.assertFalse(guard.refresh())
                self.assertLess(time.monotonic() - start, .5)
                self.assertEqual(calls, [1])
                with self.assertRaises(proxy.Refused):
                    guard.check()
        finally:
            finish.set()
            guard.pending.join(2)

    def test_observed_address_history_is_bounded_and_exhaustion_fails_closed(self):
        guard = self.guard()
        guard.observed = {f"11.0.0.{n}" for n in range(256)}
        self.assertFalse(guard.refresh())
        self.assertEqual(len(guard.observed), 256)
        with self.assertRaises(proxy.Refused):
            guard.check()

    def test_active_tunnel_closes_on_change_refresh_failure_and_expiry(self):
        for trigger in ("change", "failure", "expiry"):
            now = [100.]
            guard = self.guard(clock=lambda: now[0])
            peer, upstream = socket.socketpair()
            with tempfile.TemporaryDirectory() as temporary:
                counters = proxy.Counters(Path(temporary) / "counter.json")
                with proxy.PublicServer(("127.0.0.1", 0), counters, guard) as server:
                    worker = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": .01}, daemon=True)
                    with patch.object(proxy, "open_upstream", return_value=upstream):
                        worker.start()
                        try:
                            with socket.create_connection(server.server_address, timeout=3) as client:
                                client.sendall(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
                                self.assertIn(b"200 Connection Established", proxy.read_header(client))
                                if trigger == "change":
                                    guard.probe = lambda: "8.8.8.8"
                                    self.assertTrue(guard.refresh())
                                elif trigger == "failure":
                                    guard.probe = Mock(side_effect=OSError())
                                    self.assertFalse(guard.refresh())
                                else:
                                    now[0] += proxy.EGRESS_MAX_AGE
                                client.settimeout(2)
                                self.assertEqual(client.recv(1024), b"", trigger)
                                # Established tunnels receive EOF, never an HTTP error.
                        finally:
                            server.shutdown()
                            worker.join(2)
                            peer.close()

    @contextlib.contextmanager
    def probe_wire(self, response):
        raw, secure, context = Mock(), Mock(), Mock()
        context.wrap_socket.return_value.__enter__ = Mock(return_value=secure)
        context.wrap_socket.return_value.__exit__ = Mock(return_value=False)
        secure.recv.side_effect = [response, b""]
        answer = (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("8.8.8.8", 443))
        with patch.object(proxy.socket, "getaddrinfo", return_value=[answer]) as dns, patch.object(proxy.socket, "socket", return_value=raw), patch.object(proxy.ssl, "create_default_context", return_value=context) as tls:
            yield raw, secure, context, dns, tls

    def test_probe_is_fixed_direct_https_with_verified_hostname_and_bounded_body(self):
        with self.probe_wire(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n9.9.9.9") as (raw, secure, context, dns, tls):
            self.assertEqual(proxy.observe_public_egress(), "9.9.9.9")
            dns.assert_called_once_with("api.ipify.org", 443, socket.AF_INET, socket.SOCK_STREAM)
            raw.connect.assert_called_once_with(("8.8.8.8", 443))
            tls.assert_called_once_with()
            context.wrap_socket.assert_called_once_with(raw, server_hostname="api.ipify.org")
            secure.sendall.assert_called_once_with(b"GET / HTTP/1.0\r\nHost: api.ipify.org\r\nConnection: close\r\n\r\n")
        for response in (b"HTTP/1.1 302 Found\r\nLocation: https://elsewhere/\r\n\r\n9.9.9.9", b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9.9.9.9", b"HTTP/1.1 200 OK\r\n\r\n192.168.0.1", b"HTTP/1.1 200 OK\r\n\r\n" + b"x" * 65, b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\n9.9.9.9"):
            with self.probe_wire(response), self.assertRaises(ValueError):
                proxy.observe_public_egress()

    def test_discovery_endpoint_dns_is_public_only_and_certificate_errors_fail(self):
        answer = lambda ip: (socket.AF_INET, socket.SOCK_STREAM, 6, "", (ip, 443))
        with patch.object(proxy.socket, "getaddrinfo", return_value=[answer("8.8.8.8"), answer("192.168.1.1")]), patch.object(proxy.socket, "socket") as connect, self.assertRaises(ValueError):
            proxy.observe_public_egress()
        connect.assert_not_called()
        with self.probe_wire(b"") as (_, _, context, _, _):
            context.wrap_socket.side_effect = proxy.ssl.SSLCertVerificationError("fixture")
            with self.assertRaises(ValueError):
                proxy.observe_public_egress()


if __name__ == "__main__":
    unittest.main()
