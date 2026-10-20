import hashlib
import os
from pathlib import Path
import tempfile
import unittest

import prepare
import worker


class EnvPathTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.work, self.envs = Path(self.tmp.name) / "work", Path(self.tmp.name) / "envs"
        self.work.mkdir()
        self.envs.mkdir()

    def lock(self, relative, text, prepared=True):
        path = self.work / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(text)
        env = self.envs / hashlib.sha256(text).hexdigest()
        if prepared:
            env.mkdir()
        return str(env)

    def test_locks_match_preparation(self):
        self.assertEqual(worker.LOCKS, prepare.LOCKS)

    def test_each_pip_runtime_loads_only_its_own_prepared_lock(self):
        python = self.lock("requirements.lock", b"six==1.17.0\r\n")
        cuda = self.lock("gpu/requirements-cuda.lock", b"tomli-w==1.2.0\n")
        self.assertEqual(worker.env_paths("python", self.work, self.envs), [python])
        self.assertEqual(worker.env_paths("cuda", self.work, self.envs), [cuda])
        for runtime in ("rust", "node", None):
            self.assertEqual(worker.env_paths(runtime, self.work, self.envs), [])

    def test_unprepared_and_excluded_locks_are_skipped(self):
        self.lock("requirements-cuda.lock", b"a==1\n", prepared=False)
        for skipped in ("node_modules", ".venv", "build", "dist", "target", ".git"):
            self.lock(skipped + "/requirements-cuda.lock", skipped.encode())
        self.assertEqual(worker.env_paths("cuda", self.work, self.envs), [])
        nested = self.lock("a/b/requirements-cuda.lock", b"b==1\n")
        self.assertEqual(worker.env_paths("cuda", self.work, self.envs), [nested])

    def test_prepared_envs_precede_inherited_pythonpath(self):
        self.assertEqual(worker.python_path(["/pip-cache/envs/a", "/pip-cache/envs/b"], "/opt/ngc/python"),
                         os.pathsep.join(["/pip-cache/envs/a", "/pip-cache/envs/b", "/opt/ngc/python"]))
        self.assertEqual(worker.python_path(["/pip-cache/envs/a"], None), "/pip-cache/envs/a")
        self.assertEqual(worker.python_path(["/pip-cache/envs/a"], ""), "/pip-cache/envs/a")


if __name__ == "__main__":
    unittest.main()
