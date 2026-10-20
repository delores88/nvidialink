import base64
import json
import os
from pathlib import Path
import tempfile
import time
import unittest

from controller import Controller, Rejected, canonical, sha, CHUNK, SECRET

IMAGE = "sha256:" + "a" * 64
CUDA_IMAGE = "sha256:" + "c" * 64
PROJECT = "b" * 64


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
        self.c = Controller(self.root, self.docker, lambda: 120, lambda: {"cpu_count": 20, "memory_gb": 120})
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
        self.c = Controller(self.root, self.docker, lambda: 120, lambda: {"cpu_count": 20, "memory_gb": 120})
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
        self.c = Controller(self.root, self.docker, lambda: 120, lambda: {"cpu_count": 20, "memory_gb": 120})
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


if __name__ == "__main__":
    unittest.main()
