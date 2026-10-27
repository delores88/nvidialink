"""Capability regressions use fixed observations; never inspect an owner's GPU."""
import subprocess
import tempfile
import unittest
from unittest import mock

import controller
import test_controller as fixtures


MEMORY = "MemTotal:       268435456 kB\nMemAvailable:   209715200 kB\n"


class ComputeCapabilitiesTests(unittest.TestCase):
    def observe(self, raw):
        with mock.patch.object(controller.Path, "read_text", return_value=MEMORY), \
             mock.patch.object(controller.os.path, "isfile", return_value=True), \
             mock.patch.object(controller.os, "access", return_value=True), \
             mock.patch.object(controller, "run_tool", return_value=raw) as probe:
            value = controller.compute_capabilities()
        probe.assert_called_once_with(
            ["/usr/bin/nvidia-smi", "--query-gpu=name,memory.total,memory.free", "--format=csv,noheader,nounits"], 1.5)
        return value

    def test_discrete_vram_is_separate_from_system_ram(self):
        value = self.observe(b"NVIDIA GeForce RTX 4090, 24564, 23000\n")
        self.assertEqual(value["system_memory"]["total_bytes"], 256 * 1024**3)
        self.assertEqual(value["system_memory"]["available_bytes"], 200 * 1024**3)
        self.assertEqual(value["gpu"]["memory_model"], "discrete")
        gpu = value["gpu"]["devices"][0]
        self.assertEqual(gpu["total_memory_bytes"], 24564 * 1024**2)
        self.assertEqual(gpu["free_memory_bytes"], 23000 * 1024**2)
        self.assertNotEqual(gpu["total_memory_bytes"], value["system_memory"]["total_bytes"])

    def test_unified_gpu_never_invents_framebuffer_memory(self):
        value = self.observe(b"NVIDIA GB10, [N/A], [N/A]\n")
        self.assertTrue(value["gpu"]["observed"])
        self.assertEqual(value["gpu"]["memory_model"], "unified")
        self.assertIsNone(value["gpu"]["devices"][0]["total_memory_bytes"])
        self.assertIsNone(value["gpu"]["devices"][0]["free_memory_bytes"])

    def test_malformed_or_impossible_reports_leave_gpu_unknown(self):
        for raw in [b"", b"name,1\n", b"name,1,2\n", b"name,-1,0\n",
                    b"name,1048577,0\n", b"name,NaN,0\n", b"\xff,24,12\n",
                    b'"name\x01",24,12\n', b"name,24,12\n" * 17, b"x" * 16385]:
            with self.subTest(raw=raw[:32]):
                value = self.observe(raw)
                self.assertFalse(value["gpu"]["observed"])
                self.assertEqual(value["system_memory"]["total_bytes"], 256 * 1024**3)

    def test_missing_gpu_tool_preserves_system_observation(self):
        with mock.patch.object(controller.Path, "read_text", return_value=MEMORY), \
             mock.patch.object(controller.os.path, "isfile", return_value=False), \
             mock.patch.object(controller, "run_tool") as probe:
            value = controller.compute_capabilities()
        probe.assert_not_called()
        self.assertFalse(value["gpu"]["observed"])
        self.assertEqual(value["system_memory"]["available_bytes"], 200 * 1024**3)

    def test_timed_out_probe_does_not_invent_capacity(self):
        with mock.patch.object(controller.Path, "read_text", return_value=MEMORY), \
             mock.patch.object(controller.os.path, "isfile", return_value=True), \
             mock.patch.object(controller.os, "access", return_value=True), \
             mock.patch.object(controller, "run_tool", side_effect=subprocess.TimeoutExpired("nvidia-smi", 1.5)):
            value = controller.compute_capabilities()
        self.assertFalse(value["gpu"]["observed"])

    def test_legacy_hardware_policy_shape_stays_unchanged(self):
        with mock.patch.object(controller.Path, "read_text", return_value=MEMORY), \
             mock.patch.object(controller.os, "cpu_count", return_value=64):
            self.assertEqual(controller.hardware_capacity(), {"cpu_count": 64, "memory_gb": 256})

    def test_unknown_memory_stays_unknown(self):
        with mock.patch.object(controller.Path, "read_text", side_effect=OSError), \
             mock.patch.object(controller.os.path, "isfile", return_value=False):
            value = controller.compute_capabilities()
        self.assertIsNone(value["system_memory"]["total_bytes"])
        self.assertIsNone(value["system_memory"]["available_bytes"])


class DockerReadinessTests(unittest.TestCase):
    def test_zero_exit_without_a_server_version_is_not_ready(self):
        with tempfile.TemporaryDirectory() as directory:
            instance = controller.Controller(directory, fixtures.FakeDocker(),
                memory_probe=lambda: 120, hardware_probe=lambda: {"cpu_count": 20, "memory_gb": 120},
                link_probe=fixtures.NO_LINK, disk_probe=fixtures.PLENTY_DISK)
            try:
                for version, expected in [(b"", False), (b"\n", False), (b"unknown", False),
                                          (b"26.0.0\n", True), (b"28.3.0-ce", True),
                                          (b"26.0.0\nadditional output", False)]:
                    with self.subTest(version=version), mock.patch.object(instance.docker, "call", return_value=version):
                        self.assertEqual(instance.doctor({"v": 1, "op": "doctor"})["checks"]["docker"], expected)
            finally:
                instance.db.close()


if __name__ == "__main__":
    unittest.main()
