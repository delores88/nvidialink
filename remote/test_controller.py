import base64
import ipaddress
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

import controller
from controller import Controller, Rejected, canonical, sha, CHUNK, SECRET
from controller import IW_PATHS, NMCLI_PATHS, TOOL_ENV, TOOL_READ, frequency_band, network_link, network_view, run_tool

IMAGE = "sha256:" + "a" * 64
CUDA_IMAGE = "sha256:" + "c" * 64
PROJECT = "b" * 64
# Existing tests never probe the host network; link tests use their own fixtures.
NO_LINK = lambda: {"observed": False}
# Existing tests never depend on the host disk; disk guards have their own tests.
PLENTY_DISK = lambda: (1 << 50, 1 << 50)


class FakeDocker:
    def __init__(self):
        self.containers = {}
        self.networks = {}
        self.calls = []

    def inspect(self, name):
        return self.containers.get(name)

    def network_inspect(self, name):
        return self.networks.get(name)

    def call(self, args, timeout=30):
        self.calls.append(args)
        if args[0] == "create":
            name = args[args.index("--name") + 1]
            if name in self.containers:
                raise Rejected("already_exists", "Container already exists")
            labels = dict(a.split("=", 1) for i, a in enumerate(args) if i and args[i-1] == "--label")
            network = args[args.index("--network")+1]
            # Job and proxy creates both end with IMAGE ENTRYPOINT-SCRIPT; record the real image.
            self.containers[name] = {"Image": args[-2], "Config": {"Labels": labels}, "State": {"Status": "created", "ExitCode": 0}, "NetworkSettings": {"Networks": {network: {}}}}
        elif args[0] == "start":
            self.containers[args[1]]["State"]["Status"] = "running"
        elif args[0] == "stop":
            self.containers[args[-1]]["State"].update(Status="exited", ExitCode=143)
        elif args[0] == "rm":
            del self.containers[args[-1]]
        elif args[:2] == ["network", "create"]:
            labels = dict(a.split("=", 1) for i, a in enumerate(args) if i and args[i-1] == "--label")
            options = dict(a.split("=", 1) for i, a in enumerate(args) if i and args[i-1] == "--opt")
            self.networks[args[-1]] = {"Internal": "--internal" in args, "Labels": labels, "Options": options}
        elif args[:2] == ["network", "connect"]:
            self.containers[args[-1]]["NetworkSettings"]["Networks"][args[-2]] = {}
        elif args[:2] == ["network", "disconnect"]:
            self.containers[args[-1]]["NetworkSettings"]["Networks"].pop(args[-2])
        elif args[:2] == ["network", "rm"]:
            del self.networks[args[-1]]
        return b""


class ControllerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "state"
        self.docker = FakeDocker()
        self.c = Controller(self.root, self.docker, lambda: 120, lambda: {"cpu_count": 20, "memory_gb": 120}, link_probe=NO_LINK, disk_probe=PLENTY_DISK)
        (self.root.parent / "runtimes.json").write_text(json.dumps({"v": 1, "runtimes": {"python": {"image": IMAGE, "provenance": "test fixture"}}}))

    def tearDown(self):
        self.c.db.close()
        def writable(path):
            for root, dirs, files in os.walk(path):
                for name in dirs + files:
                    os.chmod(Path(root) / name, 0o700)
        writable(self.tmp.name)
        self.tmp.cleanup()

    def rpc(self, op, **kwargs):
        result = self.c.rpc({"v": 1, "op": op, **kwargs})
        self.assertTrue(result["ok"], result)
        return result

    def snapshot(self, project=PROJECT, content=b"print('hello')\n", name="main.py"):
        chunks = []
        for start in range(0, len(content), CHUNK):
            raw = content[start:start+CHUNK]
            self.rpc("put", sha256=sha(raw), data=base64.b64encode(raw).decode())
            chunks.append({"sha256": sha(raw), "size": len(raw)})
        entry = {"path": name, "size": len(content), "sha256": sha(content), "mode": 0o644, "chunks": chunks}
        result = self.rpc("sync", project=project, manifest=[entry])
        self.assertEqual(result["manifest_id"], sha(canonical([entry])))
        return result["manifest_id"]

    def run_job(self, project=PROJECT, client="codex", request_id="test-request", **kwargs):
        manifest = self.snapshot(project)
        request = {"project": project, "manifest_id": manifest, "argv": ["python3", "main.py"], "runtime": "python", "client": client, "request_id": request_id, **kwargs}
        return self.rpc("run", **request), request

    def test_resume_chunks_and_immutable_snapshots(self):
        content = b"a" * CHUNK + b"z" * 35
        mid = self.snapshot(content=content)
        first = sha(content[:CHUNK])
        self.assertEqual(self.rpc("missing", hashes=[first, "e"*64])["missing"], ["e"*64])
        self.assertEqual((self.root / "snapshots" / mid / "main.py").read_bytes(), content)
        next_id = self.snapshot(content=b"changed")
        self.assertNotEqual(mid, next_id)
        self.assertEqual((self.root / "snapshots" / mid / "main.py").read_bytes(), content)

    def test_batched_uploads_preserve_order_and_bounds(self):
        chunks = [{"sha256": sha(raw), "data": base64.b64encode(raw).decode()} for raw in (b"first", b"second")]
        first = self.rpc("put_many", chunks=chunks)
        self.assertEqual([c["sha256"] for c in first["accepted"]], [c["sha256"] for c in chunks])
        self.assertEqual(first["bytes"], 11)
        self.assertEqual(self.rpc("put_many", chunks=chunks), first)
        self.assertEqual(self.c.rpc({"v": 1, "op": "put_many", "chunks": chunks*65})["error"]["code"], "size_limit")

    def test_hash_corruption_and_missing_chunks(self):
        result = self.c.rpc({"v": 1, "op": "put", "sha256": "f"*64, "data": "eA=="})
        self.assertEqual(result["error"]["code"], "hash_mismatch")
        mid = self.snapshot()
        row = self.c.db.execute("SELECT manifest FROM manifests WHERE id=?", (mid,)).fetchone()
        entries = json.loads(row[0])
        entries[0]["chunks"][0]["sha256"] = "f"*64
        result = self.c.rpc({"v": 1, "op": "sync", "project": PROJECT, "manifest": entries})
        self.assertEqual(result["error"]["code"], "missing_chunks")

    def test_path_secret_and_mode_rejection(self):
        for path in ("../outside", "/absolute", "dir\\evil", "C:/outside", "a//b", ".ssh/config", "keys/test.pem", ".env", "target/file", "dir/../evil"):
            result = self.c.rpc({"v": 1, "op": "sync", "project": PROJECT, "manifest": [{"path": path, "size": 0, "sha256": sha(b""), "mode": 0o644, "chunks": []}]})
            self.assertFalse(result["ok"], path)
        raw = b"-----BEGIN " + b"PRIVATE KEY-----"
        result = self.c.rpc({"v": 1, "op": "put", "sha256": sha(raw), "data": base64.b64encode(raw).decode()})
        self.assertEqual(result["error"]["code"], "secret_detected")

    def test_secret_spanning_chunks_rejected_before_snapshot(self):
        pieces = [b"a"*(CHUNK-10) + b"-----BEGIN", b" PRIVATE KEY-----"]
        chunks = []
        for raw in pieces:
            self.rpc("put", sha256=sha(raw), data=base64.b64encode(raw).decode())
            chunks.append({"sha256": sha(raw), "size": len(raw)})
        raw = b"".join(pieces)
        result = self.c.rpc({"v": 1, "op": "sync", "project": PROJECT, "manifest": [{"path": "source.txt", "size": len(raw), "sha256": sha(raw), "mode": 0o644, "chunks": chunks}]})
        self.assertEqual(result["error"]["code"], "secret_detected")

    def test_idempotence_and_worktree_claim(self):
        first, request = self.run_job()
        repeat = self.rpc("run", **request)
        self.assertEqual(first["job"], repeat["job"])
        request["argv"] = ["python3", "different.py"]
        self.assertEqual(self.c.rpc({"v": 1, "op": "run", **request})["error"]["code"], "idempotency_conflict")
        request["request_id"] = "second"
        self.assertEqual(self.c.rpc({"v": 1, "op": "run", **request})["error"]["code"], "worktree_claimed")

    def test_restart_reconciles_without_duplicate_launch(self):
        first, _ = self.run_job()
        self.c.tick()
        self.assertEqual(self.c.get_job(first["job"])["state"], "running")
        self.c.db.close()
        self.c = Controller(self.root, self.docker, lambda: 120, lambda: {"cpu_count": 20, "memory_gb": 120}, link_probe=NO_LINK, disk_probe=PLENTY_DISK)
        self.c.tick()
        self.assertEqual(sum(call[0] == "create" for call in self.docker.calls), 1)
        self.assertEqual(sum(call[0] == "start" for call in self.docker.calls), 1)
        self.docker.containers["sparkdev-job-" + first["job"]]["State"].update(Status="exited", ExitCode=7)
        self.c.tick()
        final = self.rpc("wait", job=first["job"], timeout=0)
        self.assertEqual((final["state"], final["exit_code"]), ("failed", 7))

    def test_crash_after_create_before_start(self):
        first, _ = self.run_job()
        self.c.db.execute("UPDATE jobs SET state='starting' WHERE id=?", (first["job"],))
        self.docker.call(self.c.create_args(self.c.get_job(first["job"])))
        self.c.tick()
        self.assertEqual(sum(call[0] == "create" for call in self.docker.calls), 1)
        self.assertEqual(sum(call[0] == "start" for call in self.docker.calls), 1)

    def test_unknown_docker_state_never_relaunches(self):
        first, _ = self.run_job()
        self.c.tick()
        self.docker.containers.clear()
        self.c.tick()
        self.assertEqual(self.c.get_job(first["job"])["error"], "container_lost")
        self.assertEqual(sum(call[0] == "create" for call in self.docker.calls), 1)

    def test_cancel_queued_and_running(self):
        first, _ = self.run_job()
        self.rpc("cancel", job=first["job"])
        self.c.tick()
        self.assertEqual(self.rpc("wait", job=first["job"])["state"], "cancelled")
        self.assertFalse(self.docker.calls)
        second, _ = self.run_job(request_id="second")
        self.c.tick()
        self.rpc("cancel", job=second["job"])
        self.c.tick()
        self.assertEqual(self.c.get_job(second["job"])["state"], "cancelled")
        self.assertEqual(sum(call[0] == "stop" for call in self.docker.calls), 1)

    def test_logs_byte_offsets_binary_and_idempotence(self):
        first, _ = self.run_job()
        output = self.root / "jobs" / first["job"] / "output"
        output.mkdir(parents=True, exist_ok=True)
        (output / "stdout.log").write_bytes(b"a\xffb\nc")
        chunk = self.rpc("logs", job=first["job"], since=1, limit=3)
        self.assertEqual(base64.b64decode(chunk["data"]), b"\xffb\n")
        self.assertEqual(chunk["next_offset"], 4)
        self.assertEqual(chunk, self.rpc("logs", job=first["job"], since=1, limit=3))
        self.assertEqual(self.c.rpc({"v": 1, "op": "logs", "job": first["job"], "since": 100})["error"]["code"], "invalid_offset")

    def test_success_cache_has_new_receipt_and_bypass(self):
        first, request = self.run_job()
        self.c.tick()
        self.docker.containers["sparkdev-job-" + first["job"]]["State"].update(Status="exited", ExitCode=0)
        self.c.tick()
        request["request_id"] = "again"
        second = self.rpc("run", **request)
        self.assertTrue(second["cache_hit"])
        self.assertNotEqual(second["job"], first["job"])
        self.assertEqual(second["cached_from"], first["job"])
        request.update(request_id="bypass", no_cache=True)
        third = self.rpc("run", **request)
        self.assertFalse(third["cache_hit"])
        self.assertEqual(third["state"], "queued")

    def test_failure_never_cached(self):
        first, request = self.run_job()
        self.c.finish(self.c.get_job(first["job"]), "failed", 1)
        request["request_id"] = "retry"
        self.assertFalse(self.rpc("run", **request)["cache_hit"])

    def test_fairness_per_client_cpu_and_memory(self):
        for index, client in enumerate(("claude", "claude", "claude", "codex", "human")):
            self.run_job(project=sha(str(index).encode()), client=client, request_id="request"+str(index))
        for _ in range(10):
            self.c.tick()
        active = self.c.db.execute("SELECT client FROM jobs WHERE state='running'").fetchall()
        self.assertEqual(len(active), 4)
        self.assertLessEqual(sum(row[0] == "claude" for row in active), 2)
        self.assertIn("codex", [r[0] for r in active])
        self.assertIn("human", [r[0] for r in active])
        starts = [r[0] for r in self.c.db.execute("SELECT client FROM audit WHERE kind='job_started' ORDER BY seq")]
        self.assertEqual(starts[:3], ["claude", "codex", "human"])

    def test_admission_fails_closed_below_headroom(self):
        first, _ = self.run_job()
        self.c.memory_probe = lambda: 39
        self.c.tick()
        self.assertEqual(self.c.get_job(first["job"])["state"], "queued")
        self.assertFalse(self.docker.calls)

    def test_resource_policy_is_hardware_bounded_and_persists(self):
        initial = self.rpc("resources")
        self.assertFalse(initial["policy_saved"])
        self.assertEqual(initial["limits"]["headroom_gb"], 32)
        limits = {**initial["limits"], "cpu_per_job": 6, "memory_gb_per_job": 12,
                  "cpu_total": 18, "memory_gb_total": 80, "client_jobs": 3, "client_cpu": 12, "headroom_gb": 24}
        saved = self.rpc("resources", limits=limits)
        self.assertEqual(saved["limits"], limits)
        self.assertTrue(saved["policy_saved"])
        self.c.db.close()
        self.c = Controller(self.root, self.docker, lambda: 120, lambda: {"cpu_count": 20, "memory_gb": 120}, link_probe=NO_LINK, disk_probe=PLENTY_DISK)
        self.assertEqual(self.rpc("status")["limits"], limits)
        for key, value in (("cpu_per_job", 21), ("cpu_per_job", 13), ("client_cpu", 19),
                           ("cpu_total", 0), ("memory_gb_total", 100), ("memory_gb_per_job", 81),
                           ("headroom_gb", 0), ("client_jobs", 65), ("client_jobs", True)):
            bad = self.c.rpc({"v": 1, "op": "resources", "limits": {**limits, key: value}})
            self.assertFalse(bad["ok"], (key, value))
            self.assertEqual(self.rpc("resources")["limits"], limits)
        self.assertFalse(self.c.rpc({"v": 1, "op": "resources", "limits": {**limits, "arbitrary": 1}})["ok"])

    def test_lower_caps_preserve_existing_jobs_and_block_new_admission(self):
        first, _ = self.run_job()
        self.c.tick()
        queued, _ = self.run_job(project=sha(b"queued"), request_id="queued")
        limits = self.rpc("resources")["limits"]
        limits.update(cpu_per_job=2, cpu_total=2, client_cpu=2, memory_gb_per_job=4,
                      memory_gb_total=16, headroom_gb=40, client_jobs=1)
        self.rpc("resources", limits=limits)
        self.c.tick()
        self.assertEqual(self.c.get_job(first["job"])["state"], "running")
        self.assertEqual(self.c.get_job(first["job"])["cpu"], 4)
        self.assertEqual(self.c.get_job(first["job"])["headroom_gb"], 32)
        self.assertFalse(any(call[0] == "stop" for call in self.docker.calls))
        self.assertEqual(self.c.get_job(queued["job"])["state"], "queued")
        self.assertIn("admission_blocked", next(j for j in self.rpc("status")["jobs"] if j["job"] == queued["job"]))
        new, _ = self.run_job(project=sha(b"new"), request_id="new")
        self.assertEqual((new["cpu"], new["memory_gb"], new["headroom_gb"]), (2, 4, 40))

    def test_explicit_policy_clamps_legacy_project_allocations(self):
        (self.root.parent / "limits.json").write_text(json.dumps({"v": 1, "projects": {PROJECT: {"cpu": 8, "memory_gb": 32, "scratch_mb": 4096}}}))
        limits = self.rpc("resources")["limits"]
        self.rpc("resources", limits=limits)
        job, _ = self.run_job()
        self.assertEqual((job["cpu"], job["memory_gb"]), (4, 8))
        self.assertEqual(self.rpc("resources")["legacy_project_overrides"], "clamped_to_per_job_caps")

    def test_small_hardware_preparation_fits_default_budget(self):
        self.c.hardware_probe = lambda: {"cpu_count": 2, "memory_gb": 4}
        policy = self.rpc("resources")
        self.assertEqual(policy["limits"]["cpu_total"], 2)
        self.assertEqual(policy["limits"]["memory_gb_total"] + policy["limits"]["headroom_gb"], 4)
        mid = self.snapshot()
        result = self.rpc("prepare", project=PROJECT, manifest_id=mid,
                          runtime="python", client="codex", request_id="small-prepare")
        self.assertEqual((result["reserved_cpu"], result["reserved_memory_gb"]), (2, 2))
        self.assertEqual((result["cpu"], result["memory_gb"]), (1.75, 1.875))
        self.c.tick()
        self.assertEqual(self.c.get_job(result["job"])["state"], "running")

    def test_preparation_reserves_proxy_inside_exact_job_client_and_total_caps(self):
        limits = {"cpu_per_job": 16, "memory_gb_per_job": 32, "cpu_total": 16,
                  "memory_gb_total": 32, "client_jobs": 4, "client_cpu": 16, "headroom_gb": 32}
        self.rpc("resources", limits=limits)
        mid = self.snapshot()
        request = dict(project=PROJECT, manifest_id=mid, runtime="python", client="codex", request_id="exact-prepare")
        result = self.rpc("prepare", **request)
        self.assertEqual((result["cpu"], result["memory_gb"]), (15.75, 31.875))
        self.assertEqual((result["reserved_cpu"], result["reserved_memory_gb"]), (16, 32))
        self.assertEqual(self.rpc("prepare", **request)["job"], result["job"])
        self.c.tick()
        self.assertEqual(self.c.get_job(result["job"])["state"], "running")
        calls = [call for call in self.docker.calls if call[0] == "create"]
        self.assertEqual([call[call.index("--cpus") + 1] for call in calls], ["0.25", "15.75"])
        self.assertEqual([call[call.index("--memory") + 1] for call in calls], ["128m", "31.875g"])
        self.assertIn("CARGO_BUILD_JOBS=15", calls[1])
        # A different client still cannot consume quota reserved by the proxy.
        second, _ = self.run_job(project=sha(b"second"), client="human", request_id="second")
        self.c.tick()
        self.assertEqual(self.c.get_job(second["job"])["state"], "queued")

    def test_minimum_preparation_budget_and_owner_project_override(self):
        limits = {"cpu_per_job": 1, "memory_gb_per_job": 1, "cpu_total": 1,
                  "memory_gb_total": 1, "client_jobs": 4, "client_cpu": 1, "headroom_gb": 32}
        self.rpc("resources", limits=limits)
        mid = self.snapshot()
        result = self.rpc("prepare", project=PROJECT, manifest_id=mid,
                          runtime="python", client="codex", request_id="minimum-prepare")
        self.assertEqual((result["cpu"], result["memory_gb"], result["scratch_mb"]), (0.75, 0.875, 448))
        self.assertEqual((result["reserved_cpu"], result["reserved_memory_gb"]), (1, 1))
        self.c.tick()
        argv = next(call for call in self.docker.calls if call[0] == "create" and "CARGO_BUILD_JOBS=1" in call)
        self.assertEqual(argv[argv.index("--memory") + 1], "0.875g")
        self.assertIn("/tmp:rw,nosuid,nodev,size=469762048", argv)
        # Keep the same small request budget under a larger saved owner policy.
        limits.update(cpu_per_job=16, client_cpu=16, cpu_total=18, memory_gb_per_job=32, memory_gb_total=80)
        self.rpc("resources", limits=limits)
        project = sha(b"owner-override")
        (self.root.parent / "limits.json").write_text(json.dumps({"v": 1, "projects": {project: {"cpu": 1, "memory_gb": 1}}}))
        mid = self.snapshot(project=project)
        small = self.rpc("prepare", project=project, manifest_id=mid,
                         runtime="python", client="codex", request_id="override-prepare")
        self.assertEqual((small["reserved_cpu"], small["reserved_memory_gb"]), (1, 1))

    def test_old_preparation_allocation_cannot_bypass_new_per_job_caps(self):
        mid = self.snapshot()
        result = self.rpc("prepare", project=PROJECT, manifest_id=mid,
                          runtime="python", client="codex", request_id="legacy-prepare")
        # Existing pre-fix rows included a full worker allocation plus the proxy.
        self.c.db.execute("UPDATE jobs SET cpu=4,memory=8 WHERE id=?", (result["job"],))
        limits = self.rpc("resources")["limits"]
        self.rpc("resources", limits=limits)
        self.c.tick()
        row = self.c.get_job(result["job"])
        self.assertEqual((row["state"], row["cpu"], row["memory"]), ("queued", 4, 8))
        self.assertIn("admission_blocked", next(j for j in self.rpc("status")["jobs"] if j["job"] == row["id"]))
        self.assertFalse(self.docker.calls)

    def test_owner_project_limits_are_bounded_and_reflected_in_receipt(self):
        (self.root.parent / "limits.json").write_text(json.dumps({"v": 1, "projects": {PROJECT: {"cpu": 8, "memory_gb": 32, "scratch_mb": 4096}}}))
        first, _ = self.run_job()
        self.assertEqual((first["cpu"], first["memory_gb"]), (8, 32))
        argv = self.c.create_args(self.c.get_job(first["job"]))
        self.assertEqual(argv[argv.index("--memory")+1], "32g")
        self.assertEqual(argv[argv.index("--cpus")+1], "8")
        self.assertIn("CARGO_BUILD_JOBS=8", argv)
        self.assertEqual(first["scratch_mb"], 4096)
        self.assertIn("/tmp:rw,nosuid,nodev,size=4294967296", argv)

    def test_owner_scratch_policy_is_bounded_by_memory_and_part_of_cache_key(self):
        first, _ = self.run_job()
        self.c.db.execute("UPDATE jobs SET state='succeeded',exit_code=0,finished=1 WHERE id=?", (first["job"],))
        profile = self.root.parent / "limits.json"
        profile.write_text(json.dumps({"v": 1, "projects": {PROJECT: {"cpu": 4, "memory_gb": 8, "scratch_mb": 1024}}}))
        second, request = self.run_job(request_id="new-scratch")
        self.assertFalse(second["cache_hit"])
        self.assertEqual(second["scratch_mb"], 1024)
        self.c.db.execute("UPDATE jobs SET state='succeeded',exit_code=0,finished=1 WHERE id=?", (second["job"],))
        for memory, scratch in ((8, 4097), (1, 1024), (8, 127), (8, True)):
            profile.write_text(json.dumps({"v": 1, "projects": {PROJECT: {"cpu": 4, "memory_gb": memory, "scratch_mb": scratch}}}))
            with self.assertRaises(Rejected):
                self.c.run({"v": 1, "op": "run", **request, "request_id": "invalid-scratch"})

    def test_gpu_watchdog_stops_only_own_job(self):
        (self.root.parent / "runtimes.json").write_text(json.dumps({"v": 1, "runtimes": {"cuda": {"image": IMAGE, "gpu": True, "provenance": "test fixture"}}}))
        mid = self.snapshot()
        job = self.rpc("run", project=PROJECT, manifest_id=mid, runtime="cuda", client="codex", request_id="gpu", argv=["python", "main.py"])
        self.c.tick()
        changed = self.rpc("resources")["limits"]
        changed["headroom_gb"] = 40
        self.rpc("resources", limits=changed)
        self.c.memory_probe = lambda: 36
        self.c.tick()
        self.assertEqual(self.c.get_job(job["job"])["state"], "running")
        self.c.memory_probe = lambda: 31
        self.c.tick()
        row = self.c.get_job(job["job"])
        self.assertEqual((row["state"], row["error"]), ("failed", "memory_watchdog"))
        self.assertEqual([c[-1] for c in self.docker.calls if c[0] == "stop"], ["sparkdev-job-" + job["job"]])

    def cuda_registry(self, python=True):
        runtimes = {"cuda": {"image": CUDA_IMAGE, "gpu": True, "provenance": "test CUDA fixture"}}
        if python:
            runtimes["python"] = {"image": IMAGE, "provenance": "test fixture"}
        (self.root.parent / "runtimes.json").write_text(json.dumps({"v": 1, "runtimes": runtimes}))

    def test_cuda_preparation_uses_cuda_image_python_proxy_and_no_gpu(self):
        self.cuda_registry()
        mid = self.snapshot()
        job = self.rpc("prepare", project=PROJECT, manifest_id=mid, runtime="cuda", client="claude", request_id="cuda-prepare")
        self.assertEqual(job["runtime_digest"], CUDA_IMAGE)
        row = self.c.get_job(job["job"])
        self.assertEqual((row["image"], row["proxy_image"]), (CUDA_IMAGE, IMAGE))
        self.c.tick()
        self.assertEqual(self.c.get_job(job["job"])["state"], "running")
        proxy, worker = [c for c in self.docker.calls if c[0] == "create"]
        self.assertEqual(proxy[-2:], [IMAGE, "/sparkdev-proxy.py"])
        self.assertEqual(self.docker.containers["sparkdev-proxy-" + job["job"]]["Image"], IMAGE)
        self.assertEqual(worker[-2:], [CUDA_IMAGE, "/sparkdev-worker.py"])
        self.assertEqual(self.docker.containers["sparkdev-job-" + job["job"]]["Image"], CUDA_IMAGE)
        self.assertNotIn("--gpus", worker)
        self.assertIn("NVIDIA_VISIBLE_DEVICES=void", worker)
        self.assertNotIn("NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics", worker)
        self.assertNotIn("--runtime", worker)
        self.assertIn("HTTPS_PROXY=http://sparkdev-proxy:3128", worker)
        self.assertEqual(worker[worker.index("--network") + 1], "sparkdev-fetch-" + job["job"])
        pip = self.root / "caches" / PROJECT / CUDA_IMAGE.split(":", 1)[1] / "pip"
        self.assertIn(f"type=bind,src={pip},dst=/pip-cache", worker)
        self.assertTrue(pip.is_dir())
        argv = (self.root / "jobs" / job["job"] / "argv.json").read_bytes()
        self.assertEqual(argv, canonical(["/usr/bin/python3", "/sparkdev-prepare.py", "cuda"]))
        self.assertTrue((self.root / "jobs" / job["job"] / "prepare.py").is_file())

    def test_cuda_preparation_requires_python_proxy_image(self):
        self.cuda_registry(python=False)
        mid = self.snapshot()
        result = self.c.rpc({"v": 1, "op": "prepare", "project": PROJECT, "manifest_id": mid, "runtime": "cuda",
                             "client": "claude", "request_id": "cuda-only"})
        self.assertEqual(result["error"]["code"], "runtime_unavailable")
        self.assertEqual(self.c.db.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], 0)
        self.assertEqual(self.c.rpc({"v": 1, "op": "prepare", "project": PROJECT, "manifest_id": mid, "runtime": "go",
                                     "client": "claude", "request_id": "go"})["error"]["code"], "invalid_request")

    def test_cuda_run_keeps_gpu_and_no_network(self):
        self.cuda_registry()
        mid = self.snapshot()
        job = self.rpc("run", project=PROJECT, manifest_id=mid, runtime="cuda", client="claude", request_id="cuda-run", argv=["python3", "main.py"])
        self.assertIsNone(self.c.get_job(job["job"])["proxy_image"])
        argv = self.c.create_args(self.c.get_job(job["job"]))
        self.assertEqual(argv[argv.index("--gpus") + 1], "all")
        self.assertEqual(argv[argv.index("--network") + 1], "none")
        self.assertIn("NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics", argv)
        self.assertNotIn("NVIDIA_VISIBLE_DEVICES=void", argv)
        self.assertNotIn("HTTPS_PROXY=http://sparkdev-proxy:3128", argv)

    def test_watchdog_spares_cuda_preparation_and_stops_cuda_run(self):
        self.cuda_registry()
        prepared = self.rpc("prepare", project=PROJECT, manifest_id=self.snapshot(), runtime="cuda", client="claude", request_id="cuda-prepare")
        other = sha(b"cuda-run")
        run = self.rpc("run", project=other, manifest_id=self.snapshot(project=other), runtime="cuda", client="codex",
                       request_id="cuda-run", argv=["python3", "main.py"])
        for _ in range(3):
            self.c.tick()
        self.assertEqual([self.c.get_job(j["job"])["state"] for j in (prepared, run)], ["running", "running"])
        self.c.memory_probe = lambda: 20
        self.c.tick()
        self.assertEqual(self.c.get_job(prepared["job"])["state"], "running")
        row = self.c.get_job(run["job"])
        self.assertEqual((row["state"], row["exit_code"], row["error"]), ("failed", 137, "memory_watchdog"))
        self.assertEqual([c[-1] for c in self.docker.calls if c[0] == "stop"], ["sparkdev-job-" + run["job"]])

    def test_preparable_runtimes_require_the_python_proxy_image(self):
        self.assertEqual(self.rpc("status")["preparable_runtimes"], ["python"])
        self.cuda_registry()
        status = self.rpc("status")
        self.assertEqual(sorted(status["preparable_runtimes"]), ["cuda", "python"])
        self.assertEqual(sorted(status["runtimes"]), ["cuda", "python"])
        self.cuda_registry(python=False)
        status = self.rpc("status")
        self.assertEqual((status["runtimes"], status["preparable_runtimes"]), (["cuda"], []))
        (self.root.parent / "runtimes.json").unlink()
        self.assertEqual(self.rpc("status")["preparable_runtimes"], [])

    def test_docker_unavailability_never_means_missing(self):
        job, _ = self.run_job()
        self.c.db.execute("UPDATE jobs SET state='starting' WHERE id=?", (job["job"],))
        def unavailable(name):
            raise Rejected("docker_unavailable", "Fixture")
        self.docker.inspect = unavailable
        with self.assertRaises(Rejected):
            self.c.tick()
        self.assertFalse(self.docker.calls)
        self.assertEqual(self.c.get_job(job["job"])["state"], "starting")

    def test_container_hardening_and_no_shell(self):
        first, _ = self.run_job()
        argv = self.c.create_args(self.c.get_job(first["job"]))
        for option, value in (("--network", "none"), ("--memory", "8g"), ("--cpus", "4"), ("--pids-limit", "1024"), ("--cap-drop", "ALL"), ("--entrypoint", "/usr/bin/python3")):
            self.assertEqual(argv[argv.index(option)+1], value)
        self.assertIn("--read-only", argv)
        self.assertNotIn("sudo", argv)
        self.assertNotIn("/var/run/docker.sock", " ".join(argv))
        self.assertEqual(argv[-2:], [IMAGE, "/sparkdev-worker.py"])

    def test_laptop_claim_shared_board_logs_and_release(self):
        claim = self.rpc("claim", project=PROJECT, client="human", request_id="local", job="laptop-test", ttl=60)
        first = self.rpc("heartbeat", lease=claim["lease"], job="laptop-test", data="aGVsbG8=", since=0)
        self.assertEqual(first["next_offset"], 5)
        self.assertEqual(self.rpc("heartbeat", lease=claim["lease"], data="aGVsbG8=", since=0)["next_offset"], 5)
        self.assertEqual(self.rpc("logs", job="laptop-test", since=0)["text"], "hello")
        mid = self.snapshot()
        request = {"v": 1, "op": "run", "project": PROJECT, "manifest_id": mid, "argv": ["true"], "runtime": "python", "client": "codex", "request_id": "remote"}
        self.assertEqual(self.c.rpc(request)["error"]["code"], "worktree_claimed")
        self.rpc("heartbeat", lease=claim["lease"], state="succeeded", exit_code=0)
        self.assertEqual(self.rpc("heartbeat", lease=claim["lease"], state="succeeded", exit_code=0, data="IQ==", since=5)["next_offset"], 6)
        self.assertEqual(self.rpc("heartbeat", lease=claim["lease"], state="succeeded", exit_code=0, data="IQ==", since=5)["next_offset"], 6)
        self.assertTrue(self.c.rpc(request)["ok"])
        self.assertEqual(len(self.rpc("status", all=True)["jobs"]), 2)

    def test_expired_laptop_claim_blocks_until_stopped(self):
        claim = self.rpc("claim", project=PROJECT, client="human", request_id="local")
        self.c.db.execute("UPDATE leases SET expires=0")
        again = self.c.rpc({"v": 1, "op": "claim", "project": PROJECT, "client": "codex", "request_id": "second"})
        self.assertEqual(again["error"]["code"], "worktree_claimed")
        self.assertEqual(self.c.rpc({"v": 1, "op": "heartbeat", "lease": claim["lease"]})["error"]["code"], "lease_expired")
        self.rpc("release", lease=claim["lease"])
        self.rpc("claim", project=PROJECT, client="codex", request_id="second")

    def test_registry_closed_and_no_dependency_escape(self):
        result = self.c.rpc({"v": 1, "op": "prepare", "network": "host"})
        self.assertEqual(result["error"]["code"], "invalid_request")
        (self.root.parent / "runtimes.json").write_text(json.dumps({"v": 1, "runtimes": {"python": {"image": "python:latest", "provenance": "bad"}}}))
        self.assertEqual(self.c.rpc({"v": 1, "op": "status"})["error"]["code"], "invalid_registry")

    def test_preparation_uses_isolated_proxy_and_cleans_after_restart(self):
        mid = self.snapshot()
        response = self.rpc("prepare", project=PROJECT, manifest_id=mid, runtime="python", client="codex", request_id="prepare")
        self.c.tick()
        row = self.c.get_job(response["job"])
        self.assertEqual(row["state"], "running")
        network = self.docker.networks["sparkdev-fetch-" + row["id"]]
        self.assertTrue(network["Internal"])
        self.assertEqual(network["Options"]["com.docker.network.bridge.gateway_mode_ipv4"], "isolated")
        calls = [c for c in self.docker.calls if c[0] == "create"]
        self.assertEqual(len(calls), 2)
        self.assertIn("HTTPS_PROXY=http://sparkdev-proxy:3128", calls[1])
        self.assertNotIn("--network host", " ".join(calls[1]))
        self.c.tick()
        self.assertEqual(sum(c[0] == "create" for c in self.docker.calls), 2)
        self.docker.containers["sparkdev-job-" + row["id"]]["State"].update(Status="exited", ExitCode=0)
        self.c.tick()
        self.assertNotIn("sparkdev-proxy-" + row["id"], self.docker.containers)
        self.assertFalse(self.docker.networks)
        self.assertEqual(self.c.get_job(row["id"])["proxy_cleaned"], 1)

    def test_audit_chain_no_argv_or_raw_configuration(self):
        self.run_job()
        previous = "0"*64
        rows = self.c.db.execute("SELECT * FROM audit ORDER BY seq").fetchall()
        self.assertGreater(len(rows), 0)
        for row in rows:
            self.assertEqual(row["prev"], previous)
            self.assertEqual(row["hash"], sha(bytes.fromhex(previous) + row["data"].encode()))
            self.assertNotIn("argv", row["data"])
            previous = row["hash"]

    def test_unknown_fields_and_types_fail_closed(self):
        for request in ({"v": True, "op": "status"}, {"v": 1, "op": "status", "network": "host"}, {"v": 1, "op": []}, {"v": 1, "op": "missing", "hashes": ["../oops"]}):
            self.assertFalse(self.c.rpc(request)["ok"], request)


SSID = "OwnerHome-5521"
BSSID = "a4:2b:b0:11:22:33"
MAC = "48:b0:2d:aa:bb:cc"
CLIENT_IP = "192.168.1.50"
SPARK_IP = "192.168.1.20"
WIRED_IP = "192.168.1.21"
GATEWAY = "192.168.1.1"
ANY, MASK24 = "0.0.0.0", "255.255.255.0"
SECRETS = (SSID, BSSID, MAC, CLIENT_IP, SPARK_IP, WIRED_IP, GATEWAY)
IW_LINK = f"""Connected to {BSSID} (on wlP9s9)
\tSSID: {SSID}
\tfreq: 2462
\tRX: 1234567 bytes (2345 packets)
\tTX: 234567 bytes (1234 packets)
\tsignal: -52 dBm
\trx bitrate: 72.2 MBit/s MCS 7 short GI
\ttx bitrate: 144.4 MBit/s MCS 15 40MHz short GI
\tbss flags:\tshort-slot-time
\tdtim period:\t1
\tbeacon int:\t100
"""
IW_INFO = f"""Interface wlP9s9
\tifindex 4
\twdev 0x1
\taddr {MAC}
\tssid {SSID}
\ttype managed
\twiphy 0
\tchannel 11 (2462 MHz), width: 20 MHz, center1: 2462 MHz
\ttxpower 22.00 dBm
"""
NMCLI = ["/usr/bin/nmcli", "-t", "-e", "yes", "-f", "ACTIVE,FREQ,CHAN,BANDWIDTH", "device", "wifi", "list",
         "ifname", "wlP9s9", "--rescan", "no"]
NO_RADIO = {"tx_bitrate_mbps": None, "rx_bitrate_mbps": None}


def native4(address):
    raw = ipaddress.IPv4Address(address).packed
    return (raw[::-1] if sys.byteorder == "little" else raw).hex().upper()


def native16(address):
    raw = ipaddress.IPv6Address(address).packed
    if sys.byteorder == "little":
        raw = b"".join(raw[i:i + 4][::-1] for i in range(0, 16, 4))
    return raw.hex().upper()


def put(path, text):
    path.write_bytes(text.encode())


class LinkFixture:
    """Synthetic /sys and /proc trees; `device` is a plain directory, as the spec requires."""

    def __init__(self, root):
        self.sys, self.proc = root / "sys", root / "proc"
        (self.sys / "class" / "net").mkdir(parents=True)
        (self.proc / "net").mkdir(parents=True)
        self.files = {name: [] for name in ("route", "ipv6_route", "tcp", "tcp6", "if_inet6", "wireless")}
        self.addresses, self.outputs, self.absent, self.calls, self.environ = {}, {}, set(), [], {}

    def iface(self, name, kind, carrier=None, operstate="down", speed=None, driver=None, lower=(), ipv4=None, wlan="phy80211", upper=()):
        entry = self.sys / "class" / "net" / name
        entry.mkdir()
        put(entry / "operstate", operstate + "\n")
        put(entry / "type", "32\n" if kind == "infiniband" else "1\n")
        put(entry / "address", MAC + "\n")
        uevent = f"INTERFACE={name}\n"
        if kind != "virtual":
            (entry / "device").mkdir()
            put(entry / "device" / "uevent", f"DRIVER={driver}\nPCI_SLOT_NAME=0000:01:00.0\n" if driver else "")
        if kind == "wifi" and wlan == "phy80211":
            (entry / "phy80211").mkdir()
        elif kind == "wifi":
            uevent += "DEVTYPE=wlan\n"
        put(entry / "uevent", uevent)
        if carrier is not None:  # A missing file stands in for EINVAL on a down link.
            put(entry / "carrier", "1\n" if carrier else "0\n")
        if speed is not None:
            put(entry / "speed", f"{speed}\n")
        for member in lower:
            (entry / ("lower_" + member)).mkdir()
        for master in upper:
            (entry / ("upper_" + master)).mkdir()
        if ipv4:
            self.addresses[name] = ipaddress.IPv4Address(ipv4)

    def route(self, name, destination, mask, gateway=ANY, metric=0):
        flags = 0x1 | (0x2 if gateway != ANY else 0)
        self.files["route"].append(f"{name}\t{native4(destination)}\t{native4(gateway)}\t{flags:04X}\t0\t0\t{metric}\t{native4(mask)}\t0\t0\t0")

    def route6(self, name, destination, prefix, metric=1024, flags=0x3):
        self.files["ipv6_route"].append(f"{ipaddress.IPv6Address(destination).packed.hex()} {prefix:02x} {'0' * 32} 00 "
                                        f"{'0' * 32} {metric:08x} 00000001 00000000 {flags:08x} {name:>8}")

    def socket(self, client, client_port, server, server_port, state="01"):
        six = ":" in server
        encode = native16 if six else native4
        self.files["tcp6" if six else "tcp"].append(
            f"   0: {encode(server)}:{server_port:04X} {encode(client)}:{client_port:04X} {state} "
            "00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000000000000000 20 4 30 10 -1")

    def session(self, client, client_port, server, server_port, live=True):
        self.environ["SSH_CONNECTION"] = f"{client} {client_port} {server} {server_port}"
        if live:
            self.socket(client.split("%")[0], client_port, server.split("%")[0], server_port)

    def inet6(self, name, address, prefix=64):
        self.files["if_inet6"].append(f"{ipaddress.IPv6Address(address).packed.hex()} 04 {prefix:02x} 00 80 {name:>8}")

    def wireless(self, name, level):
        self.files["wireless"].append(f"{name}: 0000   70.  {level}.  -256        0      0      0      0      0        0")

    def write(self):
        headers = {"route": "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT",
                   "tcp": "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode",
                   "tcp6": "  sl  local_address                         remote_address                        st tx_queue",
                   "wireless": "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n"
                               " face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22"}
        for name, lines in self.files.items():
            put(self.proc / "net" / name, "\n".join(([headers[name]] if name in headers else []) + lines) + "\n")

    def run(self, argv, timeout):
        self.calls.append((list(argv), timeout))
        if argv[0] in self.absent:
            raise FileNotFoundError(argv[0])
        value = self.outputs.get("nmcli" if argv[0] in NMCLI_PATHS else argv[-1])
        if isinstance(value, BaseException):
            raise value
        return value.encode() if isinstance(value, str) else value

    def ipv4(self, name):
        return self.addresses.get(name)

    def probe(self):
        self.write()
        return network_link(self.sys, self.proc, self.environ, self.run, self.ipv4, os_name="posix")

    def view(self):
        return network_view(self.probe())


class NetworkLinkTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.f = LinkFixture(Path(self.tmp.name))

    def tearDown(self):
        self.tmp.cleanup()

    def reports(self, probe):
        c = Controller(Path(self.tmp.name) / "state", FakeDocker(), lambda: 120, lambda: {"cpu_count": 20, "memory_gb": 120}, link_probe=probe)
        try:
            return c.rpc({"v": 1, "op": "status"}), c.rpc({"v": 1, "op": "doctor"})
        finally:
            c.db.close()

    def owner_spark(self):
        """The owner's Spark: 2.4 GHz Wi-Fi, all five wired ports down, Docker running."""
        f = self.f
        f.iface("enP7s7", "wired", carrier=False, driver="r8127", ipv4=None)
        for name in ("enp1s0f0np0", "enp1s0f1np1", "enP2p1s0f0np0", "enP2p1s0f1np1"):
            f.iface(name, "wired", carrier=False, driver="mlx5_core")
        f.iface("wlP9s9", "wifi", carrier=True, operstate="up", driver="mt7925e", ipv4=SPARK_IP)
        f.iface("docker0", "virtual", carrier=False, ipv4="172.17.0.1")
        f.iface("lo", "virtual", carrier=True, operstate="unknown", ipv4="127.0.0.1")
        f.route("wlP9s9", ANY, ANY, gateway=GATEWAY, metric=600)
        f.route("wlP9s9", "192.168.1.0", MASK24, metric=600)
        f.route("docker0", "172.17.0.0", "255.255.0.0")
        f.session(CLIENT_IP, 51234, SPARK_IP, 22)
        f.outputs.update(link=IW_LINK, info=IW_INFO)

    def test_owner_spark_on_2_4_ghz_with_every_wired_port_down(self):
        self.owner_spark()
        reads = []
        real = controller.bounded_text
        def spy(path, *args):
            reads.append(Path(path).name)
            return real(path, *args)
        with mock.patch.object(controller, "bounded_text", spy):
            status, doctor = self.reports(self.f.probe)
        self.assertTrue(status["ok"] and doctor["ok"])
        wired = [{"interface": name, "carrier": False, "operstate": "down", "speed_mbps": None,
                  "driver": "r8127" if name == "enP7s7" else "mlx5_core", "default_route": False}
                 for name in sorted(("enP7s7", "enp1s0f0np0", "enp1s0f1np1", "enP2p1s0f0np0", "enP2p1s0f1np1"))]
        self.assertEqual(status["network"], {
            "v": 1, "observed": True, "basis": "ssh_session",
            "active": {"interface": "wlP9s9", "kind": "wifi", "operstate": "up", "speed_mbps": None},
            "underlay": None, "wired": wired, "wired_up": 0, "wired_lan_up": 0,
            "wifi": {"interface": "wlP9s9", "band": "2.4", "channel": 11, "frequency_mhz": 2462, "width_mhz": 20,
                     "tx_bitrate_mbps": 144.4, "rx_bitrate_mbps": 72.2, "signal_dbm": -52, "links": 1, "source": "iw"}})
        self.assertEqual(doctor["network"], status["network"])
        for report in (status, doctor):
            encoded = canonical(report)
            for secret in SECRETS:
                self.assertNotIn(secret.encode(), encoded, secret)
        self.assertNotIn("address", reads)
        self.assertEqual({tuple(argv) for argv, _ in self.f.calls},
                         {(IW_PATHS[0], "dev", "wlP9s9", "link"), (IW_PATHS[0], "dev", "wlP9s9", "info")})
        self.assertTrue(all(0 < timeout <= 1 for _, timeout in self.f.calls))

    def test_hostile_ssid_line_supplies_no_radio_facts(self):
        self.owner_spark()
        hostile = "freq: 5180 signal: -10 dBm rx bitrate: 9999.0 MBit/s <img src=x> channel 36 (5180 MHz), width: 160 MHz"
        self.f.outputs.update(link=IW_LINK.replace(SSID, hostile), info=IW_INFO.replace(SSID, hostile))
        view = self.f.view()
        wifi = view["wifi"]
        self.assertEqual((wifi["band"], wifi["frequency_mhz"], wifi["width_mhz"], wifi["signal_dbm"], wifi["rx_bitrate_mbps"]),
                         ("2.4", 2462, 20, -52, 72.2))
        self.assertNotIn(b"img", canonical(view))
        # With only the SSID lines left, nothing at all is claimed.
        self.f.outputs.update(link=f"Connected to {BSSID} (on wlP9s9)\n\tSSID: {hostile}\n", info=f"Interface wlP9s9\n\tssid {hostile}\n")
        self.f.calls.clear()
        self.assertEqual(self.f.view()["wifi"], {"interface": "wlP9s9", "band": None, "channel": None, "frequency_mhz": None,
                                                 "width_mhz": None, **NO_RADIO, "signal_dbm": None, "links": None, "source": None})
        self.assertEqual(self.f.calls[-1][0], NMCLI)

    def test_docker_interfaces_sorting_first_never_hide_wifi(self):
        f = self.f
        for index in range(20):
            f.iface(f"veth{index:04x}a", "virtual", operstate="up")
        for index in range(12):
            f.iface(f"br-{index:012x}", "virtual", operstate="up")
        f.iface("docker0", "virtual")
        f.iface("-wl0", "wifi", carrier=True, operstate="up")  # Never a valid name; never reaches iw argv.
        f.iface("wlP9s9", "wifi", carrier=True, operstate="up", wlan="devtype")
        f.route("wlP9s9", ANY, ANY, gateway=GATEWAY, metric=600)
        f.route("-wl0", ANY, ANY, gateway=GATEWAY, metric=1)
        f.outputs.update(link=IW_LINK, info=IW_INFO)
        self.assertGreater(sorted(os.listdir(f.sys / "class" / "net")).index("wlP9s9"), 30)
        view = f.view()
        self.assertEqual((view["basis"], view["active"]["interface"], view["active"]["kind"]), ("default_route", "wlP9s9", "wifi"))
        self.assertEqual((view["wifi"]["band"], view["wifi"]["channel"]), ("2.4", 11))
        self.assertEqual((view["wired"], view["wired_up"], view["wired_lan_up"]), ([], 0, 0))
        self.assertFalse(any("-wl0" in argv for argv, _ in f.calls))

    def test_wired_10_gigabit_never_runs_a_wifi_tool(self):
        f = self.f
        f.iface("enP7s7", "wired", carrier=True, operstate="up", speed=10000, driver="r8127", ipv4=WIRED_IP)
        f.iface("wlP9s9", "wifi", carrier=False, operstate="down")
        f.route("enP7s7", ANY, ANY, gateway=GATEWAY, metric=100)
        f.route("enP7s7", "192.168.1.0", MASK24, metric=100)
        f.session(CLIENT_IP, 51234, WIRED_IP, 22)
        view = f.view()
        self.assertEqual(view["active"], {"interface": "enP7s7", "kind": "wired", "operstate": "up", "speed_mbps": 10000})
        self.assertEqual((view["basis"], view["wifi"], view["underlay"]), ("ssh_session", None, None))
        self.assertEqual(view["wired"], [{"interface": "enP7s7", "carrier": True, "operstate": "up", "speed_mbps": 10000,
                                          "driver": "r8127", "default_route": True}])
        self.assertEqual((view["wired_up"], view["wired_lan_up"]), (1, 1))
        self.assertEqual(f.calls, [])

    def test_qsfp_carrier_without_default_route_is_not_lan(self):
        f = self.f
        f.iface("enP7s7", "wired", carrier=False, driver="r8127")
        f.iface("enp1s0f0np0", "infiniband", carrier=True, operstate="up", speed=200000, driver="mlx5_core")
        f.iface("wlP9s9", "wifi", carrier=True, operstate="up", ipv4=SPARK_IP)
        f.route("wlP9s9", ANY, ANY, gateway=GATEWAY, metric=600)
        f.route("enp1s0f0np0", "10.10.0.0", MASK24, metric=100)
        f.outputs.update(link=IW_LINK, info=IW_INFO)
        view = f.view()
        self.assertEqual(view["wired"][0], {"interface": "enp1s0f0np0", "carrier": True, "operstate": "up", "speed_mbps": 200000,
                                            "driver": "mlx5_core", "default_route": False})
        self.assertEqual(view["wired"][1]["interface"], "enP7s7")
        self.assertEqual((view["wired_up"], view["wired_lan_up"]), (1, 0))
        self.assertEqual((view["active"]["interface"], view["wifi"]["band"]), ("wlP9s9", "2.4"))

    def test_tailscale_session_reports_wifi_underlay(self):
        f = self.f
        f.iface("tailscale0", "virtual", operstate="unknown", ipv4="100.64.0.5")
        f.iface("wlP9s9", "wifi", carrier=True, operstate="up", ipv4=SPARK_IP)
        f.iface("enP7s7", "wired", carrier=False)
        f.route("wlP9s9", ANY, ANY, gateway=GATEWAY, metric=600)
        f.route("wlP9s9", "192.168.1.0", MASK24, metric=600)
        f.session("100.101.102.103", 50000, "100.64.0.5", 22)
        f.outputs.update(link=IW_LINK, info=IW_INFO)
        view = f.view()
        # Tunnel policy routing is not in the main table, so no egress claim is made.
        self.assertEqual(view["active"], {"interface": "tailscale0", "kind": "virtual", "operstate": "unknown", "speed_mbps": None})
        self.assertEqual((view["basis"], view["underlay"]), ("ssh_session", {"interface": "wlP9s9", "kind": "wifi"}))
        self.assertEqual((view["wifi"]["interface"], view["wifi"]["band"]), ("wlP9s9", "2.4"))
        self.assertNotIn(b"100.", canonical(view))

    def test_bridge_underlay_prefers_lower_member_with_carrier(self):
        f = self.f
        f.iface("br0", "virtual", operstate="up", lower=("bond0",), ipv4=WIRED_IP)
        f.iface("bond0", "virtual", operstate="up", lower=("enP7s7", "enp1s0f0np0"))
        f.iface("enP7s7", "wired", carrier=False)
        f.iface("enp1s0f0np0", "wired", carrier=True, operstate="up", speed=100000)
        f.iface("wlP9s9", "wifi", carrier=True, operstate="up")
        f.route("br0", ANY, ANY, gateway=GATEWAY, metric=100)
        f.route("wlP9s9", ANY, ANY, gateway=GATEWAY, metric=600)
        view = f.view()
        self.assertEqual((view["basis"], view["active"]["interface"]), ("default_route", "br0"))
        self.assertEqual((view["underlay"], view["wifi"]), ({"interface": "enp1s0f0np0", "kind": "wired"}, None))
        self.assertEqual(f.calls, [])

    def test_a_port_whose_bridge_bond_or_vlan_master_is_routed_is_lan(self):
        stacks = {"bridge": [("br0", ("enP7s7",))], "bond": [("bond0", ("enP7s7",))], "vlan": [("enP7s7.10", ("enP7s7",))],
                  "vlan on bond in bridge": [("bond0", ("enP7s7",)), ("bond0.10", ("bond0",)), ("br0", ("bond0.10",))]}
        for label, stack in stacks.items():
            with self.subTest(label), tempfile.TemporaryDirectory() as folder:
                f = LinkFixture(Path(folder))
                uppers = {}
                for master, members in stack:
                    for member in members:
                        uppers.setdefault(member, []).append(master)
                f.iface("enP7s7", "wired", carrier=True, operstate="up", speed=10000, driver="r8127", upper=uppers["enP7s7"])
                f.iface("enP2p1s0f0np0", "wired", carrier=True, operstate="up", speed=200000, driver="mlx5_core")
                for master, members in stack:
                    f.iface(master, "virtual", operstate="up", lower=members, upper=uppers.get(master, ()),
                            ipv4=WIRED_IP if master == stack[-1][0] else None)
                top = stack[-1][0]
                f.iface("wlP9s9", "wifi", carrier=True, operstate="up", ipv4=SPARK_IP)
                f.route(top, ANY, ANY, gateway=GATEWAY, metric=425)
                f.route(top, "192.168.1.0", MASK24, metric=425)
                f.route("wlP9s9", ANY, ANY, gateway=GATEWAY, metric=600)
                f.route("enP2p1s0f0np0", "10.10.0.0", MASK24, metric=100)  # Cluster cable: carrier, no default.
                f.session(CLIENT_IP, 51234, SPARK_IP, 22)
                f.outputs.update(link=IW_LINK, info=IW_INFO)
                view = f.view()
                self.assertEqual((view["active"]["interface"], view["active"]["egress_interface"]), ("wlP9s9", top))
                routed = {p["interface"]: p["default_route"] for p in view["wired"]}
                self.assertEqual(routed, {"enP7s7": True, "enP2p1s0f0np0": False})
                self.assertEqual((view["wired_up"], view["wired_lan_up"]), (2, 1))

    def test_stale_ssh_connection_falls_back_to_default_route(self):
        self.owner_spark()
        f = self.f
        f.iface("enP7s7-x", "wired", carrier=True, operstate="up", ipv4=WIRED_IP)
        f.files["tcp"].clear()
        f.session(CLIENT_IP, 51234, WIRED_IP, 22, live=False)
        f.socket(CLIENT_IP, 51234, WIRED_IP, 22, state="06")  # TIME_WAIT
        f.socket(CLIENT_IP, 51235, WIRED_IP, 22)  # A different client port
        f.socket("192.168.1.51", 51234, WIRED_IP, 22)  # A different client
        view = f.view()
        self.assertEqual((view["basis"], view["active"]["interface"]), ("default_route", "wlP9s9"))
        f.environ.clear()
        self.assertEqual(f.view()["basis"], "default_route")
        f.files["route"].clear()
        empty = f.view()
        self.assertEqual((empty["basis"], empty["active"], empty["underlay"], empty["wifi"]), ("unknown", None, None, None))

    def test_ipv6_zone_mapped_and_global_addresses(self):
        self.owner_spark()
        f = self.f
        f.iface("enP7s7-x", "wired", carrier=True, operstate="up", speed=10000, ipv4=WIRED_IP)
        f.files["tcp"].clear()
        f.session("fe80::1234%enP7s7-x", 50000, "fe80::abcd%enP7s7-x", 22)
        self.assertEqual((f.view()["basis"], f.view()["active"]["interface"]), ("ssh_session", "enP7s7-x"))
        for bad in ("fe80::1234%-x 50000 fe80::abcd%-x 22", "fe80::1234%enP7s7-x 50000 fe80::abcd%enP7s7-x/1 22",
                    "192.168.1.50%enP7s7-x 50000 192.168.1.21%enP7s7-x 22", "fe80::1234 99999 fe80::abcd 22", "a b c"):
            f.environ["SSH_CONNECTION"] = bad
            self.assertEqual(f.view()["basis"], "default_route", bad)
        f.files["tcp6"].clear()
        f.session("::ffff:" + CLIENT_IP, 51234, "::ffff:" + WIRED_IP, 22)
        self.assertEqual(f.view()["active"]["interface"], "enP7s7-x")
        f.environ["SSH_CONNECTION"] = f"{CLIENT_IP} 51234 {WIRED_IP} 22"  # sshd unwraps; the socket stays mapped.
        self.assertEqual((f.view()["basis"], f.view()["active"]["interface"]), ("ssh_session", "enP7s7-x"))
        f.session("2001:db8::50", 40000, "2001:db8::21", 22)
        f.inet6("enP7s7-x", "2001:db8::21")
        f.route6("enP7s7-x", "2001:db8::", 64, metric=256)
        view = f.view()
        self.assertEqual((view["basis"], view["active"]["interface"]), ("ssh_session", "enP7s7-x"))
        self.assertNotIn("egress_interface", view["active"])
        self.assertNotIn(b"2001:db8", canonical(view))

    def test_egress_reported_only_when_it_differs_from_owner(self):
        f = self.f
        f.iface("enP7s7", "wired", carrier=True, operstate="up", speed=10000, ipv4=WIRED_IP)
        f.iface("wlP9s9", "wifi", carrier=True, operstate="up", ipv4=SPARK_IP)
        f.route("wlP9s9", ANY, ANY, gateway=GATEWAY, metric=600)
        f.route("wlP9s9", "192.168.1.0", MASK24, metric=600)
        f.route("enP7s7", "192.168.1.0", MASK24, metric=700)
        f.session(CLIENT_IP, 51234, WIRED_IP, 22)
        view = f.view()
        self.assertEqual(view["active"], {"interface": "enP7s7", "kind": "wired", "operstate": "up", "speed_mbps": 10000,
                                          "egress_interface": "wlP9s9"})
        self.assertEqual((view["wifi"], f.calls), (None, []))
        # A secondary address is missed by SIOCGIFADDR; its connected route still names the owner.
        f.addresses.clear()
        f.route("enP7s7", "10.0.0.0", MASK24, metric=100)
        f.files["tcp"].clear()
        f.session("10.0.0.50", 40000, "10.0.0.21", 22)
        view = f.view()
        self.assertEqual((view["basis"], view["active"]["interface"]), ("ssh_session", "enP7s7"))
        self.assertNotIn("egress_interface", view["active"])

    def test_nmcli_when_iw_is_absent_and_proc_when_both_are(self):
        self.owner_spark()
        f = self.f
        f.absent.update(IW_PATHS)
        f.wireless("wlP9s9", -61)
        expected = {"interface": "wlP9s9", "band": "5", "channel": 36, "frequency_mhz": 5180, "width_mhz": 80,
                    **NO_RADIO, "signal_dbm": -61, "links": 1, "source": "nmcli"}
        for listing in ("no:2412 MHz:1:20 MHz\nyes:5180 MHz:36:80 MHz\nno:5955 MHz:1:160 MHz\n", "yes:36:5180 MHz:80 MHz\n"):
            f.outputs["nmcli"] = listing
            f.calls.clear()
            self.assertEqual(f.view()["wifi"], expected)
            self.assertEqual([argv[0] for argv, _ in f.calls], [*IW_PATHS, *IW_PATHS, NMCLI_PATHS[0]])
            self.assertEqual(f.calls[-1][0], NMCLI)
        self.assertFalse(any("SSID" in arg or "BSSID" in arg for argv, _ in f.calls for arg in argv))
        f.absent.update(NMCLI_PATHS)
        f.files["wireless"] = []
        f.wireless("wlP9s9", -71)
        self.assertEqual(f.view()["wifi"], {"interface": "wlP9s9", "band": None, "channel": None, "frequency_mhz": None,
                                            "width_mhz": None, **NO_RADIO, "signal_dbm": -71, "links": None, "source": "proc"})

    def test_tool_timeout_falls_through_without_trying_another_path(self):
        self.owner_spark()
        f = self.f
        f.outputs.update(link=subprocess.TimeoutExpired("iw", 1), info=subprocess.TimeoutExpired("iw", 1), nmcli="yes:2437 MHz:6:40 MHz\n")
        wifi = f.view()["wifi"]
        self.assertEqual((wifi["band"], wifi["channel"], wifi["width_mhz"], wifi["source"]), ("2.4", 6, 40, "nmcli"))
        self.assertEqual([argv[0] for argv, _ in f.calls], [IW_PATHS[0], IW_PATHS[0], NMCLI_PATHS[0]])

    def test_tool_time_is_bounded_by_the_probe_deadline(self):
        self.owner_spark()
        f = self.f
        f.outputs.clear()
        clock = [100.0]
        def slow(argv, timeout):
            f.calls.append((list(argv), timeout))
            clock[0] += 1.0
            return None
        f.write()
        with mock.patch.object(controller.time, "monotonic", lambda: clock[0]):
            wifi = network_view(network_link(f.sys, f.proc, f.environ, slow, f.ipv4, os_name="posix"))["wifi"]
        self.assertEqual([timeout for _, timeout in f.calls], [1.0, 1.0, 0.5])
        self.assertEqual(wifi["source"], None)

    def test_multi_link_operation_never_claims_one_band(self):
        self.owner_spark()
        mld = (f"Interface wlP9s9\n\taddr {MAC}\n\tssid {SSID}\n\ttype managed\n\tMLD with links:\n"
               f"\t - link ID  0 link addr {MAC}\n\t   channel 1 (2412 MHz), width: 20 MHz, center1: 2412 MHz\n"
               f"\t - link ID  1 link addr {MAC}\n\t   channel 36 (5180 MHz), width: 160 MHz, center1: 5250 MHz\n")
        self.f.outputs["info"] = mld
        wifi = self.f.view()["wifi"]
        self.assertEqual(wifi, {"interface": "wlP9s9", "band": "multi", "channel": None, "frequency_mhz": None, "width_mhz": None,
                                "tx_bitrate_mbps": 144.4, "rx_bitrate_mbps": 72.2, "signal_dbm": -52, "links": 2, "source": "iw"})
        self.f.outputs["info"] = mld + "\t - link ID  2 link addr x\n\t   channel 37 (6135 MHz), width: 320 MHz, center1: 6105 MHz\n"
        self.assertEqual((self.f.view()["wifi"]["band"], self.f.view()["wifi"]["links"]), ("multi", 3))
        self.f.outputs.update(info="", link=IW_LINK.replace("\tfreq: 2462\n", "\tfreq: 2412\n\tfreq: 5180\n"))
        self.assertEqual((self.f.view()["wifi"]["band"], self.f.view()["wifi"]["links"]), ("multi", 2))

    def test_frequency_table(self):
        for frequency, expected in ((2412, ("2.4", 1)), (2472, ("2.4", 13)), (2484, ("2.4", 14)), (5180, ("5", 36)),
                                    (5895, ("5", 179)), (5935, ("6", 2)), (5955, ("6", 1)), (7115, ("6", 233)),
                                    (2400, (None, None)), (2413, ("2.4", None)), (5000, (None, None)), (7200, (None, None))):
            self.assertEqual(frequency_band(frequency), expected, frequency)
        self.owner_spark()
        self.f.outputs.update(link=IW_LINK.replace("freq: 2462", "freq: 5955.0"), info="")
        wifi = self.f.view()["wifi"]
        self.assertEqual((wifi["band"], wifi["channel"], wifi["frequency_mhz"], wifi["width_mhz"]), ("6", 1, 5955, None))

    def test_sanitizer_rebuilds_only_whitelisted_bounded_fields(self):
        raw = {"observed": True, "basis": "ssh_session", "ssid": SSID, "client_ip": CLIENT_IP,
               "active": {"interface": "wlP9s9", "kind": "wifi", "operstate": "up", "speed_mbps": float("nan"),
                          "address": MAC, "egress_interface": "x" * 16},
               "underlay": {"interface": "-bad", "kind": "wired"},
               "wired": [{"interface": "a" * 16, "carrier": True},
                         {"interface": "enP7s7", "carrier": 1, "operstate": "sideways", "speed_mbps": True,
                          "driver": "Bad Driver", "default_route": "yes", "mac": MAC}],
               "wired_up": -1, "wired_lan_up": 10 ** 400,
               "wifi": {"interface": "wlP9s9", "band": "2.4GHz", "channel": 0, "frequency_mhz": 2462.0, "width_mhz": 30,
                        "tx_bitrate_mbps": float("inf"), "rx_bitrate_mbps": float("nan"), "signal_dbm": 0, "links": 4,
                        "source": "scan", "ssid": SSID, "bssid": BSSID}}
        view = network_view(raw)
        self.assertEqual(view, {"v": 1, "observed": True, "basis": "ssh_session",
                                "active": {"interface": "wlP9s9", "kind": "wifi", "operstate": "up", "speed_mbps": None},
                                "underlay": None,
                                "wired": [{"interface": "enP7s7", "carrier": None, "operstate": None, "speed_mbps": None,
                                           "driver": None, "default_route": False}],
                                "wired_up": 0, "wired_lan_up": 0,
                                "wifi": {"interface": "wlP9s9", "band": None, "channel": None, "frequency_mhz": None,
                                         "width_mhz": None, **NO_RADIO, "signal_dbm": None, "links": None, "source": None}})
        encoded = json.dumps(view, allow_nan=False)
        for secret in SECRETS:
            self.assertNotIn(secret, encoded)
        self.assertEqual(len(network_view({"observed": True, "wired": [{"interface": f"eth{i}"} for i in range(40)]})["wired"]), 16)
        self.assertEqual(network_view({"observed": True, "basis": "ssh_session", "active": {"interface": "<img>"}})["basis"], "unknown")
        for invalid in (None, [], "observed", {"observed": "true"}, {"observed": 1}, {}):
            self.assertEqual(network_view(invalid), {"v": 1, "observed": False, "basis": "unknown"})

    def test_mac_derived_names_are_replaced_by_stable_aliases(self):
        f = self.f
        macs = ("00e04c680123", "00e04c680000", "001122334455")
        f.iface("enx00e04c680123", "wired", carrier=True, operstate="up", speed=2500, driver="r8152", ipv4=WIRED_IP)
        f.iface("enx00e04c680000", "wired", carrier=False, driver="r8152")
        f.iface("enP7s7", "wired", carrier=False, driver="r8127")
        f.iface("wlx001122334455", "wifi", carrier=True, operstate="up", ipv4=SPARK_IP)
        f.route("enx00e04c680123", ANY, ANY, gateway=GATEWAY, metric=100)
        f.route("enx00e04c680123", "192.168.1.0", MASK24, metric=100)
        f.route("wlx001122334455", ANY, ANY, gateway=GATEWAY, metric=600)
        f.session(CLIENT_IP, 51234, WIRED_IP, 22)
        status, doctor = self.reports(f.probe)
        view = status["network"]
        self.assertEqual(view["active"], {"interface": "usb-ethernet-2", "kind": "wired", "operstate": "up", "speed_mbps": 2500})
        self.assertEqual([p["interface"] for p in view["wired"]], ["usb-ethernet-2", "enP7s7", "usb-ethernet-1"])
        # The same Spark seen over its USB Wi-Fi: one alias in every field.
        f.files["tcp"].clear()
        f.session(CLIENT_IP, 51235, SPARK_IP, 22)
        f.outputs.update(link=IW_LINK, info=IW_INFO)
        wifi_view = f.view()
        self.assertEqual(wifi_view["active"], {"interface": "usb-wifi-1", "kind": "wifi", "operstate": "up", "speed_mbps": None,
                                               "egress_interface": "usb-ethernet-2"})
        self.assertEqual((wifi_view["wifi"]["interface"], wifi_view["wifi"]["band"]), ("usb-wifi-1", "2.4"))
        self.assertIn((IW_PATHS[0], "dev", "wlx001122334455", "link"), {tuple(argv) for argv, _ in f.calls})  # Real name in argv only.
        for report in (status, doctor, wifi_view):
            encoded = canonical(report)
            for mac in macs:
                self.assertNotIn(mac.encode(), encoded)
        for report in (view, wifi_view):
            self.assertIsNone(re.search(rb"[0-9A-Fa-f]{12}", canonical(report)))
        # The sanitizer aliases whatever raw object it is given.
        self.assertEqual(network_view({"observed": True, "active": {"interface": "ENX00E04C680123", "kind": "wired"}})["active"]["interface"],
                         "usb-ethernet-1")
        self.assertEqual(network_view({"observed": True, "underlay": {"interface": "wwx00e04c680123", "kind": "wired"}})["underlay"],
                         {"interface": "usb-wwan-1", "kind": "wired"})

    def test_failing_or_unsupported_probe_still_reports_status(self):
        def broken():
            raise RuntimeError("probe exploded")
        for probe in (broken, lambda: ["not", "a", "dict"], lambda: network_link(os_name="nt")):
            status, doctor = self.reports(probe)
            self.assertTrue(status["ok"] and doctor["ok"])
            self.assertEqual(status["network"], {"v": 1, "observed": False, "basis": "unknown"})
            self.assertEqual(doctor["network"], status["network"])
        self.assertEqual(network_link(os_name="nt"), {"observed": False})
        if os.name != "posix":
            self.assertEqual(network_link(), {"observed": False})
        self.assertEqual(network_link(Path(self.tmp.name) / "missing", os_name="posix"), {"observed": False})

    def test_four_positional_arguments_still_construct(self):
        c = Controller(Path(self.tmp.name) / "positional", FakeDocker(), lambda: 120, lambda: {"cpu_count": 20, "memory_gb": 120})
        try:
            self.assertIs(c.link_probe, network_link)
            c.link_probe = NO_LINK  # Never probe the test host's own network.
            self.assertTrue(c.rpc({"v": 1, "op": "status"})["ok"])
        finally:
            c.db.close()

    def test_default_tool_runner_is_argv_only_with_fixed_environment(self):
        argv = [IW_PATHS[0], "dev", "wlP9s9", "link"]
        with mock.patch.object(controller.subprocess, "run", return_value=subprocess.CompletedProcess(argv, 0, b"x" * (TOOL_READ + 9))) as run:
            self.assertEqual(len(run_tool(argv, 0.75)), TOOL_READ)
        args, kwargs = run.call_args
        self.assertEqual(args, (argv,))
        self.assertEqual({k: kwargs[k] for k in ("stdin", "stdout", "stderr", "env", "timeout", "check")},
                         {"stdin": subprocess.DEVNULL, "stdout": subprocess.PIPE, "stderr": subprocess.DEVNULL,
                          "env": TOOL_ENV, "timeout": 0.75, "check": False})
        self.assertNotIn("shell", kwargs)
        with mock.patch.object(controller.subprocess, "run", return_value=subprocess.CompletedProcess(argv, 1, b"partial")):
            self.assertIsNone(run_tool(argv, 0.75))
        self.assertIsNone(controller.IFNAME.fullmatch("-wl0"))
        self.assertIsNone(controller.IFNAME.fullmatch("w" * 16))
        self.assertTrue(controller.IFNAME.fullmatch("enP2p1s0f1np1"))


if __name__ == "__main__":
    unittest.main()
