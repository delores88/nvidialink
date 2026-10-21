"""Size limits, background snapshot assembly, disk guards and the job work copy."""
import base64
import contextlib
import errno
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

import controller
from controller import CHUNK, GIB, Controller, Rejected, canonical, sha
from test_controller import IMAGE, NO_LINK, PROJECT, FakeDocker

PLENTY = 1 << 50
RESERVE = controller.DISK_RESERVE


class Crash(BaseException):
    """Stands in for the materializer process dying: no cleanup handler runs."""


def symlinks_supported():
    with tempfile.TemporaryDirectory() as folder:
        try:
            os.symlink("/sparkdev-source/probe", Path(folder) / "link")
        except (OSError, NotImplementedError):
            return False
    return True


def entry(path, content, mode=0o644):
    chunks = [{"sha256": sha(content[i:i + CHUNK]), "size": len(content[i:i + CHUNK])} for i in range(0, len(content), CHUNK)]
    return {"path": path, "size": len(content), "sha256": sha(content), "mode": mode, "chunks": chunks}


def large(seed, chunks=3, tail=b"end"):
    """A multi-chunk file of distinct chunks with no credential shapes."""
    return b"".join(bytes([0x61 + (seed + i) % 20]) * CHUNK for i in range(chunks)) + tail


class SizeLimitTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "state"
        self.docker = FakeDocker()
        self.free = PLENTY
        self.now = 0.0
        self.spawned = 0
        for name, value in (("LARGE_FILE", 4096), ("INLINE_BYTES", 2 * CHUNK)):
            patcher = mock.patch.object(controller, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.c = self.make()
        (self.root.parent / "runtimes.json").write_text(json.dumps({"v": 1, "runtimes": {"python": {"image": IMAGE, "provenance": "test fixture"}}}))

    def make(self):
        return Controller(self.root, self.docker, lambda: 120, lambda: {"cpu_count": 20, "memory_gb": 120}, link_probe=NO_LINK,
                          disk_probe=lambda: (self.free, PLENTY), spawn=self.spawn, clock=lambda: self.now, sleep=self.sleep)

    def spawn(self):
        self.spawned += 1

    def sleep(self, seconds):
        self.now += seconds

    def tearDown(self):
        self.c.db.close()
        for root, dirs, files in os.walk(self.tmp.name):
            for name in dirs + files:
                path = Path(root) / name
                if not path.is_symlink():
                    os.chmod(path, 0o700)
        self.tmp.cleanup()

    def rpc(self, op, **kwargs):
        result = self.c.rpc({"v": 1, "op": op, **kwargs})
        self.assertTrue(result["ok"], result)
        return result

    def error(self, op, **kwargs):
        result = self.c.rpc({"v": 1, "op": op, **kwargs})
        self.assertFalse(result["ok"], result)
        return result["error"]

    def upload(self, *contents):
        for content in contents:
            for start in range(0, len(content), CHUNK):
                raw = content[start:start + CHUNK]
                self.rpc("put", sha256=sha(raw), data=base64.b64encode(raw).decode())

    def sync(self, *entries, project=PROJECT):
        return self.c.rpc({"v": 1, "op": "sync", "project": project, "manifest": list(entries)})

    def queued(self):
        return self.c.pending_ids()

    def job(self, manifest_id, request_id="limits"):
        return self.rpc("run", project=PROJECT, manifest_id=manifest_id, argv=["python3", "main.py"], runtime="python",
                        client="codex", request_id=request_id)

    # Advertised limits and lstat-only presence.

    def test_limits_are_advertised_and_resource_limits_keep_their_key(self):
        probe = self.rpc("missing", hashes=[])
        self.assertEqual(probe["missing"], [])
        self.assertEqual(probe["limits"], {"chunk": CHUNK, "max_file": 16 * GIB, "max_tree": 32 * GIB, "max_small_tree": 8 * GIB,
                                           "large_file": controller.LARGE_FILE, "max_files": 100000, "max_request": 32 * CHUNK,
                                           "put_many_body": 12 * CHUNK, "missing_batch": 10000})
        self.assertEqual((probe["storage"]["free_bytes"], probe["storage"]["reserve_bytes"]), (PLENTY, 64 * GIB))
        status = self.rpc("status")
        self.assertIn("cpu_per_job", status["limits"])
        self.assertEqual(status["sync_limits"], probe["limits"])
        self.assertEqual((status["storage"]["free_bytes"], status["storage"]["queued_bytes"]), (PLENTY, 0))
        self.assertEqual(status["materializing"], [])
        self.assertEqual(self.rpc("doctor")["storage"]["reserve_bytes"], RESERVE)
        self.assertEqual(self.error("missing", hashes=["a" * 64] * 10001)["code"], "size_limit")

    def test_real_limits_and_their_messages(self):
        with mock.patch.object(controller, "LARGE_FILE", 512 * CHUNK):
            def sized(path, size):
                chunks = [{"sha256": "c" * 64, "size": CHUNK}] * (size // CHUNK) + ([{"sha256": "c" * 64, "size": size % CHUNK}] if size % CHUNK else [])
                return {"path": path, "size": size, "sha256": "d" * 64, "mode": 0o644, "chunks": chunks}
            over = self.sync(sized("a", 16 * GIB), sized("b", 16 * GIB), sized("c", 1))["error"]
            self.assertEqual((over["code"], over["message"]), ("size_limit", "Project exceeds 32 GiB"))
            self.assertEqual(self.sync(sized("a", 16 * GIB + 1))["error"]["code"], "invalid_request")
            small = self.sync(*[sized(f"s{i}", 512 * CHUNK) for i in range(16)], sized("t", 1))["error"]
            self.assertEqual((small["code"], small["message"]), ("size_limit", "Files of 512 MiB or less exceed 8 GiB in total"))
            # Exactly at the limits the manifest is valid; only the absent chunks stop it.
            self.assertEqual(self.sync(sized("a", 16 * GIB), sized("b", 8 * GIB), *[sized(f"s{i}", 512 * CHUNK) for i in range(16)])["error"]["code"], "missing_chunks")
        self.assertFalse(self.queued())

    def test_missing_is_lstat_only_and_a_corrupt_object_is_unlinked_then_uploaded_again(self):
        content = b"print('limits')\n"
        self.upload(content)
        target = self.root / "objects" / sha(content)
        target.write_bytes(content.replace(b"limits", b"LIMITS"))  # Same size, wrong content.
        oversize, folder = "e" * 64, "f" * 64
        (self.root / "objects" / oversize).write_bytes(b"x" * (CHUNK + 1))
        (self.root / "objects" / folder).mkdir()
        hashes = [sha(content), oversize, folder, "0" * 64]
        with mock.patch.object(controller, "sha", side_effect=AssertionError("missing must not hash")), \
             mock.patch.object(controller.hashlib, "sha256", side_effect=AssertionError("missing must not hash")):
            result = self.rpc("missing", hashes=hashes)
        self.assertEqual(result["missing"], [oversize, folder, "0" * 64])
        self.assertEqual(self.sync(entry("main.py", content))["error"]["code"], "hash_mismatch")
        self.assertFalse(target.exists())
        self.assertEqual(self.rpc("missing", hashes=[sha(content)])["missing"], [sha(content)])
        self.upload(content)
        mid = self.sync(entry("main.py", content))["manifest_id"]
        self.assertEqual((self.root / "snapshots" / mid / "main.py").read_bytes(), content)

    def test_put_many_body_follows_the_advertised_cap(self):
        chunks = [{"sha256": sha(raw), "data": base64.b64encode(raw).decode()} for raw in (b"a" * CHUNK, b"b" * CHUNK, b"c" * CHUNK)]
        self.assertEqual(self.rpc("put_many", chunks=chunks)["bytes"], 3 * CHUNK)  # Above the old three-MiB body.
        many = [{"sha256": sha(bytes([i]) * CHUNK), "data": base64.b64encode(bytes([i]) * CHUNK).decode()} for i in range(65, 75)]
        self.assertEqual(self.error("put_many", chunks=many)["code"], "size_limit")
        # A batch with one bad chunk stores nothing.
        bad = chunks[:1] + [{"sha256": "f" * 64, "data": "eA=="}]
        (self.root / "objects" / chunks[0]["sha256"]).unlink()
        self.assertEqual(self.error("put_many", chunks=bad)["code"], "hash_mismatch")
        self.assertFalse((self.root / "objects" / chunks[0]["sha256"]).exists())

    # Inline versus background assembly.

    def test_inline_below_threshold_and_spawn_above_it(self):
        small = b"s" * 100
        self.upload(small)
        result = self.sync(entry("small.txt", small))
        self.assertTrue(result["ok"], result)
        self.assertEqual((self.spawned, self.now), (0, 0.0))
        self.assertFalse(self.queued())
        big = b"x" * CHUNK + b"y" * CHUNK + b"z"  # One byte above the patched two-MiB inline threshold.
        self.upload(big)
        waiting = self.sync(entry("big.txt", big))["error"]
        self.assertEqual(waiting["code"], "materializing")
        self.assertEqual(waiting["message"], "Spark is assembling this snapshot: 0 of 3 MiB; run the same command again to continue")
        self.assertEqual(self.spawned, 1)
        self.assertGreaterEqual(self.now, controller.MATERIALIZE_WAIT)
        mid = sha(canonical([entry("big.txt", big)]))
        self.assertEqual(self.queued(), [mid])
        self.assertFalse(self.c.db.execute("SELECT 1 FROM manifests WHERE id=?", (mid,)).fetchone())
        self.assertEqual(self.c.materialize_queue(), 1)
        self.assertEqual(self.c.db.execute("SELECT project FROM projects WHERE manifest=?", (mid,)).fetchone()[0], PROJECT)
        self.assertFalse(self.queued())
        self.assertEqual(self.sync(entry("big.txt", big))["manifest_id"], mid)
        self.assertEqual((self.root / "snapshots" / mid / "big.txt").read_bytes(), big)
        for name in os.listdir(self.root / "snapshots"):
            self.assertFalse(name.startswith((".pending-", ".partial-")), name)

    def test_background_completion_is_seen_by_the_waiting_rpc(self):
        big = large(1)
        self.upload(big)
        self.c.spawn = self.c.materialize_queue  # An inline spawn: the "background" finishes immediately.
        result = self.sync(entry("big.txt", big))
        self.assertTrue(result["ok"], result)
        self.assertTrue(self.c.snapshot_ready(result["manifest_id"]))

    def test_global_lock_and_fifo_queue(self):
        names = []
        for index in range(3):
            content = large(index)
            self.upload(content)
            self.assertEqual(self.sync(entry(f"f{index}.bin", content))["error"]["code"], "materializing")
            names.append(sha(canonical([entry(f"f{index}.bin", content)])))

        @contextlib.contextmanager
        def held(_self):
            yield False
        with mock.patch.object(Controller, "materialize_lock", held):
            self.assertEqual(self.c.materialize_queue(), 0)
            self.c.tick()
            small = b"inline"
            self.upload(small)
            # A large background build holds the global lock: a small snapshot still commits inside the RPC.
            self.assertTrue(self.sync(entry("small.txt", small))["ok"])
            fourth = large(3)
            self.upload(fourth)
            self.assertEqual(self.sync(entry("f3.bin", fourth))["error"]["code"], "materializing")
        self.assertEqual(self.spawned, 3)  # Only the three unlocked syncs spawned a materializer.
        names.append(sha(canonical([entry("f3.bin", fourth)])))
        order = [names[2], names[0], names[3], names[1]]
        for position, manifest_id in enumerate(order):
            # Queue order is the record's write time, not the identifier.
            stamp = (1_700_000_000 + position) * 10**9
            os.utime(self.root / "pending" / (manifest_id + ".json"), ns=(stamp, stamp))
        self.assertEqual(self.queued(), order)
        extra = large(9)
        self.upload(extra)
        full = self.sync(entry("extra.bin", extra))["error"]
        self.assertEqual((full["code"], full["message"]), ("materializing", "Spark is already assembling 4 snapshots; run the same command again to continue"))
        # The cap never holds back a snapshot the RPC assembles itself.
        tiny = b"tiny but new"
        self.upload(tiny)
        self.assertTrue(self.sync(entry("tiny.txt", tiny))["ok"])
        self.assertEqual(len(self.queued()), controller.MAX_PENDING)
        status = self.rpc("status")
        self.assertEqual([item["manifest_id"] for item in status["materializing"]], order)
        self.assertEqual(status["storage"]["queued_bytes"], sum(len(large(i)) for i in range(4)))
        self.assertEqual(self.c.materialize_queue(), controller.MAX_PENDING)
        committed = [json.loads(r[0])["data"]["manifest"] for r in self.c.db.execute("SELECT data FROM audit WHERE kind='snapshot_committed' ORDER BY seq")]
        self.assertEqual([m for m in committed if m in order], order)  # Inline commits are audited too.

    def test_a_resumed_tree_counts_its_reverified_bytes_toward_the_inline_threshold(self):
        files = [entry(f"f{i:04d}.txt", (f"{i:04d}" * 1024).encode()) for i in range(40)]  # 40 x 4 KiB small files.
        for item in files:  # Stored directly: a put RPC per file would only slow the test.
            (self.root / "objects" / item["sha256"]).write_bytes((item["path"][1:5] * 1024).encode())
        with mock.patch.object(controller, "INLINE_BYTES", 64 * 1024):
            self.assertEqual(self.sync(*files)["error"]["code"], "materializing")  # 160 KiB: above 64 KiB.
            with self.crash_on(files[35]["sha256"]), self.assertRaises(Crash):
                self.c.materialize_queue()
            verified = []
            real = Controller.verify_file
            with mock.patch.object(Controller, "verify_file", lambda s, path, e: verified.append(e["path"]) or real(s, path, e)):
                # Only 20 KiB is new, but 140 KiB of kept files would be re-hashed: still background work.
                waiting = self.sync(*files)["error"]
            self.assertEqual((waiting["code"], verified), ("materializing", []))
            self.assertEqual(self.c.materialize_queue(), 1)
            self.assertTrue(self.sync(*files)["ok"])

    def test_disk_is_checked_before_any_upload(self):
        content = large(14)  # Three chunks plus a tail: about 3 MiB to upload, 3 MiB to assemble.
        self.free = RESERVE + 4 * CHUNK
        refused = self.sync(entry("big.bin", content))["error"]
        self.assertEqual(refused["code"], "insufficient_disk")
        self.assertIn("64 GiB in reserve", refused["message"])
        self.assertFalse(any((self.root / "objects").iterdir()))
        self.assertFalse(self.queued())
        self.free = RESERVE + 7 * CHUNK
        self.assertEqual(self.sync(entry("big.bin", content))["error"]["code"], "missing_chunks")
        # Each distinct chunk counts once: a repeated chunk is uploaded once.
        twice = b"r" * CHUNK * 2
        self.free = RESERVE + CHUNK + 2 * CHUNK
        self.assertEqual(self.sync(entry("twice.bin", twice))["error"]["code"], "missing_chunks")

    def test_queue_admission_runs_under_the_queue_lock(self):
        inside, events = [False], []
        real_lock, real_write = Controller.queue_lock, controller.atomic_write

        @contextlib.contextmanager
        def lock(controller_self):
            with real_lock(controller_self):
                inside[0] = True
                try:
                    yield
                finally:
                    inside[0] = False

        def write(path, data, mode=0o600):
            if Path(path).parent == self.root / "pending" and Path(path).suffix == ".json":
                events.append(inside[0])
            return real_write(path, data, mode)
        big = large(15)
        self.upload(big)
        with mock.patch.object(Controller, "queue_lock", lock), mock.patch.object(controller, "atomic_write", write), \
             mock.patch.object(Controller, "queue_view", lambda s: events.append(inside[0]) or []):
            self.sync(entry("big.bin", big))
        self.assertEqual(events, [True, True])  # The queue read and the record write.

    @unittest.skipUnless(os.name == "posix", "flock is posix-only")
    def test_held_record_and_global_locks_on_posix(self):
        import fcntl
        big, small = large(16), b"small and new"
        self.upload(big, small)
        self.assertEqual(self.sync(entry("big.bin", big))["error"]["code"], "materializing")
        mid = sha(canonical([entry("big.bin", big)]))
        with self.c.record_lock(mid) as held:
            self.assertTrue(held)
            other = self.make()
            try:
                self.assertIsNone(other.materialize_one(mid))  # Another process holds the record.
                self.assertFalse(other.claimable())
                self.assertEqual(other.materialize_queue(), 0)
            finally:
                other.db.close()
        fd = os.open(self.root / "snapshots" / ".materialize.lock", os.O_RDWR | os.O_CREAT, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)  # A background build owns the global lock.
            self.assertTrue(self.sync(entry("small.txt", small))["ok"])
            self.assertEqual(self.c.materialize_queue(), 0)
        finally:
            os.close(fd)
        self.assertEqual(self.c.materialize_queue(), 1)
        self.assertTrue(self.sync(entry("big.bin", big))["ok"])

    def test_scheduler_backstop_spawns_only_for_queued_work(self):
        self.c.tick()
        self.assertEqual(self.spawned, 0)
        big = large(2)
        self.upload(big)
        self.sync(entry("big.bin", big))
        self.assertEqual(self.spawned, 1)
        self.c.tick()
        self.assertEqual(self.spawned, 2)
        self.c.materialize_queue()
        self.c.tick()
        self.assertEqual(self.spawned, 2)

    def test_default_spawn_is_detached_with_a_minimal_environment(self):
        with mock.patch.object(controller.subprocess, "Popen") as popen:
            controller.spawn_materializer(self.root)
        argv, options = popen.call_args
        self.assertEqual(argv[0][-3:], ["materialize", "--state", str(self.root)])
        self.assertEqual(argv[0][1], "-I")
        self.assertEqual(Path(argv[0][2]).name, "controller.py")
        for name in ("stdin", "stdout", "stderr"):
            self.assertIs(options[name], subprocess.DEVNULL)
        self.assertTrue(options["start_new_session"] and options["close_fds"])
        self.assertEqual(options["env"], controller.TOOL_ENV)
        self.assertEqual(self.error("materialize")["code"], "unknown_operation")

    # Crash, resume and per-chunk checks.

    def crash_on(self, digest_value):
        real = Controller.read_object

        def read(controller_self, chunk):
            if chunk["sha256"] == digest_value:
                raise Crash()
            return real(controller_self, chunk)
        return mock.patch.object(Controller, "read_object", read)

    def test_crash_resume_keeps_completed_files_and_restarts_partials(self):
        small_a, small_c, big = b"a" * 100, b"c" * 100, large(3)
        self.upload(small_a, small_c, big)
        manifest = [entry("a.txt", small_a), entry("b.bin", big), entry("c.txt", small_c, 0o755)]
        self.assertEqual(self.sync(*manifest)["error"]["code"], "materializing")
        mid = sha(canonical(manifest))
        with self.crash_on(sha(big[CHUNK:2 * CHUNK])), self.assertRaises(Crash):
            self.c.materialize_queue()
        tree = self.root / "snapshots" / (".pending-" + mid)
        self.assertEqual((tree / "a.txt").read_bytes(), small_a)
        self.assertFalse((tree / "b.bin").exists())
        self.assertTrue(any((self.root / "snapshots" / (".partial-" + mid)).iterdir()))
        self.assertFalse(any((self.root / "files").iterdir()))
        # Crash again after the large file is linked, before the last small file.
        with self.crash_on(sha(small_c)), self.assertRaises(Crash):
            self.c.materialize_queue()
        store = self.root / "files" / (sha(big) + "-644")
        inode = os.stat(store).st_ino
        self.assertEqual(os.stat(tree / "b.bin").st_ino, inode)
        verified = []
        real = Controller.verify_file
        with mock.patch.object(Controller, "verify_file", lambda s, path, e: verified.append(Path(path).name) or real(s, path, e)):
            self.assertEqual(self.c.materialize_queue(), 1)
        self.assertEqual(verified, ["a.txt"])  # Small files are re-hashed; the fsynced large file is not.
        snapshot = self.root / "snapshots" / mid
        self.assertEqual([(snapshot / n).read_bytes() for n in ("a.txt", "b.bin", "c.txt")], [small_a, big, small_c])
        self.assertEqual(os.stat(snapshot / "b.bin").st_ino, inode)
        self.assertTrue(self.sync(*manifest)["ok"])
        self.assertFalse(self.queued())

    def test_resume_rebuilds_a_kept_small_file_that_changed(self):
        small_a, big = b"a" * 100, large(4)
        self.upload(small_a, big)
        manifest = [entry("a.txt", small_a), entry("b.bin", big)]
        self.sync(*manifest)
        mid = sha(canonical(manifest))
        with self.crash_on(sha(big[:CHUNK])), self.assertRaises(Crash):
            self.c.materialize_queue()
        kept = self.root / "snapshots" / (".pending-" + mid) / "a.txt"
        os.chmod(kept, 0o600)
        kept.write_bytes(b"b" * 100)  # Unsynced data lost or damaged in a power cut.
        self.c.materialize_queue()
        self.assertEqual((self.root / "snapshots" / mid / "a.txt").read_bytes(), small_a)

    def test_secret_across_a_resume_point_is_still_found(self):
        first = b"q" * (CHUNK - 10) + b"-----BEGIN"
        second = b" PRIVATE KEY-----" + b"r" * (CHUNK - 17)
        secret = first + second + b"s" * 10  # Above the inline threshold, so assembly is in the background.
        self.upload(secret)  # Each chunk passes the upload scan on its own.
        manifest = [entry("key.bin", secret)]
        self.sync(*manifest)
        mid = sha(canonical(manifest))
        with self.crash_on(sha(second)), self.assertRaises(Crash):
            self.c.materialize_queue()
        self.c.materialize_queue()
        self.assertEqual(self.sync(*manifest)["error"]["code"], "secret_detected")
        self.assertFalse(self.c.snapshot_ready(mid))
        self.assertFalse(os.path.lexists(self.root / "files" / (sha(secret) + "-644")))
        self.assertFalse(os.path.lexists(self.root / "snapshots" / (".pending-" + mid)))

    def test_failed_marker_is_returned_once(self):
        big = large(5)
        self.upload(big)
        manifest = [entry("big.bin", big)]
        self.sync(*manifest)
        corrupt = self.root / "objects" / sha(big[CHUNK:2 * CHUNK])
        corrupt.write_bytes(b"z" * CHUNK)
        self.c.materialize_queue()
        self.assertFalse(corrupt.exists())  # A chunk mismatch unlinks that object.
        mid = sha(canonical(manifest))
        self.assertTrue((self.root / "pending" / (mid + ".failed")).exists())
        self.assertFalse(self.queued())
        self.assertEqual(self.sync(*manifest)["error"]["code"], "hash_mismatch")
        self.assertEqual(self.sync(*manifest)["error"]["code"], "missing_chunks")
        self.upload(big)
        self.assertEqual(self.sync(*manifest)["error"]["code"], "materializing")
        self.c.materialize_queue()
        self.assertTrue(self.sync(*manifest)["ok"])

    def test_enospc_becomes_insufficient_disk_and_keeps_the_store(self):
        first, second = large(6), large(7)
        self.upload(first, second)
        manifest = [entry("a.bin", first), entry("b.bin", second)]
        self.sync(*manifest)
        real = Controller.assemble

        def assemble(s, e, target, meter, durable):
            if e["sha256"] == sha(second):
                raise OSError(errno.ENOSPC, "No space left on device")
            return real(s, e, target, meter, durable)
        with mock.patch.object(Controller, "assemble", assemble):
            self.c.materialize_queue()
        error = self.sync(*manifest)["error"]
        self.assertEqual(error["code"], "insufficient_disk")
        self.assertIn("64 GiB", error["message"])
        self.assertTrue((self.root / "files" / (sha(first) + "-644")).is_file())

    # Disk guards.

    def test_disk_guards_on_upload_sync_and_admission(self):
        self.free = RESERVE + 5
        raw = b"0123456789"
        error = self.error("put", sha256=sha(raw), data=base64.b64encode(raw).decode())
        self.assertEqual(error["code"], "insufficient_disk")
        self.assertIn("GiB", error["message"])
        self.assertNotIn(str(self.root), error["message"])
        self.assertEqual(self.error("put_many", chunks=[{"sha256": sha(raw), "data": base64.b64encode(raw).decode()}])["code"], "insufficient_disk")
        # Queued bytes count against the reserve.
        self.free = PLENTY
        a, b = large(10), large(11)
        self.upload(a, b)
        self.free = RESERVE + len(a) + len(b) - 1
        self.assertEqual(self.sync(entry("a.bin", a))["error"]["code"], "materializing")
        refused = self.sync(entry("b.bin", b))["error"]
        self.assertEqual(refused["code"], "insufficient_disk")
        self.assertEqual(len(self.queued()), 1)
        status = self.rpc("status")["storage"]
        self.assertEqual((status["free_bytes"], status["reserve_bytes"], status["queued_bytes"]), (self.free, RESERVE, len(a)))
        # A small sync below the reserve is refused too, by design.
        tiny = b"tiny"
        self.free = PLENTY
        self.upload(tiny)
        self.free = RESERVE - 1
        self.assertEqual(self.sync(entry("t.txt", tiny))["error"]["code"], "insufficient_disk")

    def test_queued_snapshot_bytes_are_promised_to_uploads_and_admission(self):
        queued = large(19)
        self.upload(queued)
        self.assertEqual(self.sync(entry("queued.bin", queued))["error"]["code"], "materializing")
        self.assertEqual(self.c.queued_bytes(), len(queued))
        self.assertEqual(self.c.queued_bytes(), sum(item["remaining_bytes"] for item in self.c.queue_view()))
        chunks = [bytes([0x41 + i]) * CHUNK for i in range(3)]
        self.free = RESERVE + len(queued) + 2 * CHUNK
        self.rpc("put", sha256=sha(chunks[0]), data=base64.b64encode(chunks[0]).decode())
        batch = [{"sha256": sha(raw), "data": base64.b64encode(raw).decode()} for raw in chunks]
        self.assertEqual(self.error("put_many", chunks=batch)["code"], "insufficient_disk")  # Free alone would admit it.
        # A job's work copy waits for the queued snapshot's bytes too.
        content = b"print('admission')\n"
        self.free = PLENTY
        self.upload(content)
        mid = self.sync(entry("main.py", content))["manifest_id"]
        job = self.job(mid)
        self.free = RESERVE + len(content) + len(queued) - 1
        self.c.tick()
        self.assertEqual(self.c.get_job(job["job"])["state"], "queued")
        blocked = next(j for j in self.rpc("status")["jobs"] if j["job"] == job["job"])
        self.assertIn("snapshots being assembled", blocked["admission_blocked"])
        self.free = RESERVE + len(content) + len(queued)
        self.c.tick()
        self.assertEqual(self.c.get_job(job["job"])["state"], "running")
        # Written bytes are no longer owed.
        self.c.materialize_queue()
        self.assertEqual(self.c.queued_bytes(), 0)

    def test_assembly_stops_before_cutting_into_the_reserve(self):
        first, second = large(17), large(18)
        self.upload(first, second)
        manifest = [entry("a.bin", first), entry("b.bin", second)]
        self.assertEqual(self.sync(*manifest)["error"]["code"], "materializing")

        def written():
            inodes = {}
            for folder in ("files", "snapshots"):
                for path in (self.root / folder).rglob("*"):
                    if path.is_file():
                        info = path.stat()
                        inodes[(info.st_dev, info.st_ino)] = info.st_size
            return sum(inodes.values())
        # Another writer took space after admission: room for the first file, not the second.
        base = RESERVE + len(first) + len(second) // 2
        self.c.disk_probe = lambda: (base - written(), PLENTY)
        self.c.materialize_queue()
        error = self.sync(*manifest)["error"]
        self.assertEqual(error["code"], "insufficient_disk")
        self.assertIn("64 GiB in reserve", error["message"])
        self.assertTrue((self.root / "files" / (sha(first) + "-644")).is_file())
        self.assertFalse(os.path.lexists(self.root / "files" / (sha(second) + "-644")))
        self.assertGreaterEqual(base - written(), RESERVE)

    def test_admission_waits_for_disk_and_work_copy_failure_finishes_the_job(self):
        content = b"print('disk')\n" * 10
        self.upload(content)
        mid = self.sync(entry("main.py", content))["manifest_id"]
        job = self.job(mid)
        self.free = RESERVE + len(content) - 1
        self.c.tick()
        self.assertEqual(self.c.get_job(job["job"])["state"], "queued")
        blocked = next(j for j in self.rpc("status")["jobs"] if j["job"] == job["job"])
        self.assertIn("64 GiB in reserve", blocked["admission_blocked"])
        self.free = RESERVE + len(content)
        self.c.tick()
        self.assertEqual(self.c.get_job(job["job"])["state"], "running")
        # Admitted, then the disk filled before the work copy: the job fails instead of sticking in 'starting'.
        other = sha(b"other")
        self.c.db.execute("INSERT INTO projects VALUES(?,?)", (other, mid))
        second = self.rpc("run", project=other, manifest_id=mid, argv=["true"], runtime="python", client="human", request_id="second")
        self.c.db.execute("UPDATE jobs SET state='starting' WHERE id=?", (second["job"],))
        self.free = RESERVE
        self.c.tick()
        row = self.c.get_job(second["job"])
        self.assertEqual((row["state"], row["exit_code"], row["error"]), ("failed", 125, "insufficient_disk"))

    # Store dedupe and the job work copy.

    def test_large_files_share_one_store_inode_and_emlink_copies(self):
        big = large(12)
        self.upload(big)
        first = [entry("one.bin", big)]
        second = [entry("two/copy.bin", big), entry("readme.txt", b"hi")]
        self.upload(b"hi")
        self.sync(*first)
        self.sync(*second, project=sha(b"second"))
        self.c.materialize_queue()
        store = self.root / "files" / (sha(big) + "-644")
        one = self.root / "snapshots" / sha(canonical(first)) / "one.bin"
        two = self.root / "snapshots" / sha(canonical(sorted(second, key=lambda e: e["path"]))) / "two" / "copy.bin"
        self.assertEqual({os.stat(p).st_ino for p in (store, one, two)}, {os.stat(store).st_ino})
        third = [entry("three.bin", big)]
        with mock.patch.object(controller.os, "link", side_effect=OSError(errno.EMLINK, "Too many links")):
            # Only links are needed, so no new bytes: the snapshot assembles inside the RPC.
            self.assertTrue(self.sync(*third)["ok"])
        three = self.root / "snapshots" / sha(canonical(third)) / "three.bin"
        self.assertNotEqual(os.stat(three).st_ino, os.stat(store).st_ino)
        self.assertEqual(three.read_bytes(), big)
        self.assertFalse(os.stat(three).st_mode & 0o222)

    def test_work_copy_is_atomic_after_an_injected_copy_failure(self):
        contents = [b"one\n", b"two\n", b"three\n"]
        self.upload(*contents)
        mid = self.sync(*[entry(f"f{i}.txt", c) for i, c in enumerate(contents)])["manifest_id"]
        job = self.job(mid)
        real, calls = shutil.copy2, []

        def flaky(source, target, **options):
            calls.append(target)
            if len(calls) == 2:
                raise OSError(errno.EIO, "Injected copy failure")
            return real(source, target, **options)
        with mock.patch.object(controller.shutil, "copy2", flaky):
            self.c.tick()
        row = self.c.get_job(job["job"])
        self.assertEqual((row["state"], row["exit_code"], row["error"]), ("failed", 125, "work_copy_failed"))
        folder = self.root / "jobs" / job["job"]
        self.assertFalse(os.path.lexists(folder / "work"))
        self.assertFalse(os.path.lexists(folder / "work.pending"))
        self.assertFalse(any(call[0] == "create" for call in self.docker.calls))

    def test_large_file_link_and_source_mount_never_carry_user_path_text(self):
        # path_name allows ',' and '"'; Windows cannot name a file with '"', so it uses '='.
        name = 'a,b"c' if os.name == "posix" else "a,b=c"
        big, small = large(13), b"#!/bin/sh\n"
        self.upload(big, small)
        manifest = [entry(name, big), entry("run.sh", small, 0o755)]
        self.sync(*manifest)
        self.c.materialize_queue()
        mid = sha(canonical(manifest))
        job = self.job(mid)
        made = []
        real_symlink = os.symlink

        def record(target, path, *args, **kwargs):
            made.append((target, Path(path).name))
            if symlinks_supported_here:
                real_symlink(target, path, *args, **kwargs)
            else:
                Path(path).write_bytes(b"")  # Stand-in where Windows refuses unprivileged links.
        symlinks_supported_here = symlinks_supported()
        with mock.patch.object(controller.os, "symlink", record):
            self.c.tick()
        self.assertEqual(self.c.get_job(job["job"])["state"], "running")
        self.assertEqual(made, [("/sparkdev-source/" + name, name)])
        work = self.root / "jobs" / job["job"] / "work"
        if symlinks_supported_here:
            self.assertEqual(os.readlink(work / name), "/sparkdev-source/" + name)  # Dangling on the host; never followed.
        self.assertEqual((work / "run.sh").read_bytes(), small)
        if os.name == "posix":
            self.assertEqual(os.stat(work / "run.sh").st_mode & 0o777, 0o755)
        args = next(call for call in self.docker.calls if call[0] == "create")
        self.assertIn(f"type=bind,src={self.root / 'snapshots' / mid},dst=/sparkdev-source,readonly", args)
        for arg in args:
            self.assertNotIn("a,b", arg)
        self.assertEqual(args[args.index("--ulimit") + 1], "fsize=4294967296:4294967296")

    def test_small_only_manifest_docker_arguments_are_unchanged(self):
        content = b"print('hello')\n"
        self.upload(content)
        mid = self.sync(entry("main.py", content))["manifest_id"]
        job = self.job(mid)["job"]
        args = self.c.create_args(self.c.get_job(job))
        root, cache = self.root / "jobs" / job, self.root / "caches" / PROJECT / IMAGE.split(":", 1)[1]
        uid = os.getuid() if hasattr(os, "getuid") else 1000
        gid = os.getgid() if hasattr(os, "getgid") else 1000
        golden = ["create", "--name", "sparkdev-job-" + job, "--pull", "never", "--network", "none", "--read-only",
                  "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", f"{uid}:{gid}",
                  "--cpus", "4", "--cpu-shares", "256", "--memory", "8g", "--memory-swap", "8g",
                  "--pids-limit", "1024", "--init", "--restart", "no", "--log-driver", "none", "--ulimit", "fsize=4294967296:4294967296",
                  "--tmpfs", "/tmp:rw,nosuid,nodev,size=536870912", "--workdir", "/work",
                  "--label", "com.delores.sparkdev.job=" + job, "--label", "com.delores.sparkdev.client=codex",
                  "--mount", f"type=bind,src={root / 'work'},dst=/work", "--mount", f"type=bind,src={root / 'output'},dst=/sparkdev-output",
                  "--mount", f"type=bind,src={root / 'argv.json'},dst=/sparkdev-argv.json,readonly",
                  "--mount", f"type=bind,src={root / 'worker.py'},dst=/sparkdev-worker.py,readonly",
                  "--mount", f"type=bind,src={cache / 'target'},dst=/target", "--mount", f"type=bind,src={cache / 'cargo'},dst=/cargo-home",
                  "--mount", f"type=bind,src={cache / 'npm'},dst=/npm-cache", "--mount", f"type=bind,src={cache / 'pip'},dst=/pip-cache",
                  "--env", "HOME=/tmp", "--env", "CARGO_HOME=/cargo-home", "--env", "CARGO_TARGET_DIR=/target", "--env", "CARGO_NET_OFFLINE=true",
                  "--env", "npm_config_cache=/npm-cache", "--env", "npm_config_offline=true", "--env", "PIP_CACHE_DIR=/pip-cache", "--env", "PIP_NO_INDEX=1",
                  "--env", "PYTHONDONTWRITEBYTECODE=1", "--env", "PYTHONUNBUFFERED=1", "--env", "SPARKDEV_RUNTIME_NAME=python",
                  "--env", "RAYON_NUM_THREADS=4", "--env", "OMP_NUM_THREADS=4", "--env", "OPENBLAS_NUM_THREADS=4", "--env", "MKL_NUM_THREADS=4",
                  "--env", "CARGO_BUILD_JOBS=4", "--entrypoint", "/usr/bin/python3", IMAGE, "/sparkdev-worker.py"]
        self.assertEqual(args, golden)
        self.assertEqual((root / "work" / "main.py").read_bytes(), content)
        if os.name == "posix":
            self.assertEqual(os.stat(root / "work" / "main.py").st_mode & 0o777, 0o644)


if __name__ == "__main__":
    unittest.main()
