"""CUDA source-bound refresh tests; all Docker/GPU activity is mocked."""
import base64
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import unittest
from unittest import mock

import bootstrap
import test_runtime_refresh as fixture

spec = importlib.util.spec_from_file_location("cuda_installer", Path(__file__).with_name("install-cuda.py"))
cuda = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cuda)


class CudaRefreshTests(unittest.TestCase):
    setUp = fixture.RefreshTests.setUp
    tearDown = fixture.RefreshTests.tearDown
    registry = fixture.RefreshTests.registry
    fake_run = fixture.RefreshTests.fake_run
    setup_context = fixture.RefreshTests.setup_context
    active = fixture.RefreshTests.active
    source_files = fixture.RefreshTests.source_files

    def cuda_bundle(self):
        for name in ("install-cuda.py", *bootstrap.CUDA_SOURCES):
            raw = ("fixture:" + name).encode()
            self.files[name] = raw
            self.envelope["files"][name] = {"data": base64.b64encode(raw).decode(), "sha256": hashlib.sha256(raw).hexdigest()}
        self.envelope["build_cuda"] = True
        return bootstrap.runtime_source_digest(self.files, bootstrap.CUDA_SOURCES)

    def cuda_sources(self):
        digest = self.cuda_bundle()
        self.source_files()
        for name in bootstrap.CUDA_SOURCES:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(self.files[name])
        return digest

    @contextlib.contextmanager
    def cuda_context(self, register=None):
        helper = {"runtime_inputs": fixture.installer.runtime_inputs,
                  "publish_runtime_status": fixture.installer.publish_runtime_status,
                  "register_runtimes": register or mock.Mock()}
        with mock.patch.object(cuda, "helpers", return_value=helper), mock.patch.object(cuda, "blender_image", return_value=(cuda.BLENDER_EXISTING, "fixture")), mock.patch.object(cuda, "require_probe_headroom"), mock.patch.object(cuda.subprocess, "run", side_effect=self.fake_run), mock.patch("builtins.print"):
            yield helper

    def test_both_refreshes_use_one_sequential_launcher_with_distinct_receipts(self):
        digest = self.cuda_bundle()
        with self.setup_context():
            receipt = bootstrap.install(self.envelope)
        launched = [a for a in self.calls if a[0] == "systemd-run"]
        self.assertEqual(len(launched), 1)
        self.assertIn("--cuda-source-digest", launched[0])
        self.assertIn(digest, launched[0])
        self.assertEqual((receipt["runtime_state"], receipt["cuda_state"]), ("scheduled", "scheduled"))
        self.assertNotEqual(receipt["runtime_attempt_id"], receipt["cuda_attempt_id"])
        self.assertEqual(json.loads((self.root / receipt["cuda_build_receipt"]).read_text())["state"], "scheduled")

    def test_existing_cuda_is_refreshed_by_runtime_setup_without_new_cuda_opt_in(self):
        self.cuda_bundle()
        self.envelope["build_cuda"] = False
        value = self.registry()
        value["runtimes"]["cuda"] = {"image": fixture.OLD_IMAGE, "gpu": True, "provenance": "fixture"}
        (self.root / "runtimes.json").write_text(json.dumps(value))
        with self.setup_context():
            receipt = bootstrap.install(self.envelope)
        self.assertEqual(receipt["cuda_state"], "scheduled")
        self.assertTrue(any("--cuda-source-digest" in args for args in self.calls))

    def test_matching_cuda_source_and_image_skips_rebuild_but_pruning_does_not(self):
        digest = self.cuda_sources()
        value = self.registry()
        value["runtimes"]["cuda"] = {"image": fixture.IMAGE, "gpu": True, "provenance": "fixture"}
        (self.root / "runtimes.json").write_text(json.dumps(value))
        (self.root / "runtime-cuda-build.json").write_text(json.dumps({"state": "succeeded", "source_digest": digest, "image": fixture.IMAGE}))
        with self.cuda_context():
            self.assertEqual(cuda.build_cuda(self.root, digest), 0)
        self.assertFalse(any(args[:2] == ["docker", "build"] for args in self.calls))
        with mock.patch.object(bootstrap.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, stdout=b"", stderr=("No such image: " + fixture.IMAGE).encode())):
            self.assertFalse(bootstrap.runtime_current(self.root, digest, cuda=True))

    def test_active_work_and_source_drift_refuse_without_build_or_registry_change(self):
        digest = self.cuda_sources()
        original = self.registry()
        (self.root / "runtime" / "Dockerfile.cuda").write_bytes(b"changed")
        with self.cuda_context():
            self.assertEqual(cuda.refresh_locked(self.root, digest, "a" * 32), 1)
        self.assertEqual(json.loads((self.root / "runtime-cuda-build.json").read_text())["state"], "not_started")
        self.active("running")
        with self.cuda_context():
            self.assertEqual(cuda.refresh_locked(self.root, digest, "b" * 32), 1)
        self.assertEqual(json.loads((self.root / "runtime-cuda-build.json").read_text())["error"], "runtime_work_active")
        self.assertEqual(json.loads((self.root / "runtimes.json").read_text()), original)
        self.assertFalse(any(args[:2] == ["docker", "build"] for args in self.calls))

    def test_failed_probe_keeps_registry_and_records_failure(self):
        digest = self.cuda_sources()
        original = self.registry()
        outputs = [json.dumps([{"Id": fixture.IMAGE, "Architecture": "arm64"}]).encode(), subprocess.CalledProcessError(1, ["docker"])]
        with self.cuda_context() as helper, mock.patch.object(cuda.subprocess, "check_output", side_effect=outputs):
            self.assertEqual(cuda.build_cuda(self.root, digest, "a" * 32), 1)
        helper["register_runtimes"].assert_not_called()
        self.assertEqual(json.loads((self.root / "runtimes.json").read_text()), original)
        self.assertEqual(json.loads((self.root / "runtime-cuda-build.json").read_text())["state"], "failed")
        self.assertEqual(json.loads((self.root / "runtime-cuda-build.json").read_text())["phase"], "gpu_probe")

    def test_unreadable_media_probe_is_identified_without_activation_or_raw_error_leak(self):
        digest = self.cuda_sources()
        original = self.registry()
        outputs = [json.dumps([{"Id": fixture.IMAGE, "Architecture": "arm64"}]).encode(), b'{"user_site":true}',
                   subprocess.CalledProcessError(2, ["docker"], stderr=b"/usr/bin/python3: can't open file '/opt/sparkdev/media_probe.py': [Errno 13] Permission denied private-fixture-text")]
        with self.cuda_context() as helper, mock.patch.object(cuda.subprocess, "check_output", side_effect=outputs):
            self.assertEqual(cuda.build_cuda(self.root, digest, "a" * 32), 1)
        helper["register_runtimes"].assert_not_called()
        self.assertEqual(json.loads((self.root / "runtimes.json").read_text()), original)
        receipt = json.loads((self.root / "runtime-cuda-build.json").read_text())
        self.assertEqual((receipt["error"], receipt["phase"], receipt["process_exit_code"]), ("cuda_media_probe_unreadable", "media_probe", 2))
        self.assertNotIn("private-fixture-text", json.dumps(receipt))

    def test_copied_runtime_probe_and_egl_metadata_are_readable_to_nonroot_users(self):
        root = Path(__file__).resolve().parents[1]
        cuda_recipe = (root / "runtimes/Dockerfile.cuda").read_text()
        blender_recipe = (root / "runtimes/Dockerfile.blender").read_text()
        self.assertIn("chmod 0755 /opt/sparkdev", cuda_recipe)
        self.assertIn("chmod 0644 /opt/sparkdev/media_probe.py /usr/share/glvnd/egl_vendor.d/10_nvidia.json", cuda_recipe)
        self.assertGreater(cuda_recipe.index("chmod 0644 /opt/sparkdev/media_probe.py"), cuda_recipe.index("COPY media_probe.py"))
        self.assertIn("chmod 0644 /usr/share/glvnd/egl_vendor.d/10_nvidia.json", blender_recipe)
        self.assertGreater(blender_recipe.index("chmod 0644 /usr/share/glvnd/egl_vendor.d/10_nvidia.json"), blender_recipe.index("COPY 10_nvidia.json"))

    def test_success_uses_immutable_context_caps_exact_worker_python_and_probe(self):
        digest = self.cuda_sources()
        outputs = [json.dumps([{"Id": fixture.IMAGE, "Architecture": "arm64"}]).encode(), b'{"user_site":true}',
                   json.dumps({"gpu_video": {"listed_encoders": ["h264_nvenc", "hevc_nvenc", "av1_nvenc"], "nvenc": False}, "blender": {"version": "4.5.9"}}).encode()]
        with self.cuda_context() as helper, mock.patch.object(cuda.subprocess, "check_output", side_effect=outputs) as check:
            self.assertEqual(cuda.build_cuda(self.root, digest, "a" * 32), 0)
        build = next(a for a in self.calls if a[:2] == ["docker", "build"])
        self.assertEqual(build[-1], str(self.root / "runtime-build-inputs" / digest))
        self.assertEqual(build[build.index("--cpu-quota") + 1], "100000")
        self.assertEqual(build[build.index("--memory") + 1], "1g")
        self.assertIn("BLENDER_IMAGE=" + cuda.BLENDER_EXISTING, build)
        probe = check.call_args_list[-2]
        self.assertEqual(probe.kwargs["timeout"], 120)
        args = probe.args[0]
        self.assertEqual(args[args.index("--entrypoint") + 1], "/usr/bin/python3")
        self.assertEqual(args[args.index("--memory") + 1], "4g")
        self.assertIn("site.ENABLE_USER_SITE", args[-1])
        self.assertIn("NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics,video", args)
        media = check.call_args_list[-1]
        self.assertEqual(media.kwargs["timeout"], 300)
        self.assertEqual(media.args[0][-2:], ["/opt/sparkdev/media_probe.py", "--probe"])
        helper["register_runtimes"].assert_called_once()
        receipt = json.loads((self.root / "runtime-cuda-build.json").read_text())
        self.assertEqual((receipt["state"], receipt["source_digest"]), ("succeeded", digest))
        self.assertFalse(receipt["probe"]["gpu_video"]["nvenc"])

    def test_existing_blender_is_only_inspected_by_exact_digest(self):
        with mock.patch.object(cuda, "inspect_image", return_value=cuda.BLENDER_EXISTING) as inspect, mock.patch.object(cuda.subprocess, "run") as run:
            image, origin = cuda.blender_image(self.root, self.root, {})
        self.assertEqual(image, cuda.BLENDER_EXISTING)
        inspect.assert_called_once_with(cuda.BLENDER_EXISTING)
        run.assert_not_called()

    def test_missing_compiled_video_support_never_activates_candidate(self):
        digest = self.cuda_sources()
        original = self.registry()
        outputs = [json.dumps([{"Id": fixture.IMAGE, "Architecture": "arm64"}]).encode(), b'{"user_site":true}',
                   b'{"gpu_video":{"listed_encoders":[]},"blender":{"version":"4.5.9"}}']
        with self.cuda_context() as helper, mock.patch.object(cuda.subprocess, "check_output", side_effect=outputs):
            self.assertEqual(cuda.build_cuda(self.root, digest, "a" * 32), 1)
        helper["register_runtimes"].assert_not_called()
        self.assertEqual(json.loads((self.root / "runtimes.json").read_text()), original)

        receipt = json.loads((self.root / "runtime-cuda-build.json").read_text())
        self.assertEqual((receipt["error"], receipt["phase"]), ("cuda_nvenc_not_compiled", "media_probe"))
        self.assertEqual(receipt["probe"]["gpu_video"]["listed_encoders"], [])

    def test_missing_owner_image_builds_bounded_standalone_blender(self):
        for name in cuda.BLENDER_FILES:
            (self.root / name).write_text("fixed source " + name)
        helper = {"atomic": fixture.installer.atomic}
        with mock.patch.object(cuda, "inspect_image", side_effect=[subprocess.CalledProcessError(1, ["docker"]), fixture.IMAGE]), \
                mock.patch.object(cuda, "helpers", return_value=helper), mock.patch.object(cuda.subprocess, "run", side_effect=self.fake_run):
            image, _ = cuda.blender_image(self.root, self.root, {})
        self.assertEqual(image, fixture.IMAGE)
        build = self.calls[-1]
        self.assertEqual(build[build.index("--cpu-quota") + 1], "200000")
        self.assertEqual(build[build.index("--memory") + 1], "8g")
        self.assertIn("JOBS=2", build)
        self.assertEqual(build[build.index("--tag") + 1], "sparkdev/blender:4.5.9-arm64")
        self.assertEqual(json.loads((self.root / "runtime-blender-build.json").read_text())["image"], fixture.IMAGE)

    def test_cuda_lock_timeout_preserves_active_builder_receipt(self):
        active = {"state": "building", "attempt_id": "b" * 32}
        (self.root / "runtime-cuda-build.json").write_text(json.dumps(active))
        with mock.patch.object(cuda.Path, "home", return_value=self.home), mock.patch.object(cuda.os, "nice", create=True), mock.patch.object(cuda, "runtime_lock", side_effect=bootstrap.RuntimeRefreshError("runtime_refresh_lock_busy", "busy")) as lock, mock.patch("builtins.print"):
            self.assertEqual(cuda.main(["--source-digest", "c" * 64, "--attempt-id", "a" * 32]), 1)
        lock.assert_called_once_with(self.root, wait=30)
        self.assertEqual(json.loads((self.root / "runtime-cuda-build.json").read_text()), active)
        self.assertEqual(json.loads(bootstrap.runtime_attempt_path(self.root, "a" * 32).read_text())["state"], "not_started")

    def test_cpu_child_runs_cuda_under_the_same_lock_after_cpu_success(self):
        events = []
        @contextlib.contextmanager
        def lock(*_, **__):
            events.append("lock")
            try:
                yield
            finally:
                events.append("unlock")
        def cpu_build(*_):
            events.append("cpu")
            return 0
        def cuda_build(*_):
            events.append("cuda")
            return 0
        with mock.patch.object(fixture.installer.Path, "home", return_value=self.home), mock.patch.object(fixture.installer.os, "nice", create=True), mock.patch.object(fixture.installer, "runtime_lock", side_effect=lock), mock.patch.object(fixture.installer, "build_runtime", side_effect=cpu_build), mock.patch("runpy.run_path", return_value={"refresh_locked": cuda_build}):
            self.assertEqual(fixture.installer.main(["--source-digest", self.digest, "--cuda-source-digest", "c" * 64, "--cuda-attempt-id", "a" * 32]), 0)
        self.assertEqual(events, ["lock", "cpu", "cuda", "unlock"])

    def test_cpu_failure_never_launches_cuda_and_preserves_its_failure_receipt(self):
        with mock.patch.object(fixture.installer.Path, "home", return_value=self.home), mock.patch.object(fixture.installer.os, "nice", create=True), mock.patch.object(fixture.installer, "runtime_lock", return_value=contextlib.nullcontext()), mock.patch.object(fixture.installer, "build_runtime", return_value=7), mock.patch("runpy.run_path") as child:
            self.assertEqual(fixture.installer.main(["--source-digest", self.digest, "--cuda-source-digest", "c" * 64, "--cuda-attempt-id", "a" * 32]), 7)
        child.assert_not_called()
        receipt = json.loads(bootstrap.runtime_attempt_path(self.root, "a" * 32).read_text())
        self.assertEqual((receipt["state"], receipt["error"]), ("not_started", "cpu_runtime_refresh_failed"))


if __name__ == "__main__":
    unittest.main()
