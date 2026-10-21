"""Source-bound runtime upgrades; subprocesses are mocked, no Docker or service writes."""
import base64
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import unittest
from unittest import mock

import bootstrap

spec = importlib.util.spec_from_file_location("runtime_installer", Path(__file__).with_name("install-runtime.py"))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
IMAGE = "sha256:" + "b" * 64
OLD_IMAGE = "sha256:" + "a" * 64


class RefreshTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.home = Path(self.temp.name)
        self.root = self.home / "sparkdev"
        self.root.mkdir()
        self.files = {name: ("fixture:" + name).encode() for name in bootstrap.REQUIRED | {"bootstrap.py"}}
        self.envelope = {"v": 1, "build_runtime": True, "files": {
            name: {"data": base64.b64encode(raw).decode(), "sha256": hashlib.sha256(raw).hexdigest()}
            for name, raw in self.files.items()}}
        self.digest = bootstrap.runtime_source_digest(self.files)
        self.calls = []

    def tearDown(self):
        for root, dirs, files in os.walk(self.temp.name):
            for name in dirs + files:
                os.chmod(Path(root) / name, 0o700)
        self.temp.cleanup()

    def registry(self, image=OLD_IMAGE):
        value = {"v": 1, "runtimes": {name: {"image": image, "provenance": "fixture"} for name in ("rust", "node", "python")}}
        (self.root / "runtimes.json").write_text(json.dumps(value))
        return value

    def fake_run(self, args, **kwargs):
        self.calls.append(args)
        output = (IMAGE + " arm64\n").encode() if args[:3] == ["docker", "image", "inspect"] else b""
        return subprocess.CompletedProcess(args, 1 if "is-active" in args else 0, stdout=output, stderr=b"")

    @contextlib.contextmanager
    def setup_context(self):
        with mock.patch.object(bootstrap.Path, "home", return_value=self.home), mock.patch.object(bootstrap, "runtime_lock", side_effect=lambda *_: contextlib.nullcontext()), mock.patch.object(bootstrap.subprocess, "run", side_effect=self.fake_run):
            yield

    def active(self, state):
        path = self.root / "state" / "state.sqlite3"
        path.parent.mkdir()
        with contextlib.closing(sqlite3.connect(path)) as db:
            db.execute("CREATE TABLE jobs(id TEXT, state TEXT)")
            db.execute("INSERT INTO jobs VALUES('fixture',?)", (state,))
            db.commit()

    def test_existing_registry_without_source_receipt_requests_one_refresh(self):
        original = self.registry()
        with self.setup_context():
            receipt = bootstrap.install(self.envelope)
        self.assertEqual(receipt["runtime_state"], "scheduled")
        self.assertEqual(receipt["runtime_source_digest"], self.digest)
        launch = [args for args in self.calls if args[0] == "systemd-run"]
        self.assertEqual(len(launch), 1)
        self.assertEqual(launch[0][-4:], ["--source-digest", self.digest, "--attempt-id", receipt["runtime_attempt_id"]])
        pending = json.loads((self.root / receipt["runtime_build_receipt"]).read_text())
        self.assertEqual(pending["state"], "scheduled")
        self.assertEqual(pending["source_digest"], self.digest)
        self.assertEqual(json.loads((self.root / "runtimes.json").read_text()), original)

    def test_matching_receipt_and_registry_do_not_rebuild(self):
        self.registry(IMAGE)
        (self.root / "runtime-build.json").write_text(json.dumps({"state": "succeeded", "image": IMAGE, "source_digest": self.digest}))
        with self.setup_context():
            receipt = bootstrap.install(self.envelope)
        self.assertEqual(receipt["runtime_state"], "provisioned")
        self.assertFalse(any(args[0] == "systemd-run" for args in self.calls))

    def current_receipt(self):
        self.registry(IMAGE)
        (self.root / "runtime-build.json").write_text(json.dumps({"state": "succeeded", "image": IMAGE, "source_digest": self.digest}))

    def test_pruned_pinned_image_refreshes_even_with_matching_receipts(self):
        self.current_receipt()
        def run(args, **kwargs):
            if args[:3] == ["docker", "image", "inspect"]:
                self.assertEqual(args[-1], IMAGE)
                self.assertEqual(kwargs["timeout"], 10)
                return subprocess.CompletedProcess(args, 1, stdout=b"", stderr=("Error response from daemon: No such image: " + IMAGE + "\n").encode())
            return self.fake_run(args, **kwargs)
        with self.setup_context(), mock.patch.object(bootstrap.subprocess, "run", side_effect=run):
            receipt = bootstrap.install(self.envelope)
        self.assertEqual(receipt["runtime_state"], "scheduled")
        self.assertEqual(len([args for args in self.calls if args[0] == "systemd-run"]), 1)

    def test_daemon_unavailable_or_timeout_never_looks_like_pruned_image(self):
        self.current_receipt()
        outcomes = [subprocess.CompletedProcess([], 1, stdout=b"", stderr=b"Cannot connect to the Docker daemon"),
                    subprocess.CompletedProcess([], 1, stdout=b"", stderr=b"unexpected inspect failure"),
                    subprocess.TimeoutExpired("docker", 10), FileNotFoundError("docker")]
        for result in outcomes:
            with self.subTest(result=type(result).__name__), self.setup_context():
                patch = mock.patch.object(bootstrap.subprocess, "run", side_effect=result) if isinstance(result, Exception) else mock.patch.object(bootstrap.subprocess, "run", return_value=result)
                with patch, self.assertRaises(bootstrap.RuntimeRefreshError) as error:
                    bootstrap.install(self.envelope)
                self.assertEqual(error.exception.code, "runtime_image_check_unavailable")
        self.assertFalse((self.root / "controller.py").exists())
        self.assertFalse(any(args[0] == "systemd-run" for args in self.calls))

    def test_unexpected_image_identity_or_architecture_fails_closed(self):
        self.current_receipt()
        for output in ((OLD_IMAGE + " arm64\n").encode(), (IMAGE + " amd64\n").encode(), b""):
            with self.subTest(output=output), mock.patch.object(bootstrap.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, stdout=output, stderr=b"")):
                with self.assertRaises(bootstrap.RuntimeRefreshError) as error:
                    bootstrap.runtime_current(self.root, self.digest)
                self.assertEqual(error.exception.code, "runtime_image_identity_mismatch")

    def test_launch_failure_records_not_started_in_its_attempt(self):
        def run(args, **kwargs):
            if args[0] == "systemd-run":
                raise subprocess.CalledProcessError(1, args)
            return self.fake_run(args, **kwargs)
        with self.setup_context(), mock.patch.object(bootstrap.subprocess, "run", side_effect=run), self.assertRaises(subprocess.CalledProcessError):
            bootstrap.install(self.envelope)
        receipts = list((self.root / "runtime-build-attempts").glob("*.json"))
        self.assertEqual(len(receipts), 1)
        receipt = json.loads(receipts[0].read_text())
        self.assertEqual((receipt["state"], receipt["error"]), ("not_started", "runtime_launch_unavailable"))

    @contextlib.contextmanager
    def child_context(self, lock=None):
        with mock.patch.object(installer.Path, "home", return_value=self.home), mock.patch.object(installer.os, "nice", create=True), mock.patch.object(installer, "runtime_lock", side_effect=lock or (lambda *_, **__: contextlib.nullcontext())), mock.patch.object(installer.subprocess, "run", side_effect=self.fake_run), mock.patch("builtins.print"):
            yield

    def test_child_refuses_job_queued_during_handoff_and_records_receipt(self):
        original = self.registry()
        self.active("queued")
        attempt = "c" * 32
        with self.child_context():
            self.assertEqual(installer.main(["--source-digest", self.digest, "--attempt-id", attempt]), 1)
        receipt = json.loads(bootstrap.runtime_attempt_path(self.root, attempt).read_text())
        self.assertEqual((receipt["state"], receipt["error"]), ("not_started", "runtime_work_active"))
        self.assertEqual(receipt["source_digest"], self.digest)
        self.assertEqual(json.loads((self.root / "runtime-build.json").read_text()), receipt)
        self.assertEqual(json.loads((self.root / "runtimes.json").read_text()), original)
        self.assertFalse(any(args[:2] == ["docker", "build"] for args in self.calls))

    def test_bounded_lock_refusal_preserves_other_builder_receipt(self):
        active = {"state": "building", "source_digest": "d" * 64, "attempt_id": "e" * 32}
        (self.root / "runtime-build.json").write_text(json.dumps(active))
        def busy(root, wait):
            self.assertEqual(root, self.root)
            self.assertEqual(wait, 30)
            raise bootstrap.RuntimeRefreshError("runtime_refresh_lock_busy", "busy")
        attempt = "c" * 32
        with self.child_context(busy):
            self.assertEqual(installer.main(["--source-digest", self.digest, "--attempt-id", attempt]), 1)
        receipt = json.loads(bootstrap.runtime_attempt_path(self.root, attempt).read_text())
        self.assertEqual((receipt["state"], receipt["error"]), ("not_started", "runtime_refresh_lock_busy"))
        self.assertEqual(json.loads((self.root / "runtime-build.json").read_text()), active)
        self.assertFalse(self.calls)

    def test_active_job_or_server_refuses_before_any_source_overwrite(self):
        for state in ("queued", "starting", "running"):
            with self.subTest(state=state):
                if not (self.root / "state" / "state.sqlite3").exists():
                    self.active(state)
                else:
                    with contextlib.closing(sqlite3.connect(self.root / "state" / "state.sqlite3")) as db:
                        db.execute("UPDATE jobs SET state=?", (state,))
                        db.commit()
                marker = self.root / "controller.py"
                marker.write_bytes(b"old controller")
                with self.setup_context(), self.assertRaisesRegex(ValueError, "jobs and model servers"):
                    bootstrap.install(self.envelope)
                self.assertEqual(marker.read_bytes(), b"old controller")
                self.assertFalse(any(args[0] == "systemd-run" for args in self.calls))

    def test_live_docker_work_and_unavailable_docker_fail_closed(self):
        for code, output in ((0, b"container-id\n"), (1, b"")):
            with mock.patch.object(bootstrap.subprocess, "run", return_value=subprocess.CompletedProcess([], code, stdout=output)):
                with self.assertRaises(ValueError):
                    bootstrap.ensure_runtime_idle(self.root)

    def test_active_builder_never_launches_duplicate(self):
        def active(args, **kwargs):
            result = self.fake_run(args, **kwargs)
            result.returncode = 0
            return result
        with self.setup_context(), mock.patch.object(bootstrap.subprocess, "run", side_effect=active), self.assertRaisesRegex(ValueError, "already starting"):
            bootstrap.install(self.envelope)
        self.assertFalse(any(args[0] == "systemd-run" for args in self.calls))

    def test_new_modules_are_required_and_hash_checked(self):
        for name in ("model_cache.py", "model_server.py"):
            envelope = {**self.envelope, "files": {key: value for key, value in self.envelope["files"].items() if key != name}}
            with self.setup_context(), self.assertRaisesRegex(ValueError, "file set"):
                bootstrap.install(envelope)
        envelope = json.loads(json.dumps(self.envelope))
        envelope["files"]["model_cache.py"]["sha256"] = "0" * 64
        with self.setup_context(), self.assertRaisesRegex(ValueError, "digest mismatch"):
            bootstrap.install(envelope)

    def source_files(self):
        for name in bootstrap.RUNTIME_SOURCES:
            path = self.root / name
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(self.files[name])

    def test_immutable_build_snapshot_and_changed_input_refused(self):
        self.source_files()
        digest, context = installer.runtime_inputs(self.root, self.digest)
        self.assertEqual(digest, self.digest)
        self.assertEqual(context.parent, self.root / "runtime-build-inputs")
        self.assertEqual((context / "Dockerfile").read_bytes(), self.files["runtime/Dockerfile"])
        (self.root / "runtime" / "Dockerfile").write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "changed after setup"):
            installer.runtime_inputs(self.root, self.digest)
        self.assertEqual((context / "Dockerfile").read_bytes(), self.files["runtime/Dockerfile"])

    def test_failed_build_keeps_previous_runtime_registry(self):
        self.source_files()
        original = self.registry()
        def run(args, **kwargs):
            return subprocess.CompletedProcess(args, 7 if args[:2] == ["docker", "build"] else 0, stdout=b"")
        with mock.patch.object(installer.subprocess, "run", side_effect=run), mock.patch.object(installer, "register_runtimes") as register:
            self.assertEqual(installer.build_runtime(self.root, self.digest), 7)
            register.assert_not_called()
        self.assertEqual(json.loads((self.root / "runtimes.json").read_text()), original)
        self.assertEqual(json.loads((self.root / "runtime-build.json").read_text())["state"], "failed")

    def test_success_records_source_only_after_probe_and_registration(self):
        self.source_files()
        self.registry()
        outputs = [json.dumps([{"Id": IMAGE, "Architecture": "arm64"}]).encode(), b"fixture runtime verified\n"]
        def register(root, updates):
            installer.atomic(root / "runtimes.json", installer.merge_registry(self.registry(), updates))
        with mock.patch.object(installer.subprocess, "run", side_effect=self.fake_run), mock.patch.object(installer.subprocess, "check_output", side_effect=outputs) as probe, mock.patch.object(installer, "register_runtimes", side_effect=register), mock.patch("builtins.print"):
            self.assertEqual(installer.build_runtime(self.root, self.digest), 0)
            self.assertTrue(bootstrap.runtime_current(self.root, self.digest))
        build = next(args for args in self.calls if args[:2] == ["docker", "build"])
        self.assertEqual(build[-1], str(self.root / "runtime-build-inputs" / self.digest))
        self.assertIn("site.ENABLE_USER_SITE", probe.call_args_list[-1].args[0][-1])


if __name__ == "__main__":
    unittest.main()
