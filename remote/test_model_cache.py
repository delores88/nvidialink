"""Download-cache tests use the real Controller DB and temporary state only."""
import contextlib
import json
import os
from pathlib import Path
import stat
import tempfile
import time
import unittest
from unittest import mock

import controller
from controller import Controller, Rejected
import model_cache
from model_cache import CacheManager, FLOOR, GIB, safe_name


class Docker:
    def __init__(self):
        self.calls = []
        self.names = set()
        self.fail = False

    def call(self, args, timeout=30):
        self.calls.append((args, timeout))
        if self.fail:
            raise OSError("test docker failure")
        if "--format" in args:
            return "\n".join(self.names).encode()
        return b"id" if self.names else b""


class CacheTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name) / "state"
        self.docker = Docker()
        self.free = 500 * GIB
        self.c = Controller(self.root, docker=self.docker, disk_probe=lambda: (self.free, 1000 * GIB),
                            link_probe=lambda: {"observed": False})
        self.cache = CacheManager(self.c, controller)
        self.now = time.time()
        self.cache.wall = lambda: self.now
        self.downloads = self.root / "shared" / "cache"

    def tearDown(self):
        self.cache._close_walk()
        if self.cache._trash is not None:
            self.cache._trash.close()
        self.c.db.close()
        # Tests make permission-trap directories deliberately. Restore only their
        # temporary fixture, never follow a link outside it.
        for root, dirs, files in os.walk(self.temp.name, topdown=True, followlinks=False):
            for name in dirs:
                path = Path(root) / name
                if not path.is_symlink():
                    os.chmod(path, 0o700)
        self.temp.cleanup()

    def job(self, name="job-test", state="running", started=None):
        self.c.db.execute("INSERT INTO jobs(id,request_id,request,project,manifest,runtime,image,client,state,created,started,finished,cache_key) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
                          (name, name, "{}", "a" * 64, "b" * 64, "python", "sha256:" + "c" * 64,
                           "codex", state, self.now - 100, started, self.now if state == "succeeded" else None, "d" * 64))
        (self.root / "jobs" / name).mkdir()
        return {"id": name}

    def file(self, name, hours=49, size=4096, area="cache"):
        path = self.root / "shared" / area / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"x" * size)
        when = self.now - hours * 3600
        os.utime(path, (when, when))
        parent = path.parent
        stop = self.root / "shared" / area
        while parent != stop:
            os.utime(parent, (when, when))
            parent = parent.parent
        return path

    def measure(self):
        for _ in range(200):
            self.cache.tidy_step()
            if self.cache._measurement_done:
                return
        self.fail("measurement did not complete")

    def evict(self, keep=1, free=500 * GIB):
        self.cache._guard = None
        self.cache._evict(keep, free, time.monotonic() + 1)

    def test_roots_are_owner_only_and_symlinks_are_refused(self):
        for name in ("cache", "prepare", "trash"):
            info = (self.root / "shared" / name).lstat()
            self.assertTrue(stat.S_ISDIR(info.st_mode))
            if os.name == "posix":
                self.assertEqual(stat.S_IMODE(info.st_mode), 0o700)
        if os.name == "posix":
            (self.root / "shared" / "prepare").rmdir()
            (self.root / "shared" / "prepare").symlink_to(self.temp.name, target_is_directory=True)
            with self.assertRaises(Rejected):
                CacheManager(self.c, controller)

    def test_four_job_kinds_keep_code_and_tokens_per_job(self):
        for runtime in ("python", "cuda"):
            for preparing in (False, True):
                name = runtime + ("-prepare" if preparing else "-run")
                row = self.job(name)
                row["runtime"] = runtime
                args = self.cache.create_args(row, preparing=preparing, network_open=True)
                text = " ".join(args)
                if preparing:
                    self.assertIn("dst=/prepare-cache", text)
                    self.assertIn("PIP_CACHE_DIR=/prepare-cache/pip", args)
                    self.assertNotIn("dst=/cache", text)
                else:
                    self.assertIn("dst=/cache", text)
                    self.assertIn("dst=/var/tmp", text)
                    for value in ("HF_TOKEN_PATH=/tmp/huggingface/token", "HF_MODULES_CACHE=/tmp/huggingface/modules",
                                  "TORCH_EXTENSIONS_DIR=/var/tmp/torch_extensions", "TRITON_CACHE_DIR=/var/tmp/triton",
                                  "TORCHINDUCTOR_CACHE_DIR=/var/tmp/torchinductor", "CUDA_CACHE_PATH=/var/tmp/cuda",
                                  "TMPDIR=/var/tmp", "UV_LINK_MODE=copy"):
                        self.assertIn(value, args)
                    for wrong in ("XDG_CONFIG_HOME=", "XDG_DATA_HOME=", "HTTP_PROXY=", "PIP_INDEX_URL=", "HF_ENDPOINT="):
                        self.assertNotIn(wrong, text)
                    self.assertNotIn("HF_HUB_OFFLINE=1", args)

    def test_offline_flags_only_on_offline_runs(self):
        row = self.job()
        args = self.cache.create_args(row, network_open=False)
        for name in ("HF_HUB_OFFLINE", "HF_DATASETS_OFFLINE", "TRANSFORMERS_OFFLINE"):
            self.assertIn(name + "=1", args)

    @unittest.skipUnless(os.name == "posix", "Linux dir-fd behavior")
    def test_root_repair_quarantines_link_without_touching_sentinel(self):
        sentinel = Path(self.temp.name) / "sentinel"
        sentinel.mkdir()
        secret = sentinel / "do-not-touch"
        secret.write_text("unchanged")
        before = secret.stat()
        (self.downloads / "huggingface").symlink_to(sentinel, target_is_directory=True)
        self.assertTrue(self.cache.repair_roots())
        self.assertFalse((self.downloads / "huggingface").is_symlink())
        self.assertTrue((self.downloads / "huggingface" / "hub").is_dir())
        for _ in range(3):
            self.cache.tidy_step()
        self.assertEqual(secret.read_text(), "unchanged")
        self.assertEqual(secret.stat().st_mtime_ns, before.st_mtime_ns)

    def test_repair_failure_does_not_raise(self):
        with mock.patch.object(self.cache, "_dir", side_effect=PermissionError()):
            self.assertFalse(self.cache.repair_roots())
        self.assertGreater(self.cache.errors, 0)

    def test_owner_changes_deferred_clear_and_status_never_walks(self):
        for client in ("claude", "codex"):
            with self.assertRaises(Rejected) as error:
                self.cache.downloads({"v": 1, "op": "downloads", "client": client, "free_up": True})
            self.assertEqual(error.exception.code, "owner_only")
        result = self.cache.downloads({"v": 1, "op": "downloads", "client": "human", "free_up": True, "keep_gb": 25})
        self.assertTrue(result["downloads"]["clear_pending"])
        self.assertEqual(result["downloads"]["keep_bytes"], 25 * GIB)
        with mock.patch.object(self.cache, "_scan", side_effect=AssertionError("status walked")):
            view = self.cache.downloads({"v": 1, "op": "downloads", "client": "codex"})
        self.assertTrue(view["downloads"]["clear_pending"])
        for value in (19, 1001, True, 21.1, "25"):
            with self.assertRaises(Rejected):
                self.cache.downloads({"v": 1, "op": "downloads", "client": "human", "keep_gb": value})

    def test_no_shared_renames_while_job_active_then_clear(self):
        path = self.file("files/weights")
        self.job(started=self.now)
        self.cache.downloads({"v": 1, "op": "downloads", "client": "human", "free_up": True})
        with mock.patch.object(self.cache, "_rename_trash", side_effect=AssertionError("active rename")):
            self.cache.tidy_step()
        self.assertTrue(path.exists())
        self.assertTrue(self.cache.status()["clear_pending"])
        self.c.db.execute("UPDATE jobs SET state='succeeded'")
        for _ in range(3):
            self.cache.tidy_step()
        self.assertFalse(path.exists())
        self.assertFalse(self.cache.status()["clear_pending"])
        self.assertTrue((self.downloads / "files").is_dir())
        self.assertTrue(any(args == ["ps", "-q", "--filter", "name=sparkdev-job-"] for args, _ in self.docker.calls))

    def test_clear_and_eviction_fail_closed_on_docker_failure_or_orphan(self):
        path = self.file("files/weights")
        self.measure()
        self.cache.downloads({"v": 1, "op": "downloads", "client": "human", "free_up": True})
        self.docker.fail = True
        self.cache.tidy_step()
        self.assertTrue(path.exists())
        self.docker.fail = False
        self.docker.names.add("sparkdev-job-orphan")
        self.cache.tidy_step()
        self.assertTrue(path.exists())
        self.assertTrue(self.cache.status()["clear_pending"])

    def test_47_hours_kept_49_hours_evicted(self):
        old = self.file("files/old", hours=49)
        recent = self.file("files/recent", hours=47)
        self.measure()
        self.evict()
        self.assertFalse(old.exists())
        self.assertTrue(recent.exists())
        self.evict()
        self.assertEqual(self.cache.status()["over_limit"], "recently_used")

    def test_directory_atime_ignored_and_future_time_clamped(self):
        path = self.file("huggingface/hub/models--org--model/weights", hours=49)
        unit = path.parent
        os.utime(unit, (self.now + 86400, self.now - 49 * 3600))
        self.measure()
        measured = self.cache._units[("cache", ("huggingface", "hub", "models--org--model"))]
        self.assertEqual(measured["last_used"], self.now - 49 * 3600)
        self.evict()
        self.assertFalse(unit.exists())
        future = self.file("files/future", hours=-2)
        self.cache._new_measurement(200 * GIB)
        self.measure()
        self.assertEqual(self.cache._units[("cache", ("files", "future"))]["last_used"], self.now)
        self.assertTrue(future.exists())

    @unittest.skipUnless(os.name == "posix", "Allocated blocks and hard links on Linux")
    def test_allocated_blocks_hardlinks_dedup_and_sparse_files(self):
        first = self.file("files/first", size=4096)
        os.link(first, self.downloads / "files" / "second")
        sparse = self.downloads / "files" / "sparse"
        with sparse.open("wb") as stream:
            # Test allocated-space accounting independently of the runner's
            # per-file cap (the installed pre-upgrade runner allows 4 GiB).
            stream.truncate(2 * GIB)
        self.measure()
        expected = first.stat().st_blocks * 512 + sparse.stat().st_blocks * 512
        self.assertEqual(self.cache.status()["used_bytes"], expected)
        self.assertLess(expected, GIB)
        self.assertEqual(sparse.stat().st_size, 2 * GIB)

    def test_ninety_percent_stop_and_floor(self):
        for index in range(10):
            self.file("files/item" + str(index))
        self.measure()
        initial = self.cache._used
        keep = initial - 1
        for _ in range(5):
            self.evict(keep)
        self.assertLessEqual(self.cache._used, keep * 9 // 10)
        self.assertGreater(self.cache._used, keep * 9 // 10 - initial // 10 - 1)
        before = self.cache._used
        self.evict(200 * GIB, free=FLOOR - 1)
        self.assertLess(self.cache._used, before)
        after = self.cache._used
        self.evict(200 * GIB, free=FLOOR + 1)
        self.assertEqual(self.cache._used, after)

    def test_measurement_resumes_and_rechecks_after_job_ran(self):
        old = self.file("files/first")
        for index in range(40):
            self.file("files/other" + str(index), hours=47)
        counter = [0]
        def clock():
            counter[0] += 0.001
            return counter[0]
        self.cache.monotonic = clock
        with mock.patch.object(model_cache, "STEP_SECONDS", 0.03):
            for _ in range(100):
                self.cache.tidy_step()
                if ("cache", ("files", "first")) in self.cache._units:
                    break
            measured_before = set(self.cache._units)
            self.assertTrue(measured_before)
            self.job(started=self.now)
            self.cache.tidy_step()
            self.assertIsNone(self.cache._walk)
            self.assertTrue(measured_before.issubset(self.cache._units))
            os.utime(old, (self.now, self.now))
            self.c.db.execute("UPDATE jobs SET state='succeeded'")
            self.measure()
        self.cache.monotonic = time.monotonic
        for _ in range(4):
            self.evict()
        self.assertTrue(old.exists(), "A read during the intervening job must protect a previously measured unit")

    def test_walk_max_never_evicts_unmeasured_unit(self):
        path = self.file("huggingface/hub/models--big/weights")
        for index in range(30):
            self.file("huggingface/hub/models--big/f" + str(index))
        with mock.patch.object(model_cache, "WALK_MAX", 12):
            self.measure()
        self.assertNotIn(("cache", ("huggingface", "hub", "models--big")), self.cache._units)
        self.assertEqual(self.cache.status()["over_limit"], "walk_limit")
        self.evict()
        self.assertTrue(path.exists())

    @unittest.skipUnless(os.name == "posix", "Linux permission repair")
    def test_trash_0555_0000_and_deep_path_chop(self):
        trash = self.root / "shared" / "trash"
        for mode in (0o555, 0):
            parent = trash / str(mode)
            parent.mkdir()
            (parent / "payload").write_bytes(b"safe")
            os.chmod(parent, mode)
        current = trash / "deep"
        current.mkdir()
        for index in range(140):
            current = current / ("level-" + str(index).zfill(3) + "-abcdefgh")
            current.mkdir()
        (current / "payload").write_bytes(b"safe")
        self.assertGreater(len(os.fsencode(str(current))), 2048)
        with mock.patch.object(self.cache, "_rename_trash", wraps=self.cache._rename_trash) as move:
            for _ in range(100):
                self.cache.tidy_step()
                if not list(trash.iterdir()):
                    break
            self.assertTrue(move.called)
        self.assertEqual(list(trash.iterdir()), [])

    def test_tmp_is_kept_until_terminal_and_not_in_docker_ps(self):
        row = self.job()
        self.cache.create_args(row)
        path = self.root / "jobs" / row["id"] / "tmp"
        (path / "wheel").write_text("temporary")
        self.cache.tidy_step()
        self.assertTrue(path.exists())
        self.c.db.execute("UPDATE jobs SET state='succeeded'")
        self.docker.names.add(self.c.container_name(row["id"]))
        self.cache.tidy_step()
        self.assertTrue(path.exists())
        self.docker.names.clear()
        self.cache.tidy_step()
        self.assertFalse(path.exists())

    @unittest.skipUnless(os.name == "posix", "Linux very-deep directory descriptors")
    def test_short_component_depth_is_chopped_before_python_recursion_limit(self):
        trash = self.root / "shared" / "trash"
        current = trash / "short-components"
        current.mkdir()
        for _ in range(1100):
            current /= "x"
            current.mkdir()
        (current / "payload").write_bytes(b"safe")
        with mock.patch.object(self.cache, "_rename_trash", wraps=self.cache._rename_trash) as move:
            for _ in range(100):
                self.cache.tidy_step()
                if not list(trash.iterdir()):
                    break
            self.assertGreaterEqual(move.call_count, 4)
        self.assertEqual(list(trash.iterdir()), [])

    def test_inode_or_ancestor_link_change_prevents_eviction(self):
        path = self.file("files/item")
        self.measure()
        old = path.with_name("moved")
        path.rename(old)
        path.write_text("new")
        self.evict()
        self.assertEqual(path.read_text(), "new")

    def test_items_sanitized_top_twenty_and_pip_merged(self):
        for index in range(25):
            self.file("files/item" + str(index), size=(index + 1) * 4096)
        self.file("pip/http/a", size=4096)
        self.file("pip/wheels/b", size=4096)
        self.file("pip/wheels/c", size=4096, area="prepare")
        self.file("huggingface/hub/models--org--name/weights", size=128 * 1024)
        self.measure()
        items = self.cache.status()["items"]
        self.assertLessEqual(len(items), 20)
        self.assertTrue(any(item["name"] == "org/name" for item in items))
        self.assertEqual(safe_name("x\n\t\x00" + "y" * 200), "x" + "y" * 119)
        self.assertLessEqual(sum(item["kind"] == "pip" for item in items), 1)

    @unittest.skipUnless(os.name == "posix", "Linux atime probe")
    def test_atime_probe_does_not_claim_bind_mount_validation(self):
        self.file("files/test")
        self.measure()
        atime = self.cache.status()["atime"]
        self.assertEqual(atime["scope"], "shared_filesystem")
        self.assertIs(type(atime["updates"]), bool)
        self.assertEqual(list((self.root / "shared").glob(".atime-probe-*")), [])


if __name__ == "__main__":
    unittest.main()
