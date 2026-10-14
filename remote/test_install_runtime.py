import runpy
from pathlib import Path
import unittest

merge_registry = runpy.run_path(str(Path(__file__).with_name("install-runtime.py")))["merge_registry"]


class RuntimeRegistryTests(unittest.TestCase):
    def test_cpu_reinstall_preserves_cuda_and_cuda_preserves_cpu(self):
        cpu = {"image": "sha256:" + "a" * 64, "provenance": "test CPU"}
        gpu = {"image": "sha256:" + "b" * 64, "provenance": "test CUDA", "gpu": True}
        first = merge_registry({"v": 1, "runtimes": {}}, {"cuda": gpu})
        second = merge_registry(first, {"rust": cpu, "node": cpu, "python": cpu})
        self.assertEqual(second["runtimes"]["cuda"], gpu)
        self.assertEqual(first["runtimes"], {"cuda": gpu})
        third = merge_registry(second, {"cuda": {**gpu, "image": "sha256:" + "c" * 64}})
        self.assertEqual(third["runtimes"]["rust"], cpu)

    def test_unpinned_unknown_or_wrong_gpu_policy_is_refused(self):
        for name, item in (("rust", {"image": "rust:latest", "provenance": "test"}),
                           ("arbitrary", {"image": "sha256:" + "a" * 64, "provenance": "test"}),
                           ("cuda", {"image": "sha256:" + "a" * 64, "provenance": "test"})):
            with self.assertRaises(ValueError):
                merge_registry({"v": 1, "runtimes": {}}, {name: item})


if __name__ == "__main__":
    unittest.main()
