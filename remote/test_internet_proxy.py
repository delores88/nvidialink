"""Public-mode wire and private-network guard tests; no public network is used."""
import contextlib
import ipaddress
import json
from pathlib import Path
import socket
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch

import proxy


def receive_all(sock):
    chunks = bytearray()
    while True:
        piece = sock.recv(65536)
        if not piece:
            return bytes(chunks)
        chunks.extend(piece)


class InternetProxyTests(unittest.TestCase):
    def setUp(self):
        self.guard = proxy.EgressGuard(probe=lambda: "9.9.9.9")
        self.assertTrue(self.guard.refresh())

    @contextlib.contextmanager
    def server(self, opener):
        with tempfile.TemporaryDirectory() as temporary:
            counters = proxy.Counters(Path(temporary) / "counters.json")
            server = proxy.PublicServer(("127.0.0.1", 0), counters, self.guard)
            thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": .01}, daemon=True)
            with patch.object(proxy, "open_upstream", side_effect=opener):
                thread.start()
                try:
                    yield server, counters
                finally:
                    server.shutdown()
                    server.server_close()
                    thread.join(2)

    def test_private_special_ipv6_and_extra_ranges(self):
        values = ("0.1.2.3", "10.1.2.3", "100.64.1.1", "127.1.1.1", "169.254.169.254", "172.17.0.1",
                  "192.0.0.9", "192.168.0.1", "198.18.1.1", "192.88.99.1", "224.0.0.1", "240.0.0.1",
                  "168.63.129.16", "::1", "2606:4700:4700::1111", "::ffff:8.8.8.8", "not-an-ip")
        for value in values:
            self.assertFalse(proxy.allowed(value), value)
        self.assertTrue(proxy.allowed("8.8.8.8"))
        with patch.object(proxy, "EXTRA", (ipaddress.IPv4Network("8.8.8.0/24"),)):
            self.assertFalse(proxy.allowed("8.8.8.8"))

    def test_dns_all_answers_checked_once_and_sockaddr_pinned(self):
        answer = lambda ip: (socket.AF_INET, socket.SOCK_STREAM, 6, "", (ip, 443))
        with patch.object(proxy.socket, "getaddrinfo", return_value=[answer("8.8.8.8"), answer("192.168.0.2")]) as dns, patch.object(proxy.socket, "socket") as create:
            with self.assertRaises(proxy.Refused) as error:
                proxy.open_upstream("example.com", 443, self.guard, self.guard.check())
            self.assertEqual(error.exception.code, 403)
            dns.assert_called_once_with("example.com", 443, socket.AF_INET, socket.SOCK_STREAM)
            create.assert_not_called()
        attempt = Mock()
        with patch.object(proxy.socket, "getaddrinfo", return_value=[answer("8.8.8.8")]) as dns, patch.object(proxy.socket, "socket", return_value=attempt):
            self.assertIs(proxy.open_upstream("example.com", 443, self.guard, self.guard.check()), attempt)
            attempt.connect.assert_called_once_with(("8.8.8.8", 443))
            self.assertEqual(dns.call_count, 1)

    def test_connect_attempts_have_one_total_deadline(self):
        answers = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("8.8.8.8", 443))] * 10
        now = [0.]
        attempt = Mock()
        def fail(_address):
            now[0] += 10
            raise TimeoutError()
        attempt.connect.side_effect = fail
        with patch.object(proxy.time, "monotonic", side_effect=lambda: now[0]), patch.object(proxy.socket, "getaddrinfo", return_value=answers), patch.object(proxy.socket, "socket", return_value=attempt):
            with self.assertRaises(proxy.Refused) as error:
                proxy.open_upstream("example.com", 443, self.guard, self.guard.check())
        self.assertEqual(error.exception.code, 502)
        self.assertEqual(attempt.connect.call_count, 3)

    def test_strict_authority_ports_and_ipv6(self):
        self.assertEqual(proxy.authority("Example.COM.:443"), ("example.com", 443))
        for value in ("example.com:0", "example.com:65536", "user@example.com:443", "example%2ecom:443", ".:443", "example.com:443:80", "example.com"):
            with self.assertRaises(proxy.Refused) as error:
                proxy.authority(value)
            self.assertEqual(error.exception.code, 400, value)
        with self.assertRaises(proxy.Refused) as error:
            proxy.authority("[::1]:443")
        self.assertEqual(error.exception.code, 403)

    def test_http_rewrite_preserves_end_to_end_credentials(self):
        raw = b"POST http://Example.com:8080/a?q=1 HTTP/1.1\r\nHost: attacker\r\nAuthorization: Bearer fixture\r\nCookie: a=b\r\nProxy-Authorization: drop\r\nConnection: keep-alive, X-Hop, Content-Length\r\nX-Hop: drop\r\nContent-Length: 3\r\n\r\n"
        host, port, rewritten, length = proxy.public_request(raw)
        self.assertEqual((host, port, length), ("example.com", 8080, 3))
        self.assertTrue(rewritten.startswith(b"POST /a?q=1 HTTP/1.1\r\n"))
        self.assertIn(b"Host: example.com:8080\r\n", rewritten)
        self.assertIn(b"Authorization: Bearer fixture\r\nCookie: a=b\r\n", rewritten)
        self.assertIn(b"Content-Length: 3\r\n", rewritten)
        self.assertNotIn(b"drop", rewritten)
        self.assertNotIn(b"attacker", rewritten)

    def test_transfer_encoding_is_411_and_duplicate_length_invalid(self):
        for headers, code in ((b"Transfer-Encoding: chunked", 411), (b"Content-Length: 1\r\nContent-Length: 1", 400), (b"Content-Length: -1", 400), (b" Bad: fold", 400)):
            with self.assertRaises(proxy.Refused) as error:
                proxy.public_request(b"POST http://example.com/ HTTP/1.1\r\n" + headers + b"\r\n\r\n")
            self.assertEqual(error.exception.code, code)

    def test_header_peek_preserves_tunnel_bytes(self):
        left, right = socket.socketpair()
        try:
            header = b"CONNECT example.com:443 HTTP/1.1\r\n\r\n"
            left.sendall(header + b"TLS")
            self.assertEqual(proxy.read_header(right), header)
            self.assertEqual(right.recv(3), b"TLS")
        finally:
            left.close(); right.close()

    def test_dripping_header_has_absolute_deadline(self):
        left, right = socket.socketpair()
        stop = threading.Event()
        def drip():
            while not stop.wait(.01):
                try:
                    left.sendall(b"a")
                except OSError:
                    return
        writer = threading.Thread(target=drip, daemon=True)
        try:
            writer.start()
            start = time.monotonic()
            with patch.object(proxy, "HEADER_TIMEOUT", .1), self.assertRaises(proxy.Refused):
                proxy.read_header(right)
            self.assertLess(time.monotonic() - start, .5)
        finally:
            stop.set(); left.close(); right.close(); writer.join(1)

    def test_pipelined_bytes_never_reach_first_origin(self):
        peer, upstream = socket.socketpair()
        received = []
        def origin():
            received.append(receive_all(peer))
            peer.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
            peer.close()
        origin_thread = threading.Thread(target=origin, daemon=True)
        origin_thread.start()
        with self.server(lambda *_: upstream) as (server, counters):
            with socket.create_connection(server.server_address, timeout=2) as client:
                client.sendall(b"POST http://example.com/a HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcGET http://other.com/ HTTP/1.1\r\n\r\n")
                self.assertTrue(receive_all(client).endswith(b"OK"))
            origin_thread.join(2)
            self.assertTrue(received[0].endswith(b"\r\n\r\nabc"))
            self.assertNotIn(b"other.com", received[0])
            self.assertEqual(counters.values["http_requests"], 1)

    def test_connect_half_close_delivers_final_response(self):
        peer, upstream = socket.socketpair()
        def origin():
            raw = receive_all(peer)
            peer.sendall(b"reply:" + raw)
            peer.shutdown(socket.SHUT_WR)
            peer.close()
        worker = threading.Thread(target=origin, daemon=True)
        worker.start()
        with self.server(lambda *_: upstream) as (server, counters):
            with socket.create_connection(server.server_address, timeout=2) as client:
                client.sendall(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
                self.assertIn(b"200 Connection Established", proxy.read_header(client))
                client.sendall(b"hello")
                client.shutdown(socket.SHUT_WR)
                self.assertEqual(receive_all(client), b"reply:hello")
            worker.join(2)
            self.assertEqual(counters.values["bytes_up"], 5)
            self.assertEqual(counters.values["bytes_down"], 11)

    def test_no_status_written_inside_established_tunnel(self):
        peer, upstream = socket.socketpair()
        try:
            with patch.object(proxy, "relay", side_effect=proxy.Refused(403, "Blocked")), self.server(lambda *_: upstream) as (server, _):
                with socket.create_connection(server.server_address, timeout=2) as client:
                    client.sendall(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
                    self.assertEqual(receive_all(client), b"HTTP/1.1 200 Connection Established\r\n\r\n")
        finally:
            peer.close()

    def test_no_status_written_after_http_response_starts(self):
        peer, upstream = socket.socketpair()
        def broken_relay(client, _upstream, _counters, started, _remaining, _guard, _epoch):
            started[0] = True
            client.sendall(b"HTTP/1.1 200 OK\r\n\r\npartial")
            raise OSError("fixture interruption")
        try:
            with patch.object(proxy, "relay", side_effect=broken_relay), self.server(lambda *_: upstream) as (server, _):
                with socket.create_connection(server.server_address, timeout=2) as client:
                    client.sendall(b"GET http://example.com/ HTTP/1.1\r\n\r\n")
                    self.assertEqual(receive_all(client), b"HTTP/1.1 200 OK\r\n\r\npartial")
        finally:
            peer.close()

    def test_http_forbidden_destination_replies_before_connecting(self):
        with self.server(lambda *_: (_ for _ in ()).throw(proxy.Refused(403, "Blocked: home or private network address"))) as (server, counters):
            with socket.create_connection(server.server_address, timeout=2) as client:
                client.sendall(b"GET http://192.168.0.1/ HTTP/1.1\r\n\r\n")
                self.assertIn(b"403 Blocked: home or private network address", receive_all(client))
            self.assertEqual(counters.values["blocked_private"], 1)

    def test_slots_refuse_503_before_another_thread(self):
        with self.server(lambda *_: self.fail("must not open")) as (server, _):
            for _ in range(256):
                self.assertTrue(server.slots.acquire(False))
            with socket.create_connection(server.server_address, timeout=2) as client:
                self.assertIn(b"503 Too many connections", receive_all(client))
            for _ in range(256):
                server.slots.release()

    def test_deny_file_invalid_missing_and_unknown_modes_fail_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "deny.json"
            for text in ('{}', '["::1/128"]', '["192.168.1.1/24"]', '["bad"]', json.dumps(["1.1.1.1/32"] * 257), ' ' * 65537):
                path.write_text(text)
                with self.assertRaises(ValueError):
                    proxy.load_extra(path)
            path.write_text('["192.168.0.0/24"]')
            self.assertEqual(str(proxy.load_extra(path)[0]), "192.168.0.0/24")
        for mode in ("unexpected", "", "allowlist"):
            with patch.dict(proxy.os.environ, {"SPARKDEV_PROXY_MODE": mode}):
                self.assertEqual(proxy.main(), 2)
        with patch.dict(proxy.os.environ, {"SPARKDEV_PROXY_MODE": "public"}), patch.object(proxy, "load_extra", side_effect=FileNotFoundError):
            self.assertEqual(proxy.main(), 3)

    def test_counter_file_contains_only_integer_totals(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "counters.json"
            counters = proxy.Counters(path)
            counters.add("bytes_down", 2 ** 35)
            counters.flush()
            first = path.stat().st_mtime_ns
            counters.flush()
            self.assertEqual(first, path.stat().st_mtime_ns)
            self.assertEqual(json.loads(path.read_text()), {**dict.fromkeys(proxy.PUBLIC_KEYS, 0), "bytes_down": 2 ** 35})

    def test_empty_loopback_readiness_probe_does_not_inflate_workload_counters(self):
        with self.server(lambda *_: self.fail("must not open")) as (server, counters):
            with socket.create_connection(server.server_address, timeout=2) as client:
                client.shutdown(socket.SHUT_WR)
                self.assertEqual(receive_all(client), b"")
            self.assertEqual(counters.values, dict.fromkeys(proxy.PUBLIC_KEYS, 0))
            with socket.create_connection(server.server_address, timeout=2) as client:
                client.sendall(b"invalid\r\n\r\n")
                self.assertIn(b"400 Invalid request", receive_all(client))
            self.assertEqual(counters.values["connections"], 1)
            self.assertEqual(counters.values["refused_invalid"], 1)


if __name__ == "__main__":
    unittest.main()
