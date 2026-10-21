import base64
import contextlib
import http.server
import json
import os
from pathlib import Path
import signal
import sqlite3
import tempfile
import threading
import time
import unittest
from unittest import mock

import controller
import model_server
import worker
from test_controller import FakeDocker, IMAGE, PROJECT, PLENTY_DISK, NO_LINK


class ServiceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "state"
        self.c = controller.Controller(self.root, FakeDocker(), lambda: 120,
                                       lambda: {"cpu_count": 20, "memory_gb": 120},
                                       link_probe=NO_LINK, disk_probe=PLENTY_DISK)
        (self.root.parent / "runtimes.json").write_text(json.dumps({"v": 1, "runtimes": {"python": {"image": IMAGE, "provenance": "fixture"}}}))
        self.now = 1800000000.0
        self.m = model_server.ServiceManager(self.c, controller, now=lambda: self.now)
        columns = {r[1] for r in self.c.db.execute("PRAGMA table_info(jobs)")}
        if "egress" not in columns:
            self.c.db.execute("ALTER TABLE jobs ADD COLUMN egress TEXT")
        self.c.job_network = lambda: {"egress": "off", "proxy_image": None}
        self.c.stage_network = lambda root, snapshot: None
        original = self.c.job_status
        self.c.job_status = lambda row: {**original(row), **model_server.status_fields(row)}
        self.manifest = "f" * 64
        source = self.c.snapshots / self.manifest
        source.mkdir()
        self.c.db.execute("INSERT INTO manifests VALUES(?,?,?)", (self.manifest, "[]", 0))
        self.c.db.execute("INSERT INTO projects VALUES(?,?)", (PROJECT, self.manifest))

    def tearDown(self):
        self.c.db.close()
        for root, dirs, files in os.walk(self.tmp.name):
            for name in files:
                path = Path(root) / name
                if not path.is_symlink():
                    os.chmod(path, 0o600)
        self.tmp.cleanup()

    def serve(self, name="tts", **kw):
        return self.m.serve({"v": 1, "op": "serve", "project": PROJECT, "manifest_id": self.manifest,
                             "argv": ["python3", "server.py"], "client": "codex", "request_id": "start-" + name, "name": name, **kw})

    def call(self, name="tts", request_id="call-1", body=b"hello", **kw):
        return self.m.call({"v": 1, "op": "call", "server": name, "request_id": request_id, "client": "codex",
                            "body": base64.b64encode(body).decode(), **kw})

    def row(self, job):
        return self.c.get_job(job["job"])

    def active(self, job, ready=True):
        self.c.db.execute("UPDATE jobs SET state='running',started=?,ready_at=?,last_activity=? WHERE id=?",
                          (self.now, self.now if ready else None, self.now, job["job"]))

    def stopped(self, job, reason):
        self.c.db.execute("UPDATE jobs SET state='cancelled',finished=?,stop_reason=?,cancel=1 WHERE id=?", (self.now, reason, job["job"]))

    def reply_files(self, call, body=b"response", **metadata):
        root = self.m.spool(call["job"]) / "replies"
        (root / (call["request_id"] + ".body")).write_bytes(body)
        (root / (call["request_id"] + ".json")).write_text(json.dumps({"status": 200, "content_type": "text/plain", **metadata}))
        return root

    def reply(self, call, since=0, **kw):
        return self.m.reply({"v": 1, "op": "reply", "request_id": call["request_id"], "since": since, **kw})

    def assert_code(self, code, fn, *args, **kw):
        with self.assertRaises(controller.Rejected) as caught:
            fn(*args, **kw)
        self.assertEqual(caught.exception.code, code)

    def test_schema_migration_is_idempotent_and_defaults_preserve_jobs(self):
        model_server.ServiceManager(self.c, controller)
        self.assertIn("calls", {r[0] for r in self.c.db.execute("SELECT name FROM sqlite_master WHERE type='table'")})
        job = self.serve()
        self.assertEqual(job["kind"], "service")
        self.assertEqual((job["cpu"], job["memory_gb"], job["scratch_mb"], job["idle_seconds"]), (2, 16, 2048, 1800))

    def test_names_and_limits_are_validated(self):
        for name in ("laptop", "laptop-tts", "TTS", "-tts", "a" * 25, "../tts"):
            self.assert_code("invalid_request", self.serve, name)
        for field, value in (("memory_gb", 49), ("memory_gb", True), ("port", 0), ("idle_minutes", 241)):
            self.assert_code("invalid_request", self.serve, **{field: value})
        self.assert_code("secret_detected", self.serve, argv=["echo", "-----BEGIN " + "PRIVATE KEY-----"])

    def test_same_request_and_same_definition_do_not_duplicate(self):
        first = self.serve()
        self.assertEqual(self.serve()["job"], first["job"])
        self.assertEqual(self.serve(request_id="another-id")["job"], first["job"])
        self.assert_code("idempotency_conflict", self.serve, argv=["different"])
        self.assertEqual(self.c.db.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], 1)
        self.stopped(first, "idle")
        self.assertEqual(self.serve(request_id="another-id")["job"], first["job"])

    def test_executable_request_metadata_is_published_after_identity_commit(self):
        self.serve()
        original = self.m._write_request
        def observe(row, raw, publish=True):
            if publish:
                with contextlib.closing(sqlite3.connect(self.root / "state.sqlite3")) as independent:
                    saved = independent.execute("SELECT job FROM calls WHERE id=?", (row["id"],)).fetchone()
                    self.assertIsNotNone(saved)
                    self.assertEqual(saved[0], row["job"])
            return original(row, raw, publish=publish)
        with mock.patch.object(self.m, "_write_request", side_effect=observe):
            call = self.call()
            self.stopped({"job": call["job"]}, "lifetime")
            self.reply(call)

    def test_poll_recovers_commit_before_metadata_publication_without_replay(self):
        self.serve()
        original = self.m._publish_request
        with mock.patch.object(self.m, "_publish_request", side_effect=RuntimeError("simulated process exit after commit")):
            with self.assertRaises(RuntimeError):
                self.call()
        row = self.c.db.execute("SELECT * FROM calls WHERE id='call-1'").fetchone()
        self.assertIsNotNone(row)
        path = self.m.spool(row["job"]) / "requests" / "call-1.json"
        self.assertFalse(path.exists())
        original("call-1")
        self.assertTrue(path.exists())
        self.assertEqual(self.call()["request_id"], "call-1")
        self.assertEqual(self.c.db.execute("SELECT COUNT(*) FROM calls").fetchone()[0], 1)

    def test_replace_requires_same_client_and_project_or_explicit_flag(self):
        first = self.serve()
        self.assert_code("server_name_in_use", self.serve, client="claude", request_id="claude-1", argv=["other"])
        different_project = "d" * 64
        self.c.db.execute("INSERT INTO projects VALUES(?,?)", (different_project, self.manifest))
        self.assert_code("server_name_in_use", self.serve, project=different_project, request_id="project-2", argv=["other"])
        second = self.serve(request_id="codex-2", argv=["other"])
        self.assertEqual(self.row(first)["stop_reason"], "replaced")
        third = self.serve(client="claude", request_id="claude-2", argv=["third"], replace=True)
        self.assertNotEqual(second["job"], third["job"])

    def test_two_server_limit_and_name_cancel(self):
        self.serve("tts")
        self.serve("embed")
        self.assert_code("too_many_servers", self.serve, "third")
        response = self.m.cancel_name({"v": 1, "op": "cancel", "name": "tts"})
        self.assertEqual((response["state"], response["stop_reason"]), ("cancelled", "stopped"))
        self.serve("third")
        self.assert_code("server_not_found", self.m.cancel_name, {"v": 1, "op": "cancel", "name": "tts"})

    def test_call_idempotency_rejects_changed_body_path_client_and_content_type(self):
        self.serve()
        call = self.call()
        self.assertEqual(call["state"], "loading")
        self.assertEqual(self.call()["request_id"], call["request_id"])
        for override in ({"body": b"changed"}, {"path": "/other"}, {"content_type": "text/plain"}):
            self.assert_code("idempotency_conflict", self.call, **override)
        self.assert_code("invalid_request", self.call, request_id="../../path")
        self.assert_code("invalid_request", self.call, request_id="invalid-path", path="/\r\nx")
        self.assert_code("secret_detected", self.call, request_id="secret", body=b"-----BEGIN " + b"PRIVATE KEY-----")

    def test_waiting_call_limit(self):
        self.serve()
        for i in range(32):
            self.call(request_id=f"call-{i}")
        self.assert_code("server_busy", self.call, request_id="call-33")

    def test_collector_computes_hash_then_protects_copy_from_container_mutation(self):
        self.serve()
        call = self.call()
        raw = b"abcdef"
        path = self.reply_files(call, raw, sha256="malicious-ignore-this", size=len(raw))
        self.now += 10
        first = self.reply(call, limit=2)
        self.assertEqual((first["state"], first["sha256"], first["size"], first["next_offset"]), ("done", controller.sha(raw), 6, 2))
        (path / (call["request_id"] + ".body")).write_bytes(b"tampered")
        rest = self.reply(call, since=2)
        self.assertEqual(base64.b64decode(first["data"]) + base64.b64decode(rest["data"]), raw)
        self.assertEqual(self.c.get_job(call["job"])["last_activity"], self.now)
        self.assertEqual(self.c.db.execute("SELECT delivered_at FROM calls").fetchone()[0], self.now)

    def test_hostile_reply_regular_file_bounds_and_size_mismatch(self):
        self.serve()
        for kind in ("hardlink", "oversize", "metadata", "mismatch"):
            call = self.call(request_id=kind)
            root = self.reply_files(call, b"x", size=1)
            body = root / (kind + ".body")
            if kind == "hardlink":
                os.link(body, root / "other-name")
            elif kind == "oversize":
                with open(body, "wb") as stream:
                    stream.truncate(model_server.REPLY_LIMIT + 1)
            elif kind == "metadata":
                (root / (kind + ".json")).write_bytes(b"x" * 4097)
            else:
                (root / (kind + ".json")).write_text(json.dumps({"status": 200, "size": 2}))
            self.assertEqual(self.reply(call)["state"], "failed", kind)

    @unittest.skipUnless(os.name == "posix", "FIFO and unprivileged symlink creation require POSIX")
    def test_hostile_fifo_and_symlink_never_block_or_follow(self):
        self.serve()
        for kind in ("fifo", "symlink"):
            call = self.call(request_id=kind)
            root = self.reply_files(call)
            body = root / (kind + ".body")
            body.unlink()
            if kind == "fifo":
                os.mkfifo(body)
            else:
                target = root / "target"
                target.write_bytes(b"private")
                body.symlink_to(target)
            self.assertEqual(self.reply(call)["state"], "failed")

    def test_idle_never_stops_waiting_call_then_stops_when_unused(self):
        job = self.serve(idle_minutes=5)
        self.active(job)
        call = self.call()
        self.now += 301
        info = {"State": {"Status": "running"}}
        self.assertFalse(self.m.reconcile(self.row(job), info))
        self.reply_files(call)
        self.reply(call)
        self.now += 301
        with mock.patch.object(self.c.docker, "call") as stop:
            self.assertTrue(self.m.reconcile(self.row(job), info))
            self.assertEqual(stop.call_args.args[0][:3], ["stop", "--time", "10"])
        self.assertEqual(self.row(job)["stop_reason"], "idle")

    def test_never_ready_and_lifetime_stops(self):
        first = self.serve()
        self.active(first, ready=False)
        self.now += model_server.STARTUP + 1
        with mock.patch.object(self.c.docker, "call"):
            self.assertTrue(self.m.reconcile(self.row(first), {"State": {"Status": "running"}}))
        self.assertEqual((self.row(first)["state"], self.row(first)["error"], self.row(first)["exit_code"]), ("failed", "never_ready", 137))
        second = self.serve(request_id="again")
        self.active(second)
        self.now += model_server.LIFETIME + 1
        with mock.patch.object(self.c.docker, "call"):
            self.m.reconcile(self.row(second), {"State": {"Status": "running"}})
        self.assertEqual((self.row(second)["stop_reason"], self.row(second)["exit_code"]), ("lifetime", 130))

    def test_unreadable_spool_stops_only_affected_server_and_cannot_block_manual_stop(self):
        for cancel, operation in ((False, "_ready"), (False, "_space_exceeded"), (True, "_ready")):
            job = self.serve(request_id=f"test-{cancel}-{operation}", argv=[operation, str(cancel)])
            self.active(job)
            if cancel:
                self.c.db.execute("UPDATE jobs SET cancel=1,stop_reason='stopped' WHERE id=?", (job["job"],))
            with mock.patch.object(self.m, operation, side_effect=PermissionError("unreadable spool")), mock.patch.object(self.c.docker, "call") as stop:
                self.assertTrue(self.m.reconcile(self.row(job), {"State": {"Status": "running"}}))
                stop.assert_called_once()
            row = self.row(job)
            self.assertEqual(row["stop_reason"], "stopped" if cancel else "reply_space_full")
            self.assertEqual(row["exit_code"], 130 if cancel else 137)

    def test_wake_matrix_and_no_wake_after_manual_stop_or_crash(self):
        for i, reason in enumerate(("idle", "lifetime", "internet_off", "stopped", "memory_watchdog", "never_ready", "replaced", None)):
            name = f"test-{i}"
            job = self.serve(name)
            self.stopped(job, reason)
            if reason in model_server.WAKE_REASONS:
                call = self.call(name, request_id=f"wakecall-{i}")
                self.assertNotEqual(call["job"], job["job"])
                self.stopped({"job": call["job"]}, "stopped")
            else:
                self.assert_code("server_stopped", self.call, name, request_id=f"wakecall-{i}")

    def test_unanswered_call_retargets_once_only_on_lifetime_and_matching_code(self):
        job = self.serve()
        call = self.call()
        self.stopped(job, "lifetime")
        moved = self.reply(call)
        self.assertNotEqual(moved["job"], job["job"])
        self.assertEqual(self.c.db.execute("SELECT retargeted FROM calls").fetchone()[0], 1)
        self.stopped(moved, "lifetime")
        self.assertEqual(self.reply(call)["state"], "stopped")

    def test_lifetime_wake_waits_through_runtime_refresh_without_losing_call(self):
        job = self.serve()
        call = self.call()
        self.stopped(job, "lifetime")
        with mock.patch.object(self.c, "runtime_gate", side_effect=controller.Rejected("runtime_refresh_in_progress", "fixture")):
            response = self.reply(call)
        self.assertIn(response["state"], ("loading", "waiting"))
        row = self.c.db.execute("SELECT * FROM calls WHERE id=?", (call["request_id"],)).fetchone()
        self.assertEqual((row["state"], row["retargeted"], row["job"]), ("waiting", 0, job["job"]))
        recovered = self.reply(call)
        self.assertNotEqual(recovered["job"], job["job"])
        self.assertEqual(self.c.db.execute("SELECT retargeted FROM calls WHERE id=?", (call["request_id"],)).fetchone()[0], 1)

    def test_no_retarget_after_replaced_service(self):
        job = self.serve()
        call = self.call()
        self.serve(request_id="replace", argv=["different"])
        self.assertEqual(self.reply(call)["state"], "stopped")

    def test_service_memory_exempts_batch_cap_but_still_fits_total_and_headroom(self):
        job = self.serve(memory_gb=32)
        limits = self.c.resource_policy()["limits"]
        self.assertGreater(32, limits["memory_gb_per_job"])
        with self.c.transaction():
            self.assertTrue(self.m.admit([], limits))
        self.active(job)
        self.assertEqual(self.m.reserved_for_headroom([self.row(job)]), 0)
        self.c.db.execute("UPDATE jobs SET ready_at=NULL WHERE id=?", (job["job"],))
        self.assertEqual(self.m.reserved_for_headroom([self.row(job)]), 32)

    def test_internet_helper_follows_shared_job_reservation_policy(self):
        self.c.job_network = lambda: {"egress": "internet", "proxy_image": IMAGE}
        job = self.serve(memory_gb=32)
        self.assertEqual(job["reserved_cpu"], 2)
        self.assertEqual(job["reserved_memory_gb"], 32)
        self.assertEqual(job["cpu"], 2)

    def test_server_mounts_hide_collected_and_keep_build_caches_read_only(self):
        job = self.serve()
        args = ["create", "--mount", "type=bind,src=/safe,dst=/pip-cache"]
        result = self.m.create_args(self.row(job), args)
        self.assertIn("type=bind,src=/safe,dst=/pip-cache,readonly", result)
        self.assertTrue(any("dst=/sparkdev-requests,readonly" in x for x in result))
        self.assertFalse(any("collected" in x for x in result))
        self.assertIn("NO_PROXY=localhost,127.0.0.1,::1", result)
        self.assertNotIn("-p", result)

    def test_expiration_retains_idempotency_and_never_resubmits(self):
        self.serve()
        call = self.call()
        self.reply_files(call)
        self.reply(call)
        self.now += 601
        self.m.housekeeping()
        self.assertEqual(self.call()["state"], "expired")
        self.assertEqual(self.c.db.execute("SELECT COUNT(*) FROM calls").fetchone()[0], 1)
        self.assertFalse((self.m.spool(call["job"]) / "requests" / "call-1.json").exists())


class WorkerServiceTests(unittest.TestCase):
    def test_real_loopback_forwarder_marks_ready_and_executes_request_once(self):
        received = []
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_GET(self):
                self.send_response(503)  # Any HTTP reply, not just success, is readiness.
                self.send_header("Content-Length", "0")
                self.end_headers()
            def do_POST(self):
                received.append((self.path, self.rfile.read(int(self.headers["Content-Length"]))))
                body = b'{"answer":42}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        serving = threading.Thread(target=server.serve_forever, daemon=True)
        serving.start()
        stop = threading.Event()
        try:
            with tempfile.TemporaryDirectory() as temporary:
                requests, replies = Path(temporary) / "requests", Path(temporary) / "replies"
                requests.mkdir()
                replies.mkdir()
                (requests / "call-1.body").write_bytes(b'{"text":"hi"}')
                (requests / "call-1.json").write_text(json.dumps({"id": "call-1", "path": "/v1/tts", "content_type": "application/json"}))
                forwarder = threading.Thread(target=worker.server_forwarder, args=(server.server_port, stop, requests, replies), daemon=True)
                with mock.patch.dict(os.environ, {"HTTP_PROXY": "http://127.0.0.1:1", "HTTPS_PROXY": "http://127.0.0.1:1"}):
                    forwarder.start()
                    deadline = time.monotonic() + 5
                    while not (replies / "call-1.json").exists() and time.monotonic() < deadline:
                        time.sleep(0.02)
                    time.sleep(0.2)
                    stop.set()
                    forwarder.join(2)
                self.assertTrue((replies / "_ready").is_file())
                self.assertEqual(json.loads((replies / "call-1.json").read_text())["status"], 200)
                self.assertEqual((replies / "call-1.body").read_bytes(), b'{"answer":42}')
                self.assertEqual(received, [("/v1/tts", b'{"text":"hi"}')])
        finally:
            stop.set()
            server.shutdown()
            server.server_close()
            serving.join(2)

    def test_http_client_ignores_environment_proxy_and_uses_fixed_loopback(self):
        response = mock.Mock(status=201)
        response.read.side_effect = [b"hello", b""]
        response.getheader.return_value = "text/plain"
        connection = mock.Mock()
        connection.return_value.getresponse.return_value = response
        with mock.patch.dict(os.environ, {"HTTP_PROXY": "http://untrusted.invalid:80", "HTTPS_PROXY": "http://untrusted.invalid:80"}):
            status, mime, raw = worker.forward_call(8123, "/v1", b"hi", "application/json", connection=connection)
        self.assertEqual((status, mime, raw), (201, "text/plain", b"hello"))
        connection.assert_called_once_with("127.0.0.1", 8123, timeout=600)
        self.assertEqual(connection.return_value.request.call_args.args[:2], ("POST", "/v1"))

    def test_bounded_reply_and_connection_failure_become_502(self):
        connection = mock.Mock()
        connection.return_value.getresponse.return_value.read.return_value = b"123456"
        self.assertEqual(worker.forward_call(8000, "/", b"", "text/plain", connection=connection, maximum=5)[0], 502)
        connection.return_value.request.side_effect = ConnectionRefusedError()
        self.assertEqual(worker.forward_call(8000, "/", b"", "text/plain", connection=connection)[0], 502)

    def test_worker_backstop_terminates_after_lifetime_plus_sixty(self):
        process, stop, killpg = mock.Mock(pid=42), mock.Mock(), mock.Mock()
        stop.wait.return_value = False
        worker.lifetime_stop(process, stop, 100, killpg=killpg)
        stop.wait.assert_called_once_with(160)
        killpg.assert_called_once_with(42, signal.SIGTERM)
        stop.wait.return_value = True
        killpg.reset_mock()
        worker.lifetime_stop(process, stop, 100, killpg=killpg)
        killpg.assert_not_called()

    def test_output_cap_adds_one_marker_but_drains_all_bytes(self):
        source, log = mock.Mock(), mock.Mock()
        with mock.patch.object(worker.os, "read", side_effect=[b"a" * 200, b"b" * 200, b"c" * 200, b""]) as read:
            worker.drain_output(source, log, maximum=128)
        output = b"".join(c.args[0] for c in log.write.call_args_list)
        self.assertEqual(read.call_count, 4)
        self.assertLessEqual(len(output), 128)
        self.assertEqual(output.count(b"output limit reached"), 1)


if __name__ == "__main__":
    unittest.main()
