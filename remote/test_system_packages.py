"""No Docker, apt, root or network required; scheduler and package-boundary fixtures."""
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest import mock

import controller
import prepare
import system_packages as sp
import test_controller as fixtures

BASE = "sha256:" + "a" * 64
DERIVED = "sha256:" + "d" * 64
PROJECT = "b" * 64
MANIFEST = "c" * 64
DEB = b"fixture deb archive"


def lock():
    return {"v": 1, "manager": "apt", "base_image": BASE, "distro": {"id": "ubuntu", "version_id": "24.04"},
            "architecture": "arm64", "packages": [{"name": "fonts-example", "version": "1:2.3-4ubuntu1",
                "url": "https://archive.ubuntu.com/ubuntu/pool/fonts-example.deb", "sha256": sp.sha(DEB), "bytes": len(DEB)}],
            "requested": ["fonts-example"]}


class BoundaryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_closed_lock_schema_exact_versions_hashes_and_bounds(self):
        self.assertEqual(sp.validate(lock()), lock())
        invalid = []
        for key, value in (("v", True), ("manager", "shell"), ("base_image", "ubuntu:latest"), ("architecture", "any")):
            item = lock(); item[key] = value; invalid.append(item)
        for key, value in (("version", "latest"), ("name", "--allow-unauthenticated"), ("version", "1.*"),
                           ("sha256", "b" * 63), ("bytes", True), ("bytes", sp.DEB_LIMIT + 1)):
            item = lock(); item["packages"][0][key] = value; invalid.append(item)
        item = lock(); item["packages"].append(copy.deepcopy(item["packages"][0])); invalid.append(item)
        item = lock(); item["script"] = "apt install anything"; invalid.append(item)
        item = lock(); item["distro"]["id"] = []; invalid.append(item)
        for value in invalid:
            with self.subTest(value=value), self.assertRaises(sp.Invalid):
                sp.validate(value)

    def test_package_urls_cannot_inject_credentials_redirects_or_private_literals(self):
        for address in ("http://deb.debian.org/p.deb", "https://user:pass@deb.debian.org/p.deb", "https://deb.debian.org/p.deb?token=x",
                        "https://deb.debian.org/p.deb#x", "https://127.0.0.1/p.deb", "https://192.168.1.1/p.deb",
                        "https://169.254.169.254/p.deb", "https://deb.debian.org:80/p.deb", "https://deb.debian.org/p.deb\r\nX: x"):
            with self.subTest(address=address), self.assertRaises(sp.Invalid):
                sp.url(address)

    def test_lock_reads_refuse_hardlink_and_oversize(self):
        file = self.root / sp.LOCK
        file.write_bytes(sp.canonical(lock()))
        self.assertEqual(sp.read_lock(file), lock())
        other = self.root / "linked"
        os.link(file, other)
        with self.assertRaises(sp.Invalid):
            sp.read_lock(file)
        other.unlink()
        file.write_bytes(b" " * (sp.LIMIT + 1))
        with self.assertRaises(sp.Invalid):
            sp.read_lock(file)

    def test_deb_bytes_and_package_metadata_must_match(self):
        value = lock()
        file = self.root / (sp.sha(DEB) + ".deb")
        file.write_bytes(DEB)
        with mock.patch.object(sp, "deb_metadata", return_value=["fonts-example", "1:2.3-4ubuntu1", "all"]):
            sp.verify_debs(value, self.root)
        for metadata in (["different", "1:2.3-4ubuntu1", "all"], ["fonts-example", "9", "arm64"], ["fonts-example", "1:2.3-4ubuntu1", "amd64"]):
            with mock.patch.object(sp, "deb_metadata", return_value=metadata), self.assertRaises(sp.Invalid):
                sp.verify_debs(value, self.root)
        file.write_bytes(b"X" * len(DEB))
        with self.assertRaises(sp.Invalid):
            sp.verify_debs(value, self.root)

    def test_download_fixed_proxy_tls_no_redirect_or_hash_bypass(self):
        class Reply(io.BytesIO):
            status = 200
            def getheader(self, name): return str(len(DEB))
        connections = []
        class Connection:
            def __init__(self, *args, **kwargs): self.args = args; self.kwargs = kwargs; connections.append(self)
            def set_tunnel(self, *args): self.tunnel = args
            def request(self, *args, **kwargs): self.requested = (args, kwargs)
            def getresponse(self): return Reply(DEB)
            def close(self): pass
        destination = self.root / "reply.deb"
        sp.download(lock()["packages"][0], destination, Connection)
        self.assertEqual(destination.read_bytes(), DEB)
        self.assertEqual(connections[0].args, ("sparkdev-proxy", 3128))
        self.assertEqual(connections[0].tunnel, ("archive.ubuntu.com", 443))
        self.assertIn("context", connections[0].kwargs)
        destination.unlink()
        with mock.patch.object(Reply, "status", 302), self.assertRaises(sp.Invalid):
            sp.download(lock()["packages"][0], destination, Connection)
        self.assertFalse(destination.exists())
        self.assertFalse(destination.with_suffix(".pending").exists())

    def test_generator_resolves_closure_nonroot_without_install_and_no_overwrite(self):
        calls = []
        def command(args, **kwargs):
            calls.append((args, kwargs))
            if "--print-uris" in args:
                return b"'https://archive.ubuntu.com/ubuntu/pool/fonts-example.deb' fonts-example.deb 19 MD5Sum:unused\n"
            if "--download-only" in args:
                archives = next(s.split("=", 1)[1] for s in args if s.startswith("Dir::Cache::archives="))
                (Path(archives) / "fonts-example.deb").write_bytes(DEB)
            return b""
        output = self.root / sp.LOCK
        with mock.patch.object(sp.os, "geteuid", return_value=1000, create=True), mock.patch.object(sp, "MARKER", self.root / "missing"), \
                mock.patch.object(sp, "platform", return_value=(lock()["distro"], "arm64")), mock.patch.object(sp, "run", side_effect=command), \
                mock.patch.object(sp, "deb_metadata", return_value=["fonts-example", "1:2.3-4ubuntu1", "all"]):
            result = sp.generate_lock(["fonts-example"], output, BASE)
            self.assertEqual(result["packages"][0]["sha256"], sp.sha(DEB))
            self.assertEqual(result["base_image"], BASE)
            with self.assertRaises(FileExistsError):
                sp.generate_lock(["fonts-example"], output, BASE)
        self.assertTrue(all("--download-only" in args for args, _ in calls if "install" in args))
        self.assertTrue(all("Acquire::https::Proxy=http://sparkdev-proxy:3128" in args for args, _ in calls))
        self.assertTrue(all("APT_CONFIG" in kwargs["env"] for _, kwargs in calls))

    def test_generator_refuses_root_and_derived_context_and_unsafe_requests(self):
        with mock.patch.object(sp.os, "geteuid", return_value=0, create=True), self.assertRaises(sp.Invalid):
            sp.generate_lock(["fonts"], self.root / sp.LOCK, BASE)
        with mock.patch.object(sp.os, "getuid", return_value=0, create=True), self.assertRaises(sp.Invalid):
            sp.SystemPackages.user()
        marker = self.root / "marker"; marker.touch()
        with mock.patch.object(sp.os, "geteuid", return_value=1000, create=True), mock.patch.object(sp, "MARKER", marker), self.assertRaises(sp.Invalid):
            sp.generate_lock(["fonts"], self.root / sp.LOCK, BASE)
        for arg in ("--allow-unauthenticated", "foo;id", "foo/latest", "foo=1.*", "foo=1=2"):
            with self.assertRaises(sp.Invalid): sp.package_argument(arg)

    def test_apt_sources_keep_signatures_and_supported_distribution(self):
        text = sp.sources(lock()["distro"], "arm64")
        self.assertIn("https://ports.ubuntu.com/ubuntu-ports", text)
        self.assertIn("signed-by=/usr/share/keyrings/ubuntu-archive-keyring.gpg", text)
        self.assertNotIn("trusted=yes", text)
        self.assertIn("https://deb.debian.org/debian trixie", sp.sources({"id": "debian", "version_id": "13"}, "arm64"))

    def test_uri_plan_refuses_paths_duplicates_and_unbounded_downloads(self):
        for raw in (b"'https://deb.debian.org/a.deb' ../a.deb 12 MD5:x", b"'http://deb.debian.org/a.deb' a.deb 12 MD5:x",
                    b"'https://deb.debian.org/a.deb' a.deb 999999999999 MD5:x",
                    b"'https://deb.debian.org/a.deb' a.deb", b"'https://deb.debian.org/a.deb' a.deb 12 MD5:x extra",
                    b"'https://deb.debian.org/a.deb' a.deb 0", b"'https://deb.debian.org/a.deb' a.deb invalid",
                    b"'https://deb.debian.org/a.deb' a.deb 12\n'https://deb.debian.org/b.deb' a.deb 12"):
            with self.assertRaises(sp.Invalid): sp.parse_uris(raw)

    def test_debian_security_uri_plan_without_printed_checksum(self):
        # Physical Spark diagnostic d8d4b70b98534ad4b494572f23a6733a, Debian 13.
        address = "https://security.debian.org/debian-security/pool/updates/main/libp/libpng1.6/libpng16-16t64_1.6.48-1%2bdeb13u6_arm64.deb"
        filename = "libpng16-16t64_1.6.48-1+deb13u6_arm64.deb"
        raw = (f"'{address}' {filename} 275220 \n"
               "'https://deb.debian.org/other.deb' other.deb 19 SHA256:printed\n").encode()
        self.assertEqual(sp.parse_uris(raw), {filename: (address, 275220),
                                            "other.deb": ("https://deb.debian.org/other.deb", 19)})

    def test_generator_hashes_actual_archive_without_printed_checksum(self):
        calls = []
        def command(args, **kwargs):
            calls.append(args)
            if "--print-uris" in args:
                return b"'https://deb.debian.org/fonts-example.deb' fonts-example.deb 19 \n"
            if "--download-only" in args:
                archives = next(s.split("=", 1)[1] for s in args if s.startswith("Dir::Cache::archives="))
                (Path(archives) / "fonts-example.deb").write_bytes(DEB)
            return b""
        with mock.patch.object(sp.os, "geteuid", return_value=1000, create=True), mock.patch.object(sp, "MARKER", self.root / "missing"), \
                mock.patch.object(sp, "platform", return_value=({"id": "debian", "version_id": "13"}, "arm64")), \
                mock.patch.object(sp, "run", side_effect=command), \
                mock.patch.object(sp, "deb_metadata", return_value=["fonts-example", "1:2.3-4ubuntu1", "all"]):
            result = sp.generate_lock(["fonts-example"], self.root / sp.LOCK, BASE)
        self.assertEqual(result["packages"][0]["sha256"], sp.sha(DEB))
        self.assertEqual(result["packages"][0]["bytes"], len(DEB))
        self.assertTrue(all("APT::Get::AllowUnauthenticated=false" in args for args in calls))
        self.assertTrue(all("Acquire::AllowInsecureRepositories=false" in args for args in calls))
        self.assertTrue(all("Acquire::AllowDowngradeToInsecureRepositories=false" in args for args in calls))

    def test_offline_install_verifies_entire_package_delta(self):
        result = lock(); marker = self.root / "system-image.json"
        before = {"base": "1"}
        after = {"base": "1", "fonts-example": "1:2.3-4ubuntu1"}
        with mock.patch.object(sp.os, "geteuid", return_value=0, create=True), mock.patch.object(sp, "check_platform"), \
                mock.patch.object(sp, "verify_debs"), mock.patch.object(sp, "installed", side_effect=[before, after]), mock.patch.object(sp, "run") as command:
            sp.install(result, self.root, marker)
            args = command.call_args.args[0]
            self.assertIn("--no-download", args)
            self.assertIn("--no-remove", args)
            self.assertNotIn("update", args)
            self.assertEqual(json.loads(marker.read_bytes())["lock_sha256"], sp.sha(sp.canonical(result)))
        marker.unlink()
        for after in ({"base": "2", "fonts-example": "1:2.3-4ubuntu1"}, {"base": "1", "fonts-example": "1:2.3-4ubuntu1", "unlocked": "1"}, {"base": "1"}):
            with mock.patch.object(sp.os, "geteuid", return_value=0, create=True), mock.patch.object(sp, "check_platform"), \
                    mock.patch.object(sp, "verify_debs"), mock.patch.object(sp, "installed", side_effect=[before, after]), mock.patch.object(sp, "run"), self.assertRaises(sp.Invalid):
                sp.install(result, self.root, marker)
            self.assertFalse(marker.exists())

    def test_system_only_prepare_needs_verified_marker(self):
        (self.root / sp.LOCK).write_bytes(sp.canonical(lock()))
        with mock.patch.object(prepare, "WORK", self.root), mock.patch.object(prepare.sys, "argv", ["prepare.py", "python"]), \
                mock.patch.dict(os.environ, {"SPARKDEV_SYSTEM_PREPARED": "1"}), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(prepare.main(), 0)
        with mock.patch.object(prepare, "WORK", self.root), mock.patch.object(prepare.sys, "argv", ["prepare.py", "python"]), \
                mock.patch.dict(os.environ, {"SPARKDEV_SYSTEM_PREPARED": "0"}), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(prepare.main(), 65)


class Docker:
    def __init__(self):
        self.calls, self.containers = [], {}
        self.images = {BASE: {"Id": BASE, "Config": {"User": "", "Env": ["NVIDIA_VISIBLE_DEVICES=all"]}}}
        self.fail_create = False

    def inspect(self, name): return self.containers.get(name)

    def invoke(self, args, timeout):
        image = self.images.get(args[-1])
        return SimpleNamespace(returncode=0 if image else 1, stdout=json.dumps([image]).encode() if image else b"", stderr=b"" if image else b"No such image")

    def call(self, args, timeout=30):
        self.calls.append(args)
        if args[0] == "create":
            if self.fail_create: raise controller.Rejected("docker_failed", "fixture failure")
            name = args[args.index("--name") + 1]
            labels = dict(arg.split("=", 1) for i, arg in enumerate(args) if i and args[i-1] == "--label")
            sysctls = dict(arg.split("=", 1) for i, arg in enumerate(args) if i and args[i-1] == "--sysctl")
            self.containers[name] = {"Image": args[args.index("--entrypoint") + 2], "Config": {"Labels": labels, "User": args[args.index("--user")+1]},
                "HostConfig": {"NetworkMode": args[args.index("--network")+1], "Sysctls": sysctls}, "State": {"Status": "created", "ExitCode": 0}}
        elif args[0] == "start": self.containers[args[1]]["State"]["Status"] = "running"
        elif args[0] == "rm": self.containers.pop(args[-1])
        elif args[0] == "commit":
            changes = [arg for i, arg in enumerate(args) if i and args[i-1] == "--change"]
            labels = dict(arg[6:].split("=", 1) for arg in changes if arg.startswith("LABEL "))
            user = next(arg[5:] for arg in changes if arg.startswith("USER "))
            env = [arg[4:] for arg in changes if arg.startswith("ENV ")]
            image = {"Id": DERIVED, "Config": {"Labels": labels, "User": user, "Env": env}}
            self.images[args[-1]] = image; self.images[DERIVED] = image
        return b""


class SchedulerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.db = sqlite3.connect(":memory:", isolation_level=None); self.db.row_factory = sqlite3.Row; self.addCleanup(self.db.close)
        self.db.execute("CREATE TABLE jobs(id TEXT PRIMARY KEY,state TEXT,image TEXT,cancel INTEGER,started REAL,proxy_cleaned INTEGER DEFAULT 0)")
        self.db.execute("INSERT INTO jobs(id,state,image,cancel) VALUES('j1','starting',?,0)", (BASE,))
        (self.root / "jobs" / "j1").mkdir(parents=True)
        self.snapshots = self.root / "snapshots"; (self.snapshots / MANIFEST).mkdir(parents=True)
        self.docker = Docker()
        self.finished, self.proxy_views, self.removed = [], [], []
        self.enabled = True
        self.c = SimpleNamespace(root=self.root, db=self.db, snapshots=self.snapshots, docker=self.docker,
            internet_policy=lambda: {"enabled": self.enabled}, ensure_proxy=lambda row: self.proxy_views.append(row),
            remove_proxy=lambda row: self.removed.append(row["id"]), require_disk=lambda size: None,
            transaction=lambda: contextlib.nullcontext(), finish=self.finish)
        self.manager = sp.SystemPackages(self.c, controller)
        self.patcher = mock.patch.object(sp.SystemPackages, "user", return_value="1000:1000"); self.patcher.start(); self.addCleanup(self.patcher.stop)

    def finish(self, row, state, code, error):
        self.finished.append((state, code, error)); self.db.execute("UPDATE jobs SET state=? WHERE id=?", (state, row["id"]))

    def row(self):
        row = dict(self.db.execute("SELECT * FROM jobs WHERE id='j1'").fetchone())
        return {**row, "cpu": 3.75, "memory": 7.875, "scratch_mb": 512, "proxy_image": BASE, "project": PROJECT,
                "manifest": MANIFEST, "runtime": "python", "client": "codex", "created": time.time(), "egress": None, "request": '{"op":"prepare"}'}

    def plan(self):
        (self.snapshots / MANIFEST / sp.LOCK).write_bytes(sp.canonical(lock()))
        runtime, plan = self.manager.runtime(PROJECT, MANIFEST, {"image": BASE}, preparing=True)
        self.assertEqual(runtime["image"], BASE)
        self.manager.stage(self.row(), plan)
        return plan

    def exited(self, phase, code=0):
        self.docker.containers["sparkdev-system-" + phase + "-j1"]["State"].update(Status="exited", ExitCode=code)

    def test_no_lock_is_noop_and_unprepared_run_fails(self):
        self.assertEqual(self.manager.runtime(PROJECT, MANIFEST, {"image": BASE}), ({"image": BASE}, None))
        self.plan()
        with self.assertRaises(controller.Rejected) as error:
            self.manager.runtime(PROJECT, MANIFEST, {"image": BASE})
        self.assertEqual(error.exception.code, "system_packages_unprepared")
        with self.assertRaises(controller.Rejected):
            self.manager.runtime(PROJECT, MANIFEST, {"image": DERIVED}, preparing=True)

    def test_three_phases_persist_identity_and_reuse_verified_nonroot_image(self):
        plan = self.plan()
        self.assertTrue(self.manager.reconcile(self.row()))
        self.assertEqual(self.proxy_views[0]["egress"], "internet")
        self.assertTrue(self.manager.reconcile(self.row()))  # restart/poll cannot create twice
        self.assertEqual(sum(c[0] == "create" for c in self.docker.calls), 1)
        self.exited("fetch"); self.manager.reconcile(self.row())
        self.assertEqual(self.removed, ["j1"])
        self.manager.reconcile(self.row())
        self.exited("build"); self.manager.reconcile(self.row())
        self.assertFalse(self.manager.reconcile(self.row()))
        self.assertEqual(self.row()["image"], DERIVED)
        self.assertEqual(self.row()["state"], "starting")
        runtime, cached = self.manager.runtime(PROJECT, MANIFEST, {"image": BASE})
        self.assertEqual(runtime["image"], DERIVED)
        self.assertEqual(cached["key"], plan["key"])
        self.assertIn("NVIDIA_VISIBLE_DEVICES=all", self.docker.images[DERIVED]["Config"]["Env"])
        self.assertEqual(self.manager.status(self.row())["system_packages"]["phase"], "ready")
        args = []; self.manager.create_args(self.row(), args)
        self.assertIn("SPARKDEV_SYSTEM_PREPARED=1", args)

    def test_only_builder_is_root_offline_no_writable_host_mount_and_caps_inside_job_budget(self):
        plan = self.plan()
        fetch = self.manager._args(self.row(), plan, "fetch", "fetch")
        build = self.manager._args(self.row(), plan, "build", "build")
        self.assertEqual(fetch[fetch.index("--user")+1], "1000:1000")
        self.assertEqual(build[build.index("--user")+1], "0:0")
        self.assertEqual(build[build.index("--network")+1], "none")
        self.assertIn("--read-only", fetch); self.assertNotIn("--read-only", build)
        for args in (fetch, build):
            self.assertEqual(args[args.index("--cpus")+1], "3.75")
            self.assertEqual(args[args.index("--memory")+1], "7.875g")
            self.assertEqual(args[args.index("--memory-swap")+1], "7.875g")
            self.assertNotIn("--privileged", args)
            self.assertFalse(any("docker.sock" in a for a in args))
            self.assertIn("NVIDIA_VISIBLE_DEVICES=void", args)
            log_options = [arg for i, arg in enumerate(args) if i and args[i-1] == "--log-opt"]
            self.assertEqual(log_options, ["max-size=8m", "max-file=1", "compress=false"])
        self.assertTrue(all(arg.endswith(",readonly") for i, arg in enumerate(build) if i and build[i-1] == "--mount"))
        self.assertNotIn("SYS_ADMIN", build)
        self.assertFalse(any("dst=/work" in a for a in build))

    def test_changed_lock_and_other_project_do_not_reuse_image(self):
        self.plan(); self.manager.reconcile(self.row()); self.exited("fetch"); self.manager.reconcile(self.row())
        self.manager.reconcile(self.row()); self.exited("build"); self.manager.reconcile(self.row())
        with self.assertRaises(controller.Rejected): self.manager.runtime("e" * 64, MANIFEST, {"image": BASE})
        value = lock(); value["packages"][0]["version"] = "2"
        (self.snapshots / MANIFEST / sp.LOCK).write_bytes(sp.canonical(value))
        with self.assertRaises(controller.Rejected): self.manager.runtime(PROJECT, MANIFEST, {"image": BASE})

    def test_failed_create_is_bounded_and_does_not_publish_image(self):
        self.plan(); self.docker.fail_create = True
        for _ in range(3):
            with self.assertRaises(controller.Rejected): self.manager.reconcile(self.row())
        self.assertEqual(self.finished[-1], ("failed", 125, "system_packages_launch_failed"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM system_images").fetchone()[0], 0)

    def test_failed_created_start_is_bounded_across_controller_restart(self):
        self.plan()
        original = self.docker.call
        def fail_start(args, **kwargs):
            if args[0] == "start":
                self.docker.calls.append(args)
                self.docker.containers[args[1]]["State"].update(Status="created", ExitCode=128,
                    Error="failed to initialize logging driver: compression cannot be enabled when max file count is 1")
                raise controller.Rejected("docker_failed", "fixture")
            return original(args, **kwargs)
        self.db.execute("UPDATE system_preparations SET failures=2")
        with mock.patch.object(self.docker, "call", side_effect=fail_start):
            for expected in (1, 2):
                with self.assertRaises(controller.Rejected): self.manager.reconcile(self.row())
                self.assertFalse(self.finished)
                self.assertEqual(self.db.execute("SELECT start_failures FROM system_preparations").fetchone()[0], expected)
                self.manager = sp.SystemPackages(self.c, controller)
            self.assertTrue(self.manager.reconcile(self.row()))
        self.assertEqual(self.finished, [("failed", 125, "system_packages_start_failed")])
        self.assertNotIn("sparkdev-system-fetch-j1", self.docker.containers)
        self.assertEqual(sum(c[0] == "start" for c in self.docker.calls), 3)
        self.assertFalse(self.manager.reconcile(self.row()))
        self.assertFalse(any(c[0] == "commit" for c in self.docker.calls))

    def test_uncertain_start_preserves_verified_running_container(self):
        self.plan()
        original = self.docker.call
        def lost_reply(args, **kwargs):
            result = original(args, **kwargs)
            if args[0] == "start": raise subprocess.TimeoutExpired("docker", 30)
            return result
        with mock.patch.object(self.docker, "call", side_effect=lost_reply):
            self.assertTrue(self.manager.reconcile(self.row()))
            self.assertTrue(self.manager.reconcile(self.row()))
        self.assertEqual(self.row()["state"], "running")
        self.assertEqual(self.db.execute("SELECT start_failures FROM system_preparations").fetchone()[0], 0)
        self.assertEqual(sum(c[0] == "start" for c in self.docker.calls), 1)
        self.assertFalse(self.finished)
        self.assertFalse(any(c[0] == "rm" for c in self.docker.calls))

    def test_uncertain_start_uses_verified_exit_result(self):
        self.plan()
        original = self.docker.call
        def lost_reply(args, **kwargs):
            result = original(args, **kwargs)
            if args[0] == "start":
                self.exited("fetch", 65)
                raise controller.Rejected("docker_failed", "fixture")
            return result
        with mock.patch.object(self.docker, "call", side_effect=lost_reply):
            self.assertTrue(self.manager.reconcile(self.row()))
        self.assertEqual(self.finished, [("failed", 65, "system_packages_fetch_failed")])
        self.assertEqual(self.db.execute("SELECT start_failures FROM system_preparations").fetchone()[0], 0)

    def test_uncertain_start_does_not_charge_unavailable_or_foreign_container(self):
        self.plan()
        original, inspect = self.docker.call, self.docker.inspect
        attempted = [False]
        def fail_start(args, **kwargs):
            if args[0] == "start":
                attempted[0] = True
                raise controller.Rejected("docker_failed", "fixture")
            return original(args, **kwargs)
        def unavailable(name):
            if attempted[0]: raise controller.Rejected("docker_unavailable", "fixture")
            return inspect(name)
        with mock.patch.object(self.docker, "call", side_effect=fail_start), mock.patch.object(self.docker, "inspect", side_effect=unavailable):
            with self.assertRaises(controller.Rejected): self.manager.reconcile(self.row())
        self.assertEqual(self.db.execute("SELECT start_failures FROM system_preparations").fetchone()[0], 0)
        def foreign(args, **kwargs):
            if args[0] == "start":
                self.docker.containers[args[1]]["Config"]["Labels"][sp.PREFIX + "key"] = "other"
                raise controller.Rejected("docker_failed", "fixture")
            return original(args, **kwargs)
        with mock.patch.object(self.docker, "call", side_effect=foreign):
            with self.assertRaises(controller.Rejected) as raised: self.manager.reconcile(self.row())
        self.assertEqual(raised.exception.code, "container_conflict")
        self.assertEqual(self.db.execute("SELECT start_failures FROM system_preparations").fetchone()[0], 0)
        self.assertFalse(self.finished)
        self.assertFalse(any(c[0] == "rm" for c in self.docker.calls))

    def test_legacy_preparation_schema_migrates_without_changing_existing_state(self):
        self.db.execute("DROP TABLE system_preparations")
        self.db.execute("CREATE TABLE system_preparations(job TEXT PRIMARY KEY,plan TEXT,phase TEXT,started REAL,failures INTEGER NOT NULL DEFAULT 0)")
        self.db.execute("INSERT INTO system_preparations VALUES('old','{}','fetch',NULL,2)")
        for _ in range(2): self.manager = sp.SystemPackages(self.c, controller)
        row = self.db.execute("SELECT * FROM system_preparations").fetchone()
        self.assertEqual(dict(row), {"job": "old", "plan": "{}", "phase": "fetch", "started": None, "failures": 2, "start_failures": 0})

    def test_unknown_container_never_stopped_or_committed(self):
        self.plan(); self.manager.reconcile(self.row())
        self.docker.containers["sparkdev-system-fetch-j1"]["Config"]["Labels"][sp.PREFIX + "key"] = "wrong"
        self.db.execute("UPDATE jobs SET cancel=1")
        before = len(self.docker.calls)
        with self.assertRaises(controller.Rejected): self.manager.reconcile(self.row())
        self.assertEqual(len(self.docker.calls), before)
        self.assertFalse(self.finished)

    def test_owner_off_switch_stops_fetch_and_prevents_start(self):
        self.plan(); self.enabled = False
        self.manager.reconcile(self.row())
        self.assertEqual(self.finished[-1][2], "system_packages_internet_disabled")
        self.assertFalse(self.docker.calls)

    def test_cancel_and_missing_started_container_do_not_relaunch(self):
        self.plan(); self.manager.reconcile(self.row())
        self.docker.containers.clear()
        self.manager.reconcile(self.row())
        self.assertEqual(self.finished[-1][2], "system_packages_container_lost")
        self.assertEqual(sum(c[0] == "create" for c in self.docker.calls), 1)

    def test_builder_failure_does_not_publish_derived_image(self):
        self.plan(); self.manager.reconcile(self.row()); self.exited("fetch"); self.manager.reconcile(self.row())
        self.manager.reconcile(self.row()); self.exited("build", 65); self.manager.reconcile(self.row())
        self.assertEqual(self.finished[-1], ("failed", 65, "system_packages_build_failed"))
        self.assertFalse(any(c[0] == "commit" for c in self.docker.calls))
        self.assertIn(b"system_packages_build_failed", (self.root / "jobs" / "j1" / "output" / "stdout.log").read_bytes())

    def test_guard_failure_finishes_fetch_instead_of_stalling_admission(self):
        self.plan()
        self.c.ensure_proxy = mock.Mock(side_effect=controller.Rejected("network_facts", "fixture"))
        self.manager.reconcile(self.row())
        self.assertEqual(self.finished[-1], ("failed", 125, "system_packages_network_guard_failed"))
        self.assertFalse(any(c[0] == "create" for c in self.docker.calls))

    def test_guard_change_cuts_live_fetch(self):
        self.plan(); self.manager.reconcile(self.row())
        self.c.ensure_proxy = mock.Mock(side_effect=controller.Rejected("network_changed", "fixture"))
        self.manager.reconcile(self.row())
        self.assertNotIn("sparkdev-system-fetch-j1", self.docker.containers)
        self.assertEqual(self.finished[-1][2], "system_packages_network_guard_failed")
        self.assertEqual(self.removed, ["j1"])

    def test_restart_after_fetch_exit_before_cleanup_does_not_redownload(self):
        self.plan(); self.manager.reconcile(self.row()); self.exited("fetch")
        original = self.docker.call
        def crash(args, **kwargs):
            if args == ["rm", "sparkdev-system-fetch-j1"]:
                raise RuntimeError("simulated process loss")
            return original(args, **kwargs)
        with mock.patch.object(self.docker, "call", side_effect=crash), self.assertRaises(RuntimeError):
            self.manager.reconcile(self.row())
        self.assertEqual(self.db.execute("SELECT phase FROM system_preparations").fetchone()[0], "build")
        self.manager = sp.SystemPackages(self.c, controller)
        self.manager.reconcile(self.row())
        self.assertNotIn("sparkdev-system-fetch-j1", self.docker.containers)
        self.assertEqual(self.docker.containers["sparkdev-system-build-j1"]["State"]["Status"], "running")
        self.assertEqual(sum(c[0] == "create" and "sparkdev-system-fetch-j1" in c for c in self.docker.calls), 1)

    def test_cancel_arriving_during_create_prevents_start(self):
        self.plan()
        original = self.docker.call
        def cancel(args, **kwargs):
            result = original(args, **kwargs)
            if args[0] == "create": self.db.execute("UPDATE jobs SET cancel=1")
            return result
        with mock.patch.object(self.docker, "call", side_effect=cancel):
            self.manager.reconcile(self.row())
        self.assertFalse(any(args[0] == "start" for args in self.docker.calls))
        self.assertEqual(self.finished[-1], ("cancelled", 130, "system_packages_cancelled"))


class IntegratedDocker(fixtures.FakeDocker):
    def __init__(self):
        super().__init__()
        self.images = {BASE: {"Id": BASE, "Config": {"User": "", "Env": ["NVIDIA_VISIBLE_DEVICES=all"]}}}
        self.fail_create = False

    invoke = Docker.invoke

    def call(self, args, timeout=30):
        if args[0] == "commit" or (args[0] == "create" and any(arg.startswith(sp.PREFIX + "phase=") for arg in args)):
            return Docker.call(self, args, timeout)
        return super().call(args, timeout)


class ControllerIntegrationTests(unittest.TestCase):
    setUp_fixture = fixtures.ControllerTests.setUp
    tearDown = fixtures.ControllerTests.tearDown
    snapshot = fixtures.ControllerTests.snapshot
    rpc = fixtures.ControllerTests.rpc

    def setUp(self):
        self.setUp_fixture()
        self.docker = IntegratedDocker(); self.c.docker = self.docker
        self.rpc("resources", internet=True)
        guard = mock.patch.object(controller, "internet_deny", return_value=[])
        guard.start(); self.addCleanup(guard.stop)
        user = mock.patch.object(sp.SystemPackages, "user", return_value="1000:1000")
        user.start(); self.addCleanup(user.stop)

    def submit(self):
        manifest = self.snapshot(content=sp.canonical(lock()), name=sp.LOCK)
        result = self.rpc("prepare", project=PROJECT, manifest_id=manifest, runtime="python", client="codex", request_id="system-prep")
        return result["job"], manifest

    def complete_stages(self, job):
        self.c.tick(); self.c.tick()
        fetch = "sparkdev-system-fetch-" + job
        self.assertIn(fetch, self.docker.containers)
        self.docker.containers[fetch]["State"].update(Status="exited", ExitCode=0)
        self.c.tick(); self.c.tick()
        build = "sparkdev-system-build-" + job
        self.docker.containers[build]["State"].update(Status="exited", ExitCode=0)
        self.c.tick(); self.c.tick()

    def test_preparation_stages_use_derived_image_then_nonroot_job_and_service(self):
        job, manifest = self.submit()
        self.complete_stages(job)
        args = next(args for args in self.docker.calls if args[0] == "create" and self.c.container_name(job) in args)
        self.assertIn(DERIVED, args)
        # Ordinary jobs retain the real controller owner's identity. WSL's unit
        # runner may itself be root; SystemPackages.user separately refuses a
        # real root controller before either preparation stage is launched.
        expected_user = f"{os.getuid()}:{os.getgid()}" if hasattr(os, "getuid") else "1000:1000"
        self.assertEqual(args[args.index("--user")+1], expected_user)
        self.assertIn("SPARKDEV_SYSTEM_PREPARED=1", args)
        # Normal language prep goes back through the fixed allowlist, not public mode.
        helpers = [args for args in self.docker.calls if args[0] == "create" and "sparkdev-proxy-" + job in args]
        self.assertEqual(len(helpers), 2)
        self.assertIn("SPARKDEV_PROXY_MODE=public", helpers[0]); self.assertNotIn("SPARKDEV_PROXY_MODE=public", helpers[1])
        self.docker.containers[self.c.container_name(job)]["State"].update(Status="exited", ExitCode=0)
        self.c.tick()
        run = self.rpc("run", project=PROJECT, manifest_id=manifest, runtime="python", client="codex", request_id="use-system",
                       argv=["fonts-example", "--version"], no_return=True)
        self.assertEqual(run["runtime_digest"], DERIVED)
        service = self.rpc("serve", project=PROJECT, manifest_id=manifest, runtime="python", client="codex", request_id="serve-system",
                           name="system-model", argv=["python3", "server.py"])
        self.assertEqual(service["runtime_digest"], DERIVED)
        self.assertEqual(service["system_packages"]["image"], DERIVED)

    def test_public_fetch_helper_stays_inside_preparation_quota(self):
        job, _ = self.submit()
        self.c.tick(); self.c.tick()
        row = self.c.get_job(job)
        helper = next(args for args in self.docker.calls if args[0] == "create" and "sparkdev-proxy-" + job in args)
        self.assertEqual(float(helper[helper.index("--cpus")+1]), controller.PROXY_CPU)
        self.assertEqual(helper[helper.index("--memory")+1], str(int(controller.PROXY_MEMORY_GB * 1024)) + "m")
        self.assertEqual(controller.reserved_cpu(row), row["cpu"] + controller.PROXY_CPU)
        self.assertEqual(controller.reserved_memory(row), row["memory"] + controller.PROXY_MEMORY_GB)

    def test_disk_floor_stops_root_builder_before_marking_job_terminal(self):
        job, _ = self.submit()
        self.c.tick(); self.c.tick()
        self.docker.containers["sparkdev-system-fetch-" + job]["State"].update(Status="exited", ExitCode=0)
        self.c.tick(); self.c.tick()
        build = "sparkdev-system-build-" + job
        self.assertEqual(self.docker.containers[build]["State"]["Status"], "running")
        with mock.patch.object(self.c, "free_bytes", return_value=1): self.c.tick()
        self.assertNotIn(build, self.docker.containers)
        self.assertEqual(self.c.get_job(job)["error"], "disk_full")


if __name__ == "__main__":
    unittest.main()
