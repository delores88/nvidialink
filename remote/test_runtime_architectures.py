"""Architecture separation and refusal tests; no Docker containers are launched."""
import base64
import hashlib
import json
from pathlib import Path
import subprocess
import unittest
from unittest import mock

import bootstrap
import test_runtime_refresh as fixture
import test_cuda_refresh as cuda_fixture


class ArchitectureTests(unittest.TestCase):
    def test_only_supported_machine_names_are_normalized(self):
        for machine, expected in (("aarch64", "arm64"), ("ARM64", "arm64"), ("x86_64", "amd64"), ("AMD64", "amd64")):
            self.assertEqual(bootstrap.runtime_architecture(machine), expected)
        for machine in ("", "x86", "ppc64le", "riscv64", None, 7):
            with self.subTest(machine=machine), mock.patch.object(bootstrap.platform, "machine", return_value=machine):
                with self.assertRaisesRegex(ValueError, "Unsupported runtime architecture"):
                    bootstrap.runtime_architecture()

    def test_sources_preserve_arm64_and_do_not_copy_blender_into_amd64(self):
        self.assertEqual(bootstrap.runtime_sources(architecture="aarch64"), bootstrap.RUNTIME_SOURCES)
        self.assertEqual(bootstrap.runtime_sources(cuda=True, architecture="arm64"), bootstrap.CUDA_SOURCES)
        self.assertEqual(bootstrap.runtime_sources(architecture="amd64"), bootstrap.AMD64_RUNTIME_SOURCES)
        self.assertEqual(bootstrap.runtime_sources(cuda=True, architecture="x86_64"), bootstrap.AMD64_CUDA_SOURCES)
        self.assertTrue(set(bootstrap.AMD64_RUNTIME_SOURCES + bootstrap.AMD64_CUDA_SOURCES) <= bootstrap.ALLOWED)
        self.assertFalse(any("blender" in name.lower() for name in bootstrap.AMD64_CUDA_SOURCES))

    def test_existing_pins_are_preserved_and_amd64_recipes_are_independent(self):
        root = Path(__file__).resolve().parents[1]
        arm_cpu = (root / "runtimes/Dockerfile").read_text()
        arm_cuda = (root / "runtimes/Dockerfile.cuda").read_text()
        amd_cpu = (root / "runtimes/Dockerfile.amd64").read_text()
        amd_cuda = (root / "runtimes/Dockerfile.cuda.amd64").read_text()
        self.assertIn("FROM " + fixture.installer.BASE, arm_cpu)
        self.assertIn("FROM " + cuda_fixture.cuda.BASE, arm_cuda)
        self.assertIn("FROM " + fixture.installer.AMD64_BASE, amd_cpu)
        self.assertIn("FROM " + cuda_fixture.cuda.AMD64_BASE, amd_cuda)
        self.assertNotIn("aarch64", amd_cuda)
        self.assertNotIn("COPY --from=blender", amd_cuda)
        self.assertNotIn("BLENDER_IMAGE", amd_cuda)


class CpuPortabilityTests(unittest.TestCase):
    setUp = fixture.RefreshTests.setUp
    tearDown = fixture.RefreshTests.tearDown
    registry = fixture.RefreshTests.registry
    fake_run = fixture.RefreshTests.fake_run
    setup_context = fixture.RefreshTests.setup_context

    def amd_sources(self):
        for name in bootstrap.AMD64_RUNTIME_SOURCES:
            raw = ("fixture:" + name).encode()
            self.files[name] = raw
            self.envelope["files"][name] = {"data": base64.b64encode(raw).decode(), "sha256": hashlib.sha256(raw).hexdigest()}
            path = self.root / name
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(raw)
        return bootstrap.runtime_source_digest(self.files, bootstrap.AMD64_RUNTIME_SOURCES)

    def test_bootstrap_uses_amd64_source_digest_and_checks_file_closure(self):
        digest = self.amd_sources()
        with self.setup_context(), mock.patch.object(bootstrap.platform, "machine", return_value="x86_64"):
            receipt = bootstrap.install(self.envelope)
        self.assertEqual((receipt["architecture"], receipt["runtime_source_digest"]), ("amd64", digest))
        self.assertNotEqual(digest, self.digest)
        marker = self.root / "controller.py"
        marker.write_bytes(b"keep installed controller")
        missing = {**self.envelope, "files": {k: v for k, v in self.envelope["files"].items() if k != "runtime/Dockerfile.amd64"}}
        with self.setup_context(), mock.patch.object(bootstrap.platform, "machine", return_value="x86_64"):
            with self.assertRaisesRegex(ValueError, "architecture are missing"):
                bootstrap.install(missing)
        self.assertEqual(marker.read_bytes(), b"keep installed controller")

    def test_amd64_image_is_probed_before_registration_and_has_bounded_build(self):
        digest = self.amd_sources()
        output = [json.dumps([{"Id": fixture.IMAGE, "Architecture": "amd64"}]).encode(), b"verified runtime"]
        with mock.patch.object(bootstrap.platform, "machine", return_value="x86_64"), \
                mock.patch.object(fixture.installer.subprocess, "run", side_effect=self.fake_run), \
                mock.patch.object(fixture.installer.subprocess, "check_output", side_effect=output), \
                mock.patch.object(fixture.installer, "register_runtimes") as register, mock.patch("builtins.print"):
            self.assertEqual(fixture.installer.build_runtime(self.root, digest), 0)
        build = next(a for a in self.calls if a[:2] == ["docker", "build"])
        self.assertEqual(build[build.index("--file") + 1], str(self.root / "runtime-build-inputs" / digest / "Dockerfile.amd64"))
        self.assertEqual(build[build.index("--cpu-quota") + 1], "200000")
        self.assertEqual(build[build.index("--memory") + 1], "2g")
        self.assertEqual(build[build.index("--tag") + 1], "sparkdev/cpu:1.98.1-amd64")
        register.assert_called_once()
        receipt = json.loads((self.root / "runtime-build.json").read_text())
        self.assertEqual((receipt["state"], receipt["architecture"], receipt["base"]), ("succeeded", "amd64", fixture.installer.AMD64_BASE))

    def test_cross_architecture_image_never_activates(self):
        digest = self.amd_sources()
        with mock.patch.object(bootstrap.platform, "machine", return_value="amd64"), \
                mock.patch.object(fixture.installer.subprocess, "run", side_effect=self.fake_run), \
                mock.patch.object(fixture.installer.subprocess, "check_output", return_value=json.dumps([{"Id": fixture.IMAGE, "Architecture": "arm64"}]).encode()), \
                mock.patch.object(fixture.installer, "register_runtimes") as register:
            self.assertEqual(fixture.installer.build_runtime(self.root, digest), 1)
        register.assert_not_called()
        self.assertEqual(json.loads((self.root / "runtime-build.json").read_text())["state"], "failed")

    def test_unknown_host_architecture_never_contacts_docker(self):
        with mock.patch.object(bootstrap.platform, "machine", return_value="s390x"), \
                mock.patch.object(fixture.installer.subprocess, "run") as run, \
                mock.patch.object(fixture.installer, "register_runtimes") as register:
            with self.assertRaisesRegex(ValueError, "Unsupported runtime architecture"):
                fixture.installer.build_runtime(self.root)
            with self.assertRaisesRegex(ValueError, "Unsupported runtime architecture"):
                bootstrap.runtime_current(self.root, self.digest)
        run.assert_not_called()
        register.assert_not_called()

    def test_reuse_verifies_actual_host_architecture(self):
        digest = self.amd_sources()
        self.registry(fixture.IMAGE)
        (self.root / "runtime-build.json").write_text(json.dumps({"state": "succeeded", "image": fixture.IMAGE, "source_digest": digest}))
        with mock.patch.object(bootstrap.platform, "machine", return_value="x86_64"), \
                mock.patch.object(bootstrap.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, stdout=(fixture.IMAGE + " amd64").encode())):
            self.assertTrue(bootstrap.runtime_current(self.root, digest))
        with mock.patch.object(bootstrap.platform, "machine", return_value="x86_64"), \
                mock.patch.object(bootstrap.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, stdout=(fixture.IMAGE + " arm64").encode())):
            with self.assertRaisesRegex(bootstrap.RuntimeRefreshError, "identity could not be verified"):
                bootstrap.runtime_current(self.root, digest)


class CudaPortabilityTests(unittest.TestCase):
    setUp = fixture.RefreshTests.setUp
    tearDown = fixture.RefreshTests.tearDown
    registry = fixture.RefreshTests.registry
    fake_run = fixture.RefreshTests.fake_run
    cuda_context = cuda_fixture.CudaRefreshTests.cuda_context

    def amd_sources(self):
        for name in bootstrap.AMD64_CUDA_SOURCES:
            raw = ("fixture:" + name).encode()
            self.files[name] = raw
            path = self.root / name
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(raw)
        return bootstrap.runtime_source_digest(self.files, bootstrap.AMD64_CUDA_SOURCES)

    def test_amd64_cuda_never_reuses_arm64_blender_and_reports_vram_separately(self):
        digest = self.amd_sources()
        output = [json.dumps([{"Id": fixture.IMAGE, "Architecture": "amd64"}]).encode(),
                  b'{"user_site":true,"gpu_memory_total_bytes":25757220864,"gpu_memory_free_bytes":25000000000}',
                  b'{"gpu_video":{"listed_encoders":[],"nvenc":false},"blender":{"state":"unsupported","reason":"amd64_blender_not_provisioned"}}']
        with self.cuda_context() as helpers, mock.patch.object(bootstrap.platform, "machine", return_value="x86_64"), \
                mock.patch.object(cuda_fixture.cuda.subprocess, "check_output", side_effect=output) as check, \
                mock.patch.object(cuda_fixture.cuda, "blender_image") as blender:
            self.assertEqual(cuda_fixture.cuda.build_cuda(self.root, digest), 0)
        blender.assert_not_called()
        helpers["register_runtimes"].assert_called_once()
        build = next(a for a in self.calls if a[:2] == ["docker", "build"])
        self.assertEqual(build[build.index("--tag") + 1], "sparkdev/cuda:25.09-amd64")
        self.assertEqual(Path(build[build.index("--file") + 1]).name, "Dockerfile.cuda.amd64")
        self.assertFalse(any(a.startswith("BLENDER_IMAGE=") for a in build))
        self.assertIn("torch.cuda.mem_get_info", check.call_args_list[-2].args[0][-1])
        self.assertIn("gpu_memory_total_bytes", check.call_args_list[-2].args[0][-1])
        self.assertIn("amd64_blender_not_provisioned", check.call_args_list[-1].args[0][-1])
        receipt = json.loads((self.root / "runtime-cuda-build.json").read_text())
        self.assertEqual((receipt["architecture"], receipt["base"], receipt["probe"]["blender"]["state"]), ("amd64", cuda_fixture.cuda.AMD64_BASE, "unsupported"))

    def test_amd64_cuda_rejects_arm64_image_before_any_gpu_probe(self):
        digest = self.amd_sources()
        with self.cuda_context() as helpers, mock.patch.object(bootstrap.platform, "machine", return_value="x86_64"), \
                mock.patch.object(cuda_fixture.cuda.subprocess, "check_output", return_value=json.dumps([{"Id": fixture.IMAGE, "Architecture": "arm64"}]).encode()) as check:
            self.assertEqual(cuda_fixture.cuda.build_cuda(self.root, digest), 1)
        check.assert_called_once()
        helpers["register_runtimes"].assert_not_called()

    def test_amd64_explicit_blender_request_is_refused_without_image_lookup(self):
        with mock.patch.object(bootstrap.platform, "machine", return_value="x86_64"), mock.patch.object(cuda_fixture.cuda, "inspect_image") as inspect:
            with self.assertRaisesRegex(bootstrap.RuntimeRefreshError, "not provisioned"):
                cuda_fixture.cuda.blender_image(self.root, self.root, {})
        inspect.assert_not_called()


if __name__ == "__main__":
    unittest.main()
