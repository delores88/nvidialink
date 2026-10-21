"""Internet policy, container boundaries and failure isolation using FakeDocker."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import controller
from controller import Rejected
import test_controller as fixtures
from test_controller import IMAGE, PROJECT


def env_args(args):
    values = [value for index, value in enumerate(args) if index and args[index - 1] == "--env"]
    return dict(value.split("=", 1) for value in values)


class InternetControllerTests(unittest.TestCase):
    rpc = fixtures.ControllerTests.rpc
    snapshot = fixtures.ControllerTests.snapshot
    run_job = fixtures.ControllerTests.run_job
    tearDown = fixtures.ControllerTests.tearDown

    def setUp(self):
        fixtures.ControllerTests.setUp(self)
        self.c.db.execute("DELETE FROM settings WHERE key='internet_policy'")
        self.deny = mock.patch.object(controller, "internet_deny", return_value=["192.168.0.0/24", "8.8.8.9/32"])
        self.guard = mock.patch.object(controller, "guard_bridge_ipv6")
        self.deny.start(); self.guard.start()
        self.addCleanup(self.deny.stop); self.addCleanup(self.guard.stop)

    def test_default_on_old_run_shape_and_status(self):
        policy = self.rpc("resources")["internet"]
        self.assertEqual((policy["enabled"], policy["saved"], policy["available"]), (True, False, True))
        job, _ = self.run_job()
        self.assertEqual(job["internet"], "on")
        self.assertEqual(self.c.get_job(job["job"])["proxy_image"], IMAGE)
        self.assertTrue((self.root / "jobs" / job["job"] / "proxy.py").is_file())
        self.assertEqual(self.rpc("status")["internet"], policy)

    def test_atomic_policy_limits_and_strict_bool(self):
        original = self.rpc("resources")["limits"]
        limits = {**original, "cpu_per_job": 8, "client_cpu": 8}
        result = self.rpc("resources", limits=limits, internet=False)
        self.assertEqual(result["limits"], limits)
        self.assertFalse(result["internet"]["enabled"])
        for value in (0, 1, "on", None, {}):
            response = self.c.rpc({"v": 1, "op": "resources", "limits": original, "internet": value})
            self.assertEqual(response["error"]["code"], "invalid_request")
            self.assertEqual(self.rpc("resources")["limits"], limits)
        response = self.c.rpc({"v": 1, "op": "resources", "limits": {**limits, "cpu_total": 1}, "internet": True})
        self.assertFalse(response["ok"])
        self.assertFalse(self.rpc("resources")["internet"]["enabled"])

    def test_cut_or_unavailable_internet_success_never_satisfies_online_result_cache(self):
        for index, egress in enumerate(("cut", "setup_failed")):
            project = ("c" if index else "b") * 64
            first, _ = self.run_job(project=project, request_id=f"first-{index}")
            self.c.db.execute("UPDATE jobs SET state='succeeded',exit_code=0,finished=1,egress=? WHERE id=?", (egress,first["job"]))
            again, _ = self.run_job(project=project, request_id=f"again-{index}")
            self.assertFalse(again["cache_hit"])
            self.assertEqual(again["state"],"queued")

    def test_no_python_is_visible_and_runs_offline(self):
        (self.root.parent / "runtimes.json").write_text(json.dumps({"v": 1, "runtimes": {"rust": {"image": IMAGE, "provenance": "fixture"}}}))
        job, _ = self.run_job(runtime="rust")
        self.assertEqual(job["internet"], "unavailable")
        self.assertEqual(self.c.get_job(job["job"])["egress"], "no_helper")
        self.c.tick()
        args = next(call for call in self.docker.calls if call[0] == "create")
        self.assertEqual(args[args.index("--network") + 1], "none")

    def test_online_args_no_offline_env_or_extra_quota(self):
        limits = self.rpc("resources")["limits"]
        self.rpc("resources", limits={**limits, "cpu_per_job": 8, "client_cpu": 8})
        job, _ = self.run_job()
        self.c.tick()
        row = self.c.get_job(job["job"])
        self.assertEqual(row["state"], "running")
        self.assertEqual(controller.reserved_cpu(row), 8)
        self.assertEqual(controller.reserved_memory(row), row["memory"])
        args = next(call for call in self.docker.calls if call[0] == "create" and call[-1] == "/sparkdev-worker.py")
        env = env_args(args)
        self.assertEqual(env["CARGO_NET_OFFLINE"], "false")
        self.assertEqual(env["npm_config_offline"], "false")
        self.assertNotIn("PIP_NO_INDEX", env)
        self.assertEqual(env["HTTP_PROXY"], "http://sparkdev-proxy:3128")
        self.assertEqual(env["NO_PROXY"], "localhost,127.0.0.1,::1,127.0.0.0/8")
        self.assertEqual(env["PYTHONUSERBASE"], "/sparkdev-scratch/python")
        self.assertEqual(env["GIT_CONFIG_VALUE_1"], "ssh://git@github.com/")
        self.assertIn(f"fsize={64 * controller.GIB}:{64 * controller.GIB}", args)
        self.assertEqual(args[args.index("--dns") + 1], "127.0.0.1")
        self.assertFalse(any("CERT" in key or key == "REQUESTS_CA_BUNDLE" for key in env))
        helper = next(call for call in self.docker.calls if call[0] == "create" and call[-1] == "/sparkdev-proxy.py")
        self.assertEqual(helper[-2], IMAGE)
        self.assertEqual(helper[helper.index("--cpus") + 1], "1")
        self.assertEqual(helper[helper.index("--memory") + 1], "256m")
        self.assertEqual(env_args(helper)["SPARKDEV_PROXY_MODE"], "public")

    def test_off_args_and_preparation_caps_remain_offline_and_allowlisted(self):
        self.rpc("resources", internet=False)
        job, _ = self.run_job()
        args = self.c.create_args(self.c.get_job(job["job"]))
        self.assertEqual(args[args.index("--network") + 1], "none")
        env = env_args(args)
        self.assertEqual(env["CARGO_NET_OFFLINE"], "true")
        self.assertEqual(env["PIP_NO_INDEX"], "1")
        self.assertNotIn("HTTP_PROXY", env)
        self.assertNotIn("PYTHONUSERBASE", env)
        self.rpc("cancel", job=job["job"])
        preparation = self.rpc("prepare", project=PROJECT, manifest_id=job["manifest_id"], runtime="python", client="codex", request_id="prep")
        row = self.c.get_job(preparation["job"])
        self.assertEqual(preparation["internet"], "package_registries_only")
        self.assertEqual(controller.reserved_cpu(row), self.rpc("resources")["limits"]["cpu_per_job"])
        args = self.c.create_args(row)
        self.assertIn(f"fsize={4 * controller.GIB}:{4 * controller.GIB}", args)
        self.c.ensure_proxy(row)
        helper = next(call for call in self.docker.calls if call[0] == "create")
        self.assertNotIn("SPARKDEV_PROXY_MODE=public", helper)

    def test_cache_does_not_cross_network_setting(self):
        first, request = self.run_job()
        row = self.c.get_job(first["job"])
        self.c.finish(row, "succeeded", 0)
        cached = self.rpc("run", **{**request, "request_id": "repeat-on"})
        self.assertTrue(cached["cache_hit"])
        self.rpc("resources", internet=False)
        second = self.rpc("run", **{**request, "request_id": "repeat-off"})
        self.assertFalse(second["cache_hit"])
        self.assertNotEqual(row["cache_key"], self.c.get_job(second["job"])["cache_key"])

    def test_deny_snapshot_written_once_and_never_replaced(self):
        job, _ = self.run_job()
        row = self.c.get_job(job["job"])
        self.c.ensure_proxy(row)
        path = self.root / "jobs" / job["job"] / "egress-deny.json"
        before = (path.stat().st_ino, path.stat().st_mtime_ns, path.read_bytes())
        with mock.patch.object(controller, "internet_deny", side_effect=AssertionError("must not refresh")):
            self.c.ensure_proxy(row)
        self.assertEqual(before, (path.stat().st_ino, path.stat().st_mtime_ns, path.read_bytes()))

    def test_helper_setup_failure_runs_offline_but_daemon_failure_does_not(self):
        first, _ = self.run_job()
        with mock.patch.object(controller, "guard_bridge_ipv6", side_effect=Rejected("ipv6_link", "fixture")):
            self.c.tick()
        row = self.c.get_job(first["job"])
        self.assertEqual((row["state"], row["egress"]), ("running", "setup_failed"))
        args = next(call for call in self.docker.calls if call[0] == "create")
        self.assertEqual(args[args.index("--network") + 1], "none")
        second, _ = self.run_job(project="c" * 64, request_id="second")
        with mock.patch.object(self.c, "ensure_proxy", side_effect=Rejected("docker_unavailable", "fixture")):
            self.c.tick()
        self.assertEqual(self.c.get_job(second["job"])["egress"], "internet")
        self.assertNotIn(self.c.container_name(second["job"]), self.docker.containers)

    def test_owner_switch_cuts_running_and_queued_helpers_not_workloads(self):
        first, _ = self.run_job()
        self.c.tick()
        second, _ = self.run_job(project="c" * 64, request_id="second")
        self.rpc("resources", internet=False)
        self.c.tick()
        for job in (first, second):
            row = self.c.get_job(job["job"])
            self.assertEqual(row["egress"], "cut")
            self.assertEqual(row["state"], "running")
            self.assertNotIn("sparkdev-proxy-" + job["job"], self.docker.containers)
        self.assertFalse(any(call[0] == "stop" for call in self.docker.calls))
        self.rpc("resources", internet=True)
        self.c.tick()
        self.assertEqual(self.c.get_job(first["job"])["egress"], "cut")

    def test_one_cleanup_conflict_does_not_leak_every_later_job(self):
        first, _ = self.run_job()
        self.c.tick()
        second, _ = self.run_job(project="c" * 64, request_id="second")
        self.c.tick()
        for job in (first, second):
            self.c.finish(self.c.get_job(job["job"]), "succeeded", 0)
        self.docker.containers["sparkdev-proxy-" + first["job"]]["Config"]["Labels"] = {"com.delores.sparkdev.job": "other"}
        self.c.tick()
        self.assertIn("sparkdev-proxy-" + first["job"], self.docker.containers)
        self.assertNotIn("sparkdev-proxy-" + second["job"], self.docker.containers)
        self.assertEqual(self.c.get_job(second["job"])["proxy_cleaned"], 1)

    def test_cleanup_preserves_counter_receipt_and_removes_only_job_scratch(self):
        job, _ = self.run_job()
        self.c.tick()
        root = self.root / "jobs" / job["job"]
        (root / "scratch" / "temporary.bin").write_bytes(b"fixture")
        counters = dict.fromkeys(controller.INTERNET_COUNTERS, 0)
        counters["bytes_down"] = 2 ** 34
        (root / "egress" / "counters.json").write_text(json.dumps(counters))
        self.c.finish(self.c.get_job(job["job"]), "succeeded", 0)
        self.c.tick()
        self.assertEqual(self.c.job_status(self.c.get_job(job["job"]))["internet_use"], counters)
        self.assertFalse((root / "scratch").exists())
        self.assertTrue((root / "work").is_dir())

    def test_invalid_counter_bytes_never_enter_receipts(self):
        job, _ = self.run_job()
        self.c.tick()
        path = self.root / "jobs" / job["job"] / "egress" / "counters.json"
        for value in ({**dict.fromkeys(controller.INTERNET_COUNTERS, 0), "url": "private"},
                      {**dict.fromkeys(controller.INTERNET_COUNTERS, 0), "bytes_up": True},
                      {**dict.fromkeys(controller.INTERNET_COUNTERS, 0), "bytes_up": -1}):
            path.write_text(json.dumps(value))
            self.c.collect_internet_use(self.c.get_job(job["job"]))
            self.assertIsNone(self.c.get_job(job["job"])["internet_use"])

    def test_disk_floor_stops_one_newest_job_per_tick(self):
        first, _ = self.run_job()
        self.c.tick()
        second, _ = self.run_job(project="c" * 64, request_id="second")
        self.c.tick()
        self.c.db.execute("UPDATE jobs SET started=1 WHERE id=?", (first["job"],))
        self.c.db.execute("UPDATE jobs SET started=2 WHERE id=?", (second["job"],))
        self.c.disk_probe = lambda: (15 * controller.GIB, 1000 * controller.GIB)
        self.c.tick()
        self.assertEqual(self.c.get_job(second["job"])["error"], "disk_full")
        self.assertEqual(self.c.get_job(first["job"])["state"], "running")

    def test_hf_credential_shape_is_refused(self):
        self.assertIsNotNone(controller.SECRET.search(b"hf_" + b"a" * 34))


class NetworkGuardTests(unittest.TestCase):
    def test_connected_route_and_fib_local_addresses(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "net").mkdir()
            (root / "net" / "route").write_text("Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\neth0 0000A8C0 00000000 0001 0 0 100 00FFFFFF 0 0 0\neth0 00000000 0100A8C0 0003 0 0 100 00000000 0 0 0\n")
            (root / "net" / "fib_trie").write_text("Main:\n  +-- 8.8.8.9/32 2 0 2\n     |-- 8.8.8.9\n        /32 host LOCAL\n  |-- 192.168.0.255\n     /32 link BROADCAST\n")
            self.assertEqual(controller.internet_deny(root), ["192.168.0.0/24", "8.8.8.9/32"])
            (root / "net" / "fib_trie").unlink()
            with self.assertRaises(Rejected):
                controller.internet_deny(root)

    def test_bridge_ipv6_must_be_proven_disabled(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            network = {"Id": "a" * 64, "Options": {}}
            controller.guard_bridge_ipv6(network, root)
            conf = root / "sys" / "net" / "ipv6" / "conf" / ("br-" + "a" * 12)
            conf.mkdir(parents=True)
            with self.assertRaises(Rejected):
                controller.guard_bridge_ipv6(network, root)
            (conf / "disable_ipv6").write_text("1\n")
            controller.guard_bridge_ipv6(network, root)
            (conf / "disable_ipv6").write_text("0\n")
            (conf / "addr_gen_mode").write_text("1\n")
            controller.guard_bridge_ipv6(network, root)
            (conf / "addr_gen_mode").write_text("0\n")
            with self.assertRaises(Rejected):
                controller.guard_bridge_ipv6(network, root)


if __name__ == "__main__":
    unittest.main()
