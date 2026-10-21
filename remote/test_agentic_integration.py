"""Integrated scheduler contracts; fake Docker, never live Spark evidence."""
import json
import os
from pathlib import Path
import unittest
from unittest import mock

import controller
from controller import Rejected
import test_controller as fixtures
from test_controller import IMAGE, CUDA_IMAGE, PROJECT


class AgenticIntegrationTests(unittest.TestCase):
    setUp = fixtures.ControllerTests.setUp
    tearDown = fixtures.ControllerTests.tearDown
    rpc = fixtures.ControllerTests.rpc
    snapshot = fixtures.ControllerTests.snapshot
    run_job = fixtures.ControllerTests.run_job

    def server(self, name="model", request_id="service-1", **kwargs):
        mid = self.snapshot()
        return self.rpc("serve", project=PROJECT, manifest_id=mid, argv=["python3", "main.py"],
                        client="codex", request_id=request_id, name=name, **kwargs)

    def test_service_skips_project_claim_per_job_and_client_caps(self):
        self.c.resources({"v":1,"op":"resources","limits":{
            "cpu_per_job":4,"memory_gb_per_job":8,"cpu_total":16,"memory_gb_total":64,
            "client_jobs":1,"client_cpu":4,"headroom_gb":32}})
        server = self.server(memory_gb=32)
        job, _ = self.run_job(request_id="batch")
        self.assertNotIn("admission_blocked", next(r for r in self.rpc("status")["jobs"] if r["job"] == server["job"]))
        self.c.tick()
        self.assertEqual(self.c.get_job(server["job"])["state"], "running")
        self.assertEqual(self.c.get_job(job["job"])["state"], "queued")
        self.c.tick()
        self.assertEqual(self.c.get_job(job["job"])["state"], "running")
        args = self.c.create_args(self.c.get_job(server["job"]))
        for dest in ("/target", "/cargo-home", "/npm-cache", "/pip-cache"):
            self.assertTrue(next(v for v in args if f",dst={dest}" in v).endswith(",readonly"))
        self.assertIn(f"type=bind,src={self.root / 'shared' / 'cache'},dst=/cache", args)
        self.assertNotIn("-p", args)

    def test_ready_server_not_double_reserved_against_live_memory(self):
        server = self.server(memory_gb=32)
        self.c.tick()
        self.c.db.execute("UPDATE jobs SET ready_at=1,last_activity=? WHERE id=?", (controller.time.time(),server["job"]))
        job, _ = self.run_job(project="c"*64, request_id="batch")
        self.c.memory_probe = lambda: 50
        self.c.tick()
        self.assertEqual(self.c.get_job(job["job"])["state"],"running")

    def test_alias_request_id_is_reserved_against_batch(self):
        first = self.server()
        again = self.server(request_id="alias")
        self.assertEqual(first["job"],again["job"])
        mid = self.snapshot()
        result = self.c.rpc({"v":1,"op":"run","project":PROJECT,"manifest_id":mid,
            "argv":["python3","main.py"],"runtime":"python","client":"codex","request_id":"alias"})
        self.assertEqual(result["error"]["code"],"idempotency_conflict")

    def test_three_definite_failed_creates_do_not_stall_another_job(self):
        bad, _ = self.run_job()
        good, _ = self.run_job(project="c"*64,client="human",request_id="good")
        original = self.docker.call
        def call(args, **kwargs):
            if args[0] == "create" and self.c.container_name(bad["job"]) in args:
                raise Rejected("docker_failed","fixture")
            return original(args, **kwargs)
        self.docker.call = call
        for _ in range(4):
            self.c.tick()
        row = self.c.get_job(bad["job"])
        self.assertEqual((row["state"],row["error"],row["launch_failures"]),("failed","launch_failed",3))
        self.assertEqual(self.c.get_job(good["job"])["state"],"running")

    def test_unknown_docker_state_stops_admission_without_duplicate_launch(self):
        old, _ = self.run_job()
        self.c.tick()
        new, _ = self.run_job(project="c"*64,request_id="new")
        self.docker.calls.clear()
        with mock.patch.object(self.docker,"inspect",side_effect=Rejected("docker_unavailable","fixture")):
            self.c.tick()
        self.assertTrue(self.c._docker_unavailable)
        self.assertEqual(self.c.get_job(new["job"])["state"],"queued")
        self.assertEqual(self.c.get_job(old["job"])["state"],"running")
        self.assertFalse(self.docker.calls)

    def test_old_active_jobs_are_never_hidden_by_finished_history(self):
        job,_ = self.run_job()
        columns = [r[1] for r in self.c.db.execute("PRAGMA table_info(jobs)")]
        base = dict(self.c.get_job(job["job"]))
        for i in range(205):
            row = {**base,"id":f"done-{i}","request_id":f"done-{i}","state":"succeeded","created":base["created"]+i+1}
            self.c.db.execute(f"INSERT INTO jobs({','.join(columns)}) VALUES({','.join('?' for _ in columns)})", [row[k] for k in columns])
        jobs = self.rpc("status")["jobs"]
        self.assertEqual(len(jobs),201)
        self.assertIn(job["job"], [r["job"] for r in jobs])

    def test_cuda_watchdog_checks_newest_first(self):
        (self.root.parent / "runtimes.json").write_text(json.dumps({"v":1,"runtimes":{
            "cuda":{"image":CUDA_IMAGE,"gpu":True,"provenance":"fixture"},"python":{"image":IMAGE,"provenance":"fixture"}}}))
        older,_ = self.run_job(runtime="cuda")
        newer,_ = self.run_job(project="c"*64,client="human",runtime="cuda",request_id="newer")
        self.c.tick(); self.c.tick()
        self.c.db.execute("UPDATE jobs SET started=1 WHERE id=?",(older["job"],))
        self.c.db.execute("UPDATE jobs SET started=2 WHERE id=?",(newer["job"],))
        self.docker.calls.clear()
        self.c.memory_probe=lambda: 30
        self.c.tick()
        stopped=[v[-1] for v in self.docker.calls if v[0]=="stop"]
        self.assertEqual(stopped[0],self.c.container_name(newer["job"]))

    def test_route_addresses_only_in_explicit_authenticated_rpc(self):
        self.c.lan_probe=lambda: [{"kind":"wired","ipv4":"192.168.77.2","prefix":24,"speed_mbps":1000}]
        routes=self.rpc("lan_routes")
        status=self.rpc("status")
        self.assertEqual(routes["routes"][0]["ipv4"],"192.168.77.2")
        self.assertEqual(routes["epoch"],status["network"]["lan_epoch"])
        self.assertNotIn("192.168.77.2",json.dumps(status))
        self.assertNotIn("192.168.77.2",str(self.c.db.execute("SELECT data FROM audit").fetchall()))

    @unittest.skipUnless(os.name=="posix","POSIX filesystem safety")
    def test_fifo_logs_refused_without_blocking(self):
        job,_=self.run_job()
        output=self.root / "jobs" / job["job"] / "output"
        output.mkdir(exist_ok=True)
        os.mkfifo(output / "stdout.log")
        with self.assertRaises(Rejected):
            controller.safe_open(output / "stdout.log")

    @unittest.skipUnless(os.name=="posix","POSIX runtime refresh gate")
    def test_runtime_refresh_gate_blocks_new_submission_and_admission(self):
        import fcntl
        mid=self.snapshot()
        with (self.root.parent / "runtime-refresh.lock").open("a") as lock:
            fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
            self.c.tick()
            result=self.c.rpc({"v":1,"op":"serve","project":PROJECT,"manifest_id":mid,
                "argv":["python3","main.py"],"client":"codex","request_id":"locked","name":"test"})
            self.assertEqual(result["error"]["code"],"runtime_refresh_in_progress")
            self.assertFalse(self.docker.calls)


if __name__=="__main__":
    unittest.main()
