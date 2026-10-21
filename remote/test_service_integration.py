"""Focused review regressions: fake Docker only, no physical Spark claims."""
import unittest
from unittest import mock

import controller
import test_controller as fixtures


class ServiceSafetyIntegrationTests(unittest.TestCase):
    setUp = fixtures.ControllerTests.setUp
    tearDown = fixtures.ControllerTests.tearDown
    rpc = fixtures.ControllerTests.rpc
    snapshot = fixtures.ControllerTests.snapshot
    run_job = fixtures.ControllerTests.run_job

    def running_service(self):
        manifest = self.snapshot()
        job = self.rpc("serve", project=fixtures.PROJECT, manifest_id=manifest,
                       argv=["python3", "main.py"], name="model", client="codex", request_id="service-1")
        self.c.db.execute("UPDATE jobs SET state='running',started=?,egress='internet' WHERE id=?", (controller.time.time(), job["job"]))
        self.docker.containers[self.c.container_name(job["job"])] = {
            "Image": fixtures.IMAGE,
            "Config": {"Labels": {"com.delores.sparkdev.job": job["job"]}},
            "State": {"Status": "running", "ExitCode": 0},
            "NetworkSettings": {"Networks": {}},
        }
        return job

    def test_failed_internet_cut_stops_verified_service(self):
        job = self.running_service()
        self.c.resources({"v": 1, "op": "resources", "internet": False})
        with mock.patch.object(self.c, "remove_proxy", side_effect=controller.Rejected("docker_failed", "fixture")):
            self.c.tick()
        row = self.c.get_job(job["job"])
        self.assertEqual((row["state"], row["stop_reason"], row["exit_code"]), ("cancelled", "internet_off", 130))
        self.assertTrue(any(args[:3] == ["stop", "--time", "10"] and args[-1] == self.c.container_name(job["job"]) for args in self.docker.calls))

    def test_failed_internet_cut_never_stops_mismatched_container(self):
        job = self.running_service()
        self.docker.containers[self.c.container_name(job["job"])]["Image"] = "sha256:" + "e" * 64
        self.c.resources({"v": 1, "op": "resources", "internet": False})
        with mock.patch.object(self.c, "remove_proxy", side_effect=controller.Rejected("docker_failed", "fixture")):
            self.c.tick()
        self.assertFalse(any(args[0] == "stop" for args in self.docker.calls))
        self.assertEqual(self.c.get_job(job["job"])["state"], "running")
        self.assertTrue(self.c._docker_unavailable)

    def test_cleanup_daemon_outage_blocks_new_admission(self):
        old, _ = self.run_job(request_id="old")
        self.c.db.execute("UPDATE jobs SET state='succeeded',finished=1,exit_code=0,proxy_image=?,proxy_cleaned=0 WHERE id=?", (fixtures.IMAGE, old["job"]))
        new, _ = self.run_job(project="c" * 64, request_id="new")
        with mock.patch.object(self.c, "cleanup_proxy", side_effect=controller.Rejected("docker_unavailable", "fixture")):
            self.c.tick()
        self.assertTrue(self.c._docker_unavailable)
        self.assertEqual(self.c.get_job(new["job"])["state"], "queued")

    def test_missing_preparation_helper_image_cannot_hold_quota_forever(self):
        manifest = self.snapshot()
        job = self.rpc("prepare", project=fixtures.PROJECT, manifest_id=manifest,
                       runtime="python", client="codex", request_id="prepare-1")
        original = self.docker.call
        def call(args, **kwargs):
            if args[0] == "create" and "sparkdev-proxy-" + job["job"] in args:
                raise controller.Rejected("docker_failed", "fixture missing image")
            return original(args, **kwargs)
        with mock.patch.object(self.docker, "call", side_effect=call):
            for _ in range(4):
                self.c.tick()
        row = self.c.get_job(job["job"])
        self.assertEqual((row["state"], row["error"], row["launch_failures"]), ("failed", "launch_failed", 3))

    def test_emergency_disk_floor_preserves_server_stop_contract(self):
        job = self.running_service()
        self.c.disk_probe = lambda: (15 * controller.GIB, 120 * controller.GIB)
        self.c.tick()
        row = self.c.get_job(job["job"])
        self.assertEqual((row["state"], row["stop_reason"], row["exit_code"]), ("cancelled", "disk_full", 130))


if __name__ == "__main__":
    unittest.main()
