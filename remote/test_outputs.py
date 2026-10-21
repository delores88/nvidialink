"""Real Linux filesystem tests; Docker remains a fixture, never a live job."""
import base64
import json
import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest import mock

import controller
import outputs
from test_controller import FakeDocker, IMAGE, PROJECT, NO_LINK, PLENTY_DISK


@unittest.skipUnless(os.name == "posix", "Spark result fencing needs Linux directory descriptors")
class OutputTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name) / "state"
        self.docker = FakeDocker()
        self.spawned = []
        self.c = controller.Controller(self.root, self.docker, lambda: 120, lambda: {"cpu_count": 20, "memory_gb": 120},
                                       link_probe=NO_LINK, disk_probe=PLENTY_DISK, output_spawn=self.spawned.append)
        (self.root.parent / "runtimes.json").write_text(json.dumps({"v": 1, "runtimes": {"python": {"image": IMAGE, "provenance": "test fixture"}}}))
        self.c.resources({"v": 1, "op": "resources", "internet": False})
        self.original = b"original source\n"
        digest = controller.sha(self.original)
        self.rpc("put", sha256=digest, data=base64.b64encode(self.original).decode())
        manifest = [{"path": "main.py", "size": len(self.original), "sha256": digest, "mode": 0o644,
                     "chunks": [{"sha256": digest, "size": len(self.original)}]}]
        self.manifest = self.rpc("sync", project=PROJECT, manifest=manifest)["manifest_id"]
        self.counter = 0

    def tearDown(self):
        self.c.db.close()
        self.temp.cleanup()

    def rpc(self, op, **fields):
        result = self.c.rpc({"v": 1, "op": op, **fields})
        self.assertTrue(result["ok"], result)
        return result

    def submit(self, **extra):
        self.counter += 1
        return self.rpc("run", project=PROJECT, manifest_id=self.manifest, argv=["python3", "main.py"], runtime="python",
                        client="codex", request_id="output-test-" + str(self.counter), **extra)["job"]

    def setup_job(self, state="succeeded", **extra):
        job = self.submit(**extra)
        self.c.prepare_work(self.c.get_job(job))
        output = self.root / "jobs" / job / "output"
        output.mkdir(exist_ok=True)
        (output / "stdout.log").write_text("worker control log")
        self.c.finish(self.c.get_job(job), state, 0 if state == "succeeded" else 7)
        return job, self.root / "jobs" / job / "work", output

    def collect(self, job):
        self.c.outputs_manager.collect(job)
        return self.rpc("outputs", job=job)

    def test_automatic_manifest_tracks_changed_and_new_files(self):
        job, work, output = self.setup_job()
        (work / "main.py").write_bytes(b"updated source\n")
        (work / "new.txt").write_text("new file")
        (output / "movie.mp4").write_bytes(b"test media")
        result = self.collect(job)
        self.assertEqual(result["state"], "ready")
        entries = {(e["scope"], e["path"]): e for e in result["manifest"]["entries"]}
        self.assertEqual(set(entries), {("work", "main.py"), ("work", "new.txt"), ("output", "movie.mp4")})
        self.assertEqual(entries["work", "main.py"]["original_sha256"], controller.sha(self.original))
        self.assertIsNone(entries["work", "new.txt"]["original_sha256"])
        self.assertEqual(result["manifest_sha256"], controller.sha(controller.canonical(result["manifest"])))
        self.assertEqual(self.rpc("wait", job=job, timeout=0)["state"], "succeeded")

    def test_unchanged_inputs_and_control_caches_secret_paths_excluded(self):
        job, work, output = self.setup_job()
        for path in ("target/cache", "node_modules/pkg", ".venv/bin/python", ".git/objects/x", "build/artifact", ".env", "keys/token.pem", "name.spark-original-backup.txt", "name.spark-return-staging.txt"):
            destination = work / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text("do not return")
        result = self.collect(job)
        self.assertEqual(result["manifest"], {"v": 1, "entries": []})
        self.assertEqual(result["total_bytes"], 0)
        self.assertEqual(len(list((self.root / "results" / job).iterdir())), 1)

    def test_multichunk_immutable_transfer_and_resuming(self):
        job, _, output = self.setup_job()
        content = b"a" * outputs.CHUNK + b"b" * outputs.CHUNK + b"tail"
        (output / "large.bin").write_bytes(content)
        saved = self.collect(job)
        (output / "large.bin").write_bytes(b"changed after snapshot")
        result = b""
        for index in (0, 1, 2):
            chunk = self.rpc("output_chunk", job=job, scope="output", path="large.bin", index=index)
            self.assertEqual(chunk, self.rpc("output_chunk", job=job, scope="output", path="large.bin", index=index))
            raw = base64.b64decode(chunk["data"])
            self.assertEqual(controller.sha(raw), chunk["sha256"])
            result += raw
        self.assertEqual(result, content)
        self.assertEqual(saved["manifest"]["entries"][0]["sha256"], controller.sha(result))
        invalid = self.c.rpc({"v": 1, "op": "output_chunk", "job": job, "scope": "output", "path": "large.bin", "index": 3})
        self.assertEqual(invalid["error"]["code"], "invalid_offset")

    def test_large_result_streams_without_loading_file_into_memory(self):
        job, _, output = self.setup_job()
        block = b"video fixture\0" * (outputs.CHUNK // 14)
        with (output / "render.mp4").open("wb") as stream:
            for _ in range(130):
                stream.write(block)
        saved = self.collect(job)
        entry = saved["manifest"]["entries"][0]
        self.assertGreater(entry["bytes"], 100 * outputs.CHUNK)
        self.assertEqual(entry["bytes"], 130 * len(block))
        last = self.rpc("output_chunk", job=job, scope="output", path="render.mp4", index=len(entry["chunks"]) - 1)
        self.assertEqual(controller.sha(base64.b64decode(last["data"])), entry["chunks"][-1])

    def test_exact_digest_ack_is_idempotent_and_retention_visible(self):
        job, _, output = self.setup_job()
        (output / "one.txt").write_text("hello")
        saved = self.collect(job)
        wrong = self.c.rpc({"v": 1, "op": "outputs_ack", "job": job, "manifest_sha256": "0" * 64})
        self.assertEqual(wrong["error"]["code"], "hash_mismatch")
        first = self.rpc("outputs_ack", job=job, manifest_sha256=saved["manifest_sha256"])
        self.assertEqual(first, self.rpc("outputs_ack", job=job, manifest_sha256=saved["manifest_sha256"]))
        result = self.rpc("outputs", job=job)
        self.assertAlmostEqual(result["retention"]["expires_at"] - first["acknowledged_at"], outputs.ACKNOWLEDGED_SECONDS, places=4)
        self.c.db.execute("UPDATE job_outputs SET expires=? WHERE job=?", (time.time() - 1, job))
        self.c.outputs_manager.collect(job)
        self.assertEqual(first, self.rpc("outputs_ack", job=job, manifest_sha256=saved["manifest_sha256"]))

    def test_cached_job_reuses_snapshot_extends_retention_and_acks_separately(self):
        job, _, output = self.setup_job()
        (output / "one.txt").write_text("hello")
        saved = self.collect(job)
        self.rpc("outputs_ack", job=job, manifest_sha256=saved["manifest_sha256"])
        cached = self.submit()
        status = self.rpc("wait", job=cached)
        self.assertEqual(status["cached_from"], job)
        cached_outputs = self.rpc("outputs", job=cached)
        self.assertEqual(cached_outputs["manifest_sha256"], saved["manifest_sha256"])
        self.assertIsNone(cached_outputs["retention"]["acknowledged_at"])
        self.assertGreater(cached_outputs["retention"]["expires_at"], time.time() + 6 * 86400)
        data = self.rpc("output_chunk", job=cached, scope="output", path="one.txt", index=0)
        self.assertEqual(base64.b64decode(data["data"]), b"hello")
        self.rpc("outputs_ack", job=cached, manifest_sha256=saved["manifest_sha256"])
        self.assertLess(self.rpc("outputs", job=cached)["retention"]["expires_at"], time.time() + 2 * 86400)

    def test_expired_cache_result_runs_again(self):
        job, _, _ = self.setup_job()
        self.collect(job)
        self.c.db.execute("UPDATE job_outputs SET expires=? WHERE job=?", (time.time() - 1, job))
        replacement = self.submit()
        self.assertFalse(self.rpc("wait", job=replacement)["cache_hit"])
        self.assertEqual(self.rpc("outputs", job=job)["state"], "expired")
        self.c.outputs_manager.collect(job)
        self.assertFalse((self.root / "results" / job).exists())

    def test_no_return_is_explicit_and_keeps_logs(self):
        job, _, _ = self.setup_job(no_return=True)
        self.assertEqual(self.collect(job)["state"], "disabled")
        self.assertEqual(self.rpc("logs", job=job, since=0)["text"], "worker control log")
        self.assertFalse(self.spawned)
        cached = self.submit(no_return=True)
        self.assertTrue(self.rpc("wait", job=cached)["cache_hit"])
        self.assertEqual(self.rpc("outputs", job=cached)["state"], "disabled")

    def test_failed_and_cancelled_jobs_return_partial_files(self):
        for state in ("failed", "cancelled"):
            with self.subTest(state=state):
                job, _, output = self.setup_job(state, no_cache=True)
                (output / "partial.txt").write_text("partial")
                self.assertEqual(self.collect(job)["state"], "ready")
                final = self.rpc("wait", job=job)
                self.assertEqual((final["state"], final["exit_code"]), (state, 7))

    def test_cancelled_before_launch_returns_empty_manifest(self):
        job = self.submit()
        self.rpc("cancel", job=job)
        self.assertEqual(self.collect(job)["manifest"], {"v": 1, "entries": []})

    def test_running_container_is_never_snapshotted(self):
        job, _, _ = self.setup_job()
        self.docker.containers[self.c.container_name(job)] = {"Image": IMAGE, "Config": {"Labels": {"com.delores.sparkdev.job": job}}, "State": {"Status": "running"}}
        self.c.outputs_manager.collect(job)
        result = self.c.outputs_manager.metadata(self.c.get_job(job))
        self.assertEqual(result["error"]["code"], "outputs_running")
        self.assertFalse((self.root / "results" / job).exists())

    def test_symlinks_hardlinks_and_fifos_are_refused(self):
        for kind in ("symlink", "hardlink", "fifo", "directory_link"):
            with self.subTest(kind=kind):
                job, _, output = self.setup_job(no_cache=True)
                outside = Path(self.temp.name) / ("outside-" + kind)
                outside.write_text("must never be copied")
                target = output / "unsafe"
                if kind == "symlink":
                    target.symlink_to(outside)
                elif kind == "hardlink":
                    os.link(outside, target)
                elif kind == "fifo":
                    os.mkfifo(target)
                else:
                    target.symlink_to(Path(self.temp.name), target_is_directory=True)
                self.c.outputs_manager.collect(job)
                saved = self.c.outputs_manager.metadata(self.c.get_job(job))
                self.assertEqual(saved["error"]["code"], "outputs_unsafe")
                self.assertEqual(outside.read_text(), "must never be copied")
                self.assertFalse((self.root / "results" / job).exists())

    def test_large_input_symlink_is_skipped_only_for_exact_original_target(self):
        job, work, _ = self.setup_job()
        (work / "main.py").unlink()
        (work / "main.py").symlink_to("/sparkdev-source/main.py")
        with mock.patch.object(controller, "LARGE_FILE", 1):
            self.assertEqual(self.collect(job)["manifest"]["entries"], [])

    def test_secret_content_across_chunk_boundary_refused(self):
        job, _, output = self.setup_job()
        (output / "report.txt").write_bytes(b"a" * (outputs.CHUNK - 10) + b"-----BEGIN" + b" PRIVATE KEY-----")
        self.c.outputs_manager.collect(job)
        result = self.c.outputs_manager.metadata(self.c.get_job(job))
        self.assertEqual(result["error"]["code"], "outputs_secret")
        self.assertNotIn("PRIVATE KEY", json.dumps(result))

    def test_quota_failure_is_plain_and_does_not_publish_partial_files(self):
        job, _, output = self.setup_job()
        (output / "a.bin").write_bytes(b"a" * 100)
        with mock.patch.object(outputs, "TOTAL_BYTES", 50):
            self.c.outputs_manager.collect(job)
        saved = self.c.outputs_manager.metadata(self.c.get_job(job))
        self.assertEqual(saved["error"]["code"], "outputs_quota")
        self.assertFalse((self.root / "results" / job).exists())
        self.assertEqual(self.c.get_job(job)["state"], "succeeded")
        self.c.outputs_manager.collect(job)
        self.assertEqual(self.rpc("outputs", job=job)["state"], "ready")

    def test_disk_headroom_checked_before_each_file_even_below_chunk_interval(self):
        job, _, output = self.setup_job()
        (output / "a.bin").write_bytes(b"a" * 100)
        with mock.patch.object(self.c, "free_bytes", return_value=controller.DISK_STOP_FLOOR + 50):
            self.c.outputs_manager.collect(job)
        saved = self.c.outputs_manager.metadata(self.c.get_job(job))
        self.assertEqual(saved["error"]["code"], "outputs_disk_full")
        self.assertFalse((self.root / "results" / job).exists())

    def test_status_and_dispatch_do_no_work_tree_reads(self):
        job, _, _ = self.setup_job()
        with mock.patch.object(self.c.outputs_manager, "_files", side_effect=AssertionError("walk")), mock.patch.object(self.c.docker, "inspect", side_effect=AssertionError("docker")):
            self.assertEqual(self.rpc("wait", job=job)["outputs"]["state"], "pending")
            self.c.outputs_manager.step()
            self.c.outputs_manager.step()
        self.assertEqual(self.spawned, [job])

    def test_stale_collector_restarts_and_discards_unpublished_partial_tree(self):
        job, _, output = self.setup_job()
        (output / "real.txt").write_text("real")
        stale = self.root / "results" / (job + ".pending-crashed")
        stale.mkdir()
        (stale / "0").write_text("incomplete")
        self.c.db.execute("UPDATE job_outputs SET state='collecting',updated=0,dispatch=0 WHERE job=?", (job,))
        self.c.outputs_manager.step()
        self.assertEqual(self.spawned, [job])
        self.assertEqual(self.collect(job)["state"], "ready")
        self.assertFalse(stale.exists())

    def test_lock_prevents_duplicate_collector_and_chunk_integrity_checked(self):
        job, _, output = self.setup_job()
        (output / "one.txt").write_text("real")
        with self.c.outputs_manager.lock(job):
            self.c.outputs_manager.collect(job)
            self.assertFalse((self.root / "results" / job).exists())
        self.collect(job)
        blob = self.root / "results" / job / "0"
        blob.chmod(0o600)
        blob.write_bytes(b"fake")
        bad = self.c.rpc({"v": 1, "op": "output_chunk", "job": job, "scope": "output", "path": "one.txt", "index": 0})
        self.assertEqual(bad["error"]["code"], "outputs_corrupt")

    def test_result_and_lock_symlink_replacement_is_refused(self):
        job, _, output = self.setup_job()
        (output / "one.txt").write_text("real")
        self.collect(job)
        blob = self.root / "results" / job / "0"
        blob.unlink()
        outside = Path(self.temp.name) / "outside"
        outside.write_text("secret data must stay outside")
        blob.symlink_to(outside)
        bad = self.c.rpc({"v": 1, "op": "output_chunk", "job": job, "scope": "output", "path": "one.txt", "index": 0})
        self.assertEqual(bad["error"]["code"], "outputs_corrupt")
        lock = self.root / "results" / (job + ".lock")
        lock.unlink()
        lock.symlink_to(outside)
        bad = self.c.rpc({"v": 1, "op": "output_chunk", "job": job, "scope": "output", "path": "one.txt", "index": 0})
        self.assertEqual(bad["error"]["code"], "outputs_unsafe")
        self.assertEqual(outside.read_text(), "secret data must stay outside")

    def test_file_change_during_copy_never_publishes_snapshot(self):
        job, _, output = self.setup_job()
        file = output / "one.txt"
        file.write_bytes(b"a" * (2 * outputs.CHUNK))
        original = self.c.outputs_manager._copy
        def racing_copy(fd, name, info, destination, remaining, heartbeat, source=None):
            def change():
                if name == "one.txt":
                    with file.open("r+b") as stream:
                        stream.write(b"changed")
                heartbeat()
            return original(fd, name, info, destination, remaining, change, source)
        with mock.patch.object(self.c.outputs_manager, "_copy", side_effect=racing_copy):
            self.c.outputs_manager.collect(job)
        saved = self.c.outputs_manager.metadata(self.c.get_job(job))
        self.assertEqual(saved["error"]["code"], "outputs_changed")
        self.assertFalse((self.root / "results" / job).exists())

    def test_collector_launch_imports_under_isolated_python(self):
        # Missing job ends before any Docker lookup, exercising the real detached
        # launch/import path without starting Docker or touching owner resources.
        original = outputs.subprocess.Popen
        children = []
        def capture(*args, **kwargs):
            child = original(*args, **kwargs)
            children.append(child)
            return child
        with mock.patch.object(outputs.subprocess, "Popen", side_effect=capture):
            self.c.outputs_manager._spawn("not-a-job")
        self.assertEqual(children[0].wait(timeout=15), 0)

    def test_unicode_digest_and_portable_path_rejection(self):
        job, _, output = self.setup_job()
        (output / "café.txt").write_text("hello")
        saved = self.collect(job)
        self.assertEqual(saved["manifest_sha256"], controller.sha(json.dumps(saved["manifest"], sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()))
        for name in ("CON.txt", "trailing.", "back\\slash", "colon:name"):
            job, _, output = self.setup_job(no_cache=True)
            (output / name).write_text("unsafe name")
            self.c.outputs_manager.collect(job)
            self.assertEqual(self.c.outputs_manager.metadata(self.c.get_job(job))["error"]["code"], "outputs_unsafe")

    def test_casefold_directory_collision_rejected(self):
        job, _, output = self.setup_job()
        for name in ("A/one.txt", "a/two.txt"):
            path = output / name
            path.parent.mkdir()
            path.write_text("test")
        self.c.outputs_manager.collect(job)
        self.assertEqual(self.c.outputs_manager.metadata(self.c.get_job(job))["error"]["code"], "outputs_unsafe")


if __name__ == "__main__":
    unittest.main()
