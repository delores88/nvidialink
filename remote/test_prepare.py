"""Dependency preparation policy tests. They run on Windows and Linux: every
container path is a patched module constant and nothing needs /dev, symlink
privileges or POSIX rename-over semantics."""
import contextlib
import hashlib
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import prepare

try:
    import pip._vendor.packaging.requirements  # noqa: F401
    HAVE_PIP = True
except ImportError:
    HAVE_PIP = False

HASH = " --hash=sha256:" + "a" * 64
LINUX_SUFFIXES = (".py", ".pyc", ".cpython-312-aarch64-linux-gnu.so", ".abi3.so", ".so")


def write(root, relative, text=""):
    path = Path(root) / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return path


def dist(root, name, version, requires=()):
    lines = ["Metadata-Version: 2.1", "Name: " + name, "Version: " + version]
    write(root, name.replace("-", "_") + "-" + version + ".dist-info/METADATA",
          "\n".join(lines + ["Requires-Dist: " + r for r in requires]) + "\n")


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


@contextlib.contextmanager
def symlinked_dir(path, target):
    """A real directory symlink, or a simulated one where Windows lacks the privilege."""
    try:
        os.symlink(target, path, target_is_directory=True)
    except OSError:
        path.mkdir()
        original = prepare.linked
        with mock.patch.object(prepare, "linked", side_effect=lambda p: Path(p) == path or original(p)):
            yield
        return
    yield


class Sandbox(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.work, self.envs, self.cache_tmp, self.dev = root / "work", root / "cache" / "envs", root / "cache" / ".tmp", root / "dev"
        self.image = root / "image"
        for path in (self.work, self.dev, self.image):
            path.mkdir(parents=True)
        for name, value in (("WORK", self.work), ("ENVS", self.envs), ("TMP", self.cache_tmp), ("DEV", self.dev)):
            patcher = mock.patch.object(prepare, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        patcher = mock.patch.object(prepare, "image_path", lambda: [str(self.image)])
        patcher.start()
        self.addCleanup(patcher.stop)
        self.calls = []

    def pip(self, code=0, files=("tinylib/__init__.py",), dists=(("tinylib", "1.0", ()),), during=None):
        def run(argv, cwd=None, stdin=None, env=None, check=False):
            self.calls.append({"argv": argv, "cwd": cwd, "env": env, "tmp_dir": prepare.TMP.is_dir()})
            target = Path(argv[argv.index("--target") + 1])
            target.mkdir(parents=True, exist_ok=True)
            for name in files:
                write(target, name)
            for name, version, requires in dists:
                dist(target, name, version, requires)
            if during:
                during()
            return subprocess.CompletedProcess(argv, code)
        return mock.patch.object(prepare.subprocess, "run", side_effect=run)

    def main(self, runtime):
        output = io.StringIO()
        with mock.patch.object(sys, "argv", ["/sparkdev-prepare.py", runtime]), \
             mock.patch.object(prepare.shutil, "which", return_value=sys.executable), \
             contextlib.redirect_stdout(output):
            code = prepare.main()
        return code, output.getvalue()

    def leftovers(self):
        return sorted(p.name for p in self.envs.iterdir() if p.name.startswith((".pending-", ".old-")))


class CommandTests(Sandbox):
    def test_pip_argv_per_runtime_is_isolated_and_targets_pending(self):
        python = write(self.work, "requirements.lock", "six==1.17.0" + HASH + "\n")
        cuda = write(self.work, "gpu/requirements-cuda.lock", "tomli-w==1.2.0" + HASH + "\n")
        (_, plain, _), = prepare.commands("python", self.work)
        self.assertEqual(plain[:6], ["python3", "-I", "-X", "utf8", "-m", "pip"])
        self.assertNotIn("--no-deps", plain)
        self.assertEqual(plain[plain.index("--target") + 1], str(self.envs / (".pending-" + digest(python))))
        self.assertEqual(plain[plain.index("-r") + 1], str(python))
        with mock.patch.object(prepare.shutil, "which", return_value=sys.executable):
            (cwd, gpu, lock), = prepare.commands("cuda", self.work)
        self.assertEqual((cwd, lock), (cuda.parent, cuda))
        self.assertEqual(gpu[:6], [sys.executable, "-I", "-X", "utf8", "-m", "pip"])
        for flag in ("--no-deps", "--require-hashes", "--only-binary=:all:"):
            self.assertIn(flag, gpu)
        self.assertEqual(gpu[gpu.index("--target") + 1], str(self.envs / (".pending-" + digest(cuda))))

    def test_cuda_interpreter_mismatch_raises(self):
        write(self.work, "requirements-cuda.lock", "tomli-w==1.2.0" + HASH + "\n")
        for found in (None, str(Path(self.tmp.name) / "other" / "python3")):
            with mock.patch.object(prepare.shutil, "which", return_value=found):
                with self.assertRaises(ValueError):
                    prepare.commands("cuda", self.work)

    def test_pip_env_strips_pip_and_python_variables_and_keeps_proxies(self):
        inherited = {"PIP_CONSTRAINT": "/work/c.txt", "PIP_INDEX_URL": "https://mirror.invalid/simple",
                     "PIP_EXTRA_INDEX_URL": "https://extra.invalid/simple", "PIP_NO_INDEX": "0", "PIP_TARGET": "/elsewhere",
                     "PYTHONPATH": "/work", "PYTHONSTARTUP": "/work/start.py", "PYTHONUSERBASE": "/work",
                     "HTTPS_PROXY": "http://sparkdev-proxy:3128", "https_proxy": "http://sparkdev-proxy:3128", "NO_PROXY": ""}
        with mock.patch.dict(os.environ, inherited):
            env = prepare.pip_env()
        self.assertFalse({k for k in env if k.startswith("PYTHON")})
        self.assertEqual({k for k in env if k.startswith("PIP_")},
                         {"PIP_CONFIG_FILE", "PIP_CACHE_DIR", "PIP_INDEX_URL", "PIP_DISABLE_PIP_VERSION_CHECK", "PIP_NO_INPUT"})
        self.assertEqual((env["PIP_CONFIG_FILE"], env["PIP_INDEX_URL"], env["PIP_CACHE_DIR"]), (os.devnull, "https://pypi.org/simple", "/pip-cache"))
        self.assertEqual(env["TMPDIR"], str(self.cache_tmp))
        folded = {k.upper(): v for k, v in env.items()}  # Windows environment keys are case-insensitive.
        self.assertEqual((folded["HTTPS_PROXY"], folded["NO_PROXY"]), ("http://sparkdev-proxy:3128", ""))
        if os.name == "posix":
            self.assertEqual(env["https_proxy"], "http://sparkdev-proxy:3128")

    def test_python_missing_lock_keeps_generic_message(self):
        code, output = self.main("python")
        self.assertEqual((code, output.strip()), (65, prepare.GENERIC))


class PrecheckTests(unittest.TestCase):
    def test_crlf_continuations_and_comments_parse(self):
        raw = ("# generated\r\nsix==1.17.0 \\\r\n    --hash=sha256:" + "a" * 64 + " \\\r\n    --hash=sha256:" + "b" * 64 +
               "  # trailing\r\n\r\n   # indented comment\r\nattrs[tests] == 25.1.0 ; python_version >= \"3.8\" \\\r\n"
               "    --hash sha256:" + "c" * 64 + "\r\nnvidia-ml-py==13.580.82" + HASH + "\r\n").encode()
        self.assertEqual(prepare.precheck(raw, set()), [])
        self.assertEqual([(n, prepare.lock_name(line)) for n, line in prepare.logical_lines(raw.decode())],
                         [(2, "six"), (7, "attrs"), (9, "nvidia-ml-py")])

    def test_cuda_stack_names_are_reserved(self):
        raw = "\n".join(n + "==1.0" + HASH for n in ("torch", "Torch_Vision", "nvidia-cublas-cu12", "TorchAudio",
                                                  "pytorch_triton", "nvidia-cudnn-cu13", "nvidia-ml-py", "nvidia-cublas",
                                                  "nvidia_cuda_runtime", "Nvidia.NvJitLink", "nvidia-cufft", "cuda-toolkit",
                                                  "NVIDIA_ML_PY", "cuda-python")).encode()
        self.assertEqual(prepare.precheck(raw, set()), [
            "sparkdev: refused: torch is reserved for the CUDA image",
            "sparkdev: refused: torch-vision is reserved for the CUDA image",
            "sparkdev: refused: nvidia-cublas-cu12 is reserved for the CUDA image",
            "sparkdev: refused: torchaudio is reserved for the CUDA image",
            "sparkdev: refused: pytorch-triton is reserved for the CUDA image",
            "sparkdev: refused: nvidia-cudnn-cu13 is reserved for the CUDA image",
            # CUDA 13 wheels have no -cuNN suffix.
            "sparkdev: refused: nvidia-cublas is reserved for the CUDA image",
            "sparkdev: refused: nvidia-cuda-runtime is reserved for the CUDA image",
            "sparkdev: refused: nvidia-nvjitlink is reserved for the CUDA image",
            "sparkdev: refused: nvidia-cufft is reserved for the CUDA image",
            "sparkdev: refused: cuda-toolkit is reserved for the CUDA image"])
        self.assertFalse(prepare.reserved("nvidia-ml-py") or prepare.reserved("cuda-python"))

    def test_options_urls_and_image_names_are_refused(self):
        lines = ["-r other.txt", "--index-url https://mirror.invalid/simple", "--extra-index-url https://x.invalid",
                 "-e .", "-c constraints.txt", "--find-links /work/wheels", "foo @ https://x.invalid/foo-1.0-py3-none-any.whl",
                 "foo@https://x.invalid/foo.whl", "https://files.pythonhosted.org/foo-1.0-py3-none-any.whl" + HASH,
                 "./wheels/foo-1.0-py3-none-any.whl", "foo-1.0-py3-none-any.whl" + HASH, "foo==1.0 --config-settings=x=y" + HASH,
                 "foo==1.0 --hash=md5:" + "a" * 32, "NumPy==2.1.0" + HASH, "Pillow==11.0.0" + HASH]
        self.assertEqual(prepare.precheck("\n".join(lines).encode(), {"numpy", "pillow"}),
                         ["sparkdev: refused: unsupported lock line " + str(n) for n in range(1, 14)] +
                         ["sparkdev: refused: numpy is provided by the CUDA image",
                          "sparkdev: refused: pillow is provided by the CUDA image"])

    def test_local_archive_and_path_requirements_are_refused(self):
        # pip resolves each of these against the lock's directory and builds a committed sdist.
        lines = ["evil==1.0.tar.gz", "x>=0.tar.gz", "a[b]==1.tar.gz", "EVIL==1.0.TAR.GZ", "evil == 1.0.tar.gz",
                 "x.whl[extra]", "x==1.whl[y]", "x==1.zip [y] [z]", 'evil.tar.gz;python_version>"0"', "x==1/../sub",
                 "x==1\\sub", "evil-1.0.tar.gz", "x<2.tgz ; python_version >= \"3\""]
        raw = "\n".join(line + HASH for line in lines).encode()
        self.assertEqual(prepare.precheck(raw, set()), ["sparkdev: refused: unsupported lock line " + str(n) for n in range(1, len(lines) + 1)])
        self.assertEqual(prepare.precheck(raw, set(), "requirements.lock", cuda=False), prepare.precheck(raw, set()))
        accepted = ["zipp==3.21.0", "backports.tarfile==1.2.0", 'attrs[tests] == 25.1.0 ; python_version >= "3.8"', "wheel==0.45.1"]
        self.assertEqual([prepare.lock_name(line + HASH) for line in accepted], ["zipp", "backports-tarfile", "attrs", "wheel"])

    @unittest.skipUnless(HAVE_PIP, "pip is not importable")
    def test_pip_reads_a_versioned_archive_name_as_a_local_file(self):
        try:
            from pip._internal.req.constructors import install_req_from_line
        except ImportError:
            self.skipTest("pip internals moved")
        name = "evil==1.0.tar.gz"
        with tempfile.TemporaryDirectory() as folder, contextlib.chdir(folder):
            Path(name).write_bytes(b"")
            self.assertIsNotNone(install_req_from_line(name).link)  # The bypass this precheck closes.
        self.assertIsNone(prepare.lock_name(name + HASH))

    def test_coding_cookies_other_than_utf8_are_refused(self):
        hidden = b"# note +AAo-torch==2.9.0 --hash=sha256:" + b"a" * 64 + b"\n"
        for raw in (b"# -*- coding: utf-7 -*-\n" + hidden, b"# generated\n# vim: set fileencoding=utf-7 :\n" + hidden,
                    b"# coding=latin-1\nsix==1.17.0" + HASH.encode() + b"\n", b"# coding: no-such-codec\n"):
            self.assertEqual(prepare.precheck(raw, set()), ["sparkdev: refused: requirements-cuda.lock declares a coding other than UTF-8"])
        self.assertEqual(prepare.precheck(b"# coding: utf-7\n", set(), "requirements.lock", cuda=False),
                         ["sparkdev: refused: requirements.lock declares a coding other than UTF-8"])
        # A UTF-8 cookie, or one past the first two lines, changes nothing pip reads.
        for raw in (b"# -*- coding: utf-8 -*-\nsix==1.17.0" + HASH.encode() + b"\n",
                    b"\xef\xbb\xbf# coding: UTF8\nsix==1.17.0" + HASH.encode() + b"\n",
                    b"# one\n# two\n# coding: utf-7\nsix==1.17.0" + HASH.encode() + b"\n"):
            self.assertEqual(prepare.precheck(raw, set()), [], raw)
        try:
            from pip._internal.utils.encoding import auto_decode
        except ImportError:
            return
        self.assertIn("\ntorch==2.9.0", auto_decode(b"# -*- coding: utf-7 -*-\n" + hidden))  # What pip would read.

    def test_image_path_drops_only_the_script_directory(self):
        paths = ["/", "/usr/lib/python312.zip", "/usr/local/lib/python3.12/dist-packages"]
        with mock.patch.object(sys, "path", paths):
            with mock.patch.object(sys, "flags", mock.Mock(safe_path=False)):
                self.assertEqual(prepare.image_path(), paths[1:])
            with mock.patch.object(sys, "flags", mock.Mock(safe_path=True)):
                self.assertEqual(prepare.image_path(), paths)

    def test_non_utf8_lock_is_refused(self):
        self.assertEqual(prepare.precheck(b"\xff\xfe\x00", set()), ["sparkdev: refused: requirements-cuda.lock is not UTF-8"])


class OverlapTests(Sandbox):
    def setUp(self):
        super().setUp()
        self.staging = Path(self.tmp.name) / "staging"
        self.staging.mkdir()

    def overlap(self):
        return prepare.overlap(self.staging, [str(self.image)])

    def test_refuses_image_distribution_and_module(self):
        dist(self.image, "numpy", "2.1.0")
        write(self.image, "numpy/__init__.py")
        dist(self.staging, "NumPy", "2.2.0")
        write(self.staging, "numpy/__init__.py")
        self.assertEqual(self.overlap(), ["sparkdev: refused: numpy is provided by the CUDA image",
                                          "sparkdev: refused: numpy shadows a module the CUDA image provides"])

    def test_refuses_stdlib_shadow_and_startup_hooks(self):
        for name in ("json.py", "sitecustomize.py", "usercustomize/__init__.py"):
            write(self.staging, name)
        self.assertEqual(self.overlap(), ["sparkdev: refused: json shadows the Python standard library",
                                          "sparkdev: refused: sitecustomize would run at every CUDA interpreter startup",
                                          "sparkdev: refused: usercustomize would run at every CUDA interpreter startup"])

    def test_refuses_module_from_a_differently_named_distribution(self):
        dist(self.image, "opencv", "4.10.0")
        write(self.image, "cv2/__init__.py")
        dist(self.staging, "opencv-python-headless", "4.11.0.86")
        write(self.staging, "cv2/__init__.py")
        self.assertEqual(self.overlap(), ["sparkdev: refused: cv2 shadows a module the CUDA image provides"])

    def test_refuses_extension_module_stems(self):
        write(self.image, "_speedups.py")
        write(self.image, "fastmod/__init__.py")
        write(self.staging, "_speedups.cpython-312-aarch64-linux-gnu.so")
        write(self.staging, "fastmod.abi3.so")
        write(self.staging, "newmod.cpython-312-aarch64-linux-gnu.so")
        with mock.patch.object(prepare, "SUFFIXES", LINUX_SUFFIXES):
            self.assertEqual(self.overlap(), ["sparkdev: refused: _speedups shadows a module the CUDA image provides",
                                              "sparkdev: refused: fastmod shadows a module the CUDA image provides"])

    def test_namespace_portions_merge_but_child_collisions_are_refused(self):
        write(self.image, "google/protobuf/__init__.py")
        dist(self.staging, "google-auth", "2.38.0")
        write(self.staging, "google/auth/__init__.py")
        write(self.staging, "google/auth/transport/__init__.py")
        self.assertEqual(self.overlap(), [])
        write(self.staging, "google/protobuf/__init__.py")
        self.assertEqual(self.overlap(), ["sparkdev: refused: google.protobuf shadows a module the CUDA image provides"])

    def test_regular_package_over_image_namespace_is_refused(self):
        write(self.image, "google/protobuf/__init__.py")
        write(self.staging, "google/__init__.py")
        self.assertEqual(self.overlap(), ["sparkdev: refused: google shadows a module the CUDA image provides"])

    def test_metadata_scripts_and_non_modules_are_ignored(self):
        write(self.image, "tool.py")
        dist(self.staging, "tinylib", "1.0")
        for name in ("tinylib/__init__.py", "bin/tool", "bin/json.py", "__pycache__/json.cpython-312.pyc", "extra.pth",
                     "README.md", "not-a-module/__init__.py", "tinylib-1.0.data/scripts/tool.py", "tinylib.pyi"):
            write(self.staging, name)
        self.assertEqual(self.overlap(), [])

    def test_reserved_staged_distribution_is_refused(self):
        dist(self.staging, "pytorch-triton", "3.0.0")
        self.assertEqual(self.overlap(), ["sparkdev: refused: pytorch-triton is reserved for the CUDA image"])

    def test_staged_nvidia_namespace_is_refused_whatever_ships_it(self):
        dist(self.staging, "nvidia-cublas", "13.0.0.19")
        dist(self.staging, "repackaged-blas", "1.0")
        write(self.staging, "nvidia/cu13/lib/libcublas.so.13")
        dist(self.staging, "nvidia-ml-py", "13.580.82")
        write(self.staging, "pynvml.py")
        self.assertEqual(self.overlap(), ["sparkdev: refused: nvidia-cublas is reserved for the CUDA image",
                                          "sparkdev: refused: nvidia is reserved for the CUDA image"])


@unittest.skipUnless(HAVE_PIP, "pip's vendored packaging is not importable")
class UnmetTests(Sandbox):
    def setUp(self):
        super().setUp()
        self.staging = Path(self.tmp.name) / "staging"
        self.staging.mkdir()
        dist(self.image, "torch", "2.9.0a0+nv25.09")
        dist(self.image, "numpy", "2.1.0")

    def unmet(self):
        return prepare.unmet(self.staging, [str(self.image)])

    def test_prerelease_local_image_version_satisfies(self):
        dist(self.staging, "lib-a", "1.0", ["torch>=2.0", "numpy>=1.26"])
        self.assertEqual(self.unmet(), ([], []))

    def test_missing_dependency_is_refused(self):
        dist(self.staging, "lib-b", "1.0", ["absent-pkg>=1"])
        self.assertEqual(self.unmet(), (["sparkdev: refused: lib-b requires absent-pkg>=1, missing from requirements-cuda.lock"], []))

    def test_staged_version_conflict_is_refused_and_match_passes(self):
        dist(self.staging, "lib-c", "1.0", ["helper>=2"])
        dist(self.staging, "helper", "1.0")
        self.assertEqual(self.unmet(), (["sparkdev: refused: lib-c requires helper>=2; requirements-cuda.lock has 1.0"], []))
        (self.staging / "helper-1.0.dist-info" / "METADATA").write_text("Metadata-Version: 2.1\nName: helper\nVersion: 2.1\n")
        self.assertEqual(self.unmet(), ([], []))

    def test_image_version_conflict_only_warns(self):
        dist(self.staging, "lib-d", "1.0", ["numpy<2"])
        self.assertEqual(self.unmet(), ([], ["sparkdev: warning: lib-d requires numpy<2; CUDA image has 2.1.0"]))

    def test_false_markers_and_extras_are_ignored(self):
        dist(self.staging, "lib-e", "1.0", ['absent-pkg; python_version < "3"', 'pytest; extra == "test"'])
        self.assertEqual(self.unmet(), ([], []))

    def test_unevaluable_marker_is_a_fixed_refusal(self):
        import platform
        dist(self.staging, "lib-f", "1.0", ['foo; platform_release >= "5"', 'bar; python_version ~= "abc"'])
        with mock.patch.object(platform, "release", return_value="6.8.0-1015-nvidia"):
            self.assertEqual(self.unmet(), (["sparkdev: refused: lib-f has an unevaluable requirement marker"] * 2, []))


class PublishTests(Sandbox):
    def lock(self, runtime, text="tinylib==1.0" + HASH + "\n"):
        self.lockfile = write(self.work, prepare.LOCKS[runtime], text)
        self.final = self.envs / digest(self.lockfile)
        return self.lockfile

    def old_final(self):
        write(self.final, "old.py", "old")

    def test_pip_failure_deletes_pending_and_returns_its_code(self):
        self.lock("python")
        self.old_final()
        with self.pip(code=3):
            code, _ = self.main("python")
        self.assertEqual(code, 3)
        self.assertEqual(self.leftovers(), [])
        self.assertEqual(sorted(p.name for p in self.final.iterdir()), ["old.py"])
        self.assertFalse(self.cache_tmp.exists())

    def test_success_replaces_final_and_passes_fixed_environment(self):
        self.lock("python")
        self.old_final()
        with mock.patch.dict(os.environ, {"PIP_CONSTRAINT": "/work/c.txt", "PYTHONPATH": "/work"}), self.pip():
            code, output = self.main("python")
        self.assertEqual(code, 0)
        self.assertIn("sparkdev: preparing requirements.lock", output)
        self.assertFalse((self.final / "old.py").exists())
        self.assertTrue((self.final / "tinylib" / "__init__.py").is_file())
        self.assertEqual(self.leftovers(), [])
        self.assertFalse(self.cache_tmp.exists())
        call, = self.calls
        self.assertEqual(call["argv"][:2], ["python3", "-I"])
        self.assertEqual(call["env"]["TMPDIR"], str(self.cache_tmp))
        self.assertTrue(call["tmp_dir"])
        self.assertNotIn("PIP_CONSTRAINT", call["env"])
        self.assertNotIn("PYTHONPATH", call["env"])

    def test_cuda_refusal_keeps_old_final(self):
        self.lock("cuda")
        self.old_final()
        with self.pip(files=("sitecustomize.py",)):
            code, output = self.main("cuda")
        self.assertEqual(code, 65)
        self.assertIn("sparkdev: refused: sitecustomize would run at every CUDA interpreter startup", output)
        self.assertEqual(sorted(p.name for p in self.final.iterdir()), ["old.py"])
        self.assertEqual(self.leftovers(), [])
        self.assertFalse(self.cache_tmp.exists())

    @unittest.skipUnless(HAVE_PIP, "pip's vendored packaging is not importable")
    def test_cuda_success_publishes_and_warns_on_image_conflict(self):
        dist(self.image, "numpy", "2.1.0")
        self.lock("cuda")
        with self.pip(dists=(("tinylib", "1.0", ("numpy<2",)),)):
            code, output = self.main("cuda")
        self.assertEqual(code, 0, output)
        self.assertIn("sparkdev: warning: tinylib requires numpy<2; CUDA image has 2.1.0", output)
        self.assertTrue((self.final / "tinylib" / "__init__.py").is_file())
        call, = self.calls
        self.assertEqual(call["argv"][0], sys.executable)
        self.assertIn("--no-deps", call["argv"])

    @unittest.skipUnless(HAVE_PIP, "pip's vendored packaging is not importable")
    def test_cuda_missing_dependency_is_fatal(self):
        self.lock("cuda")
        with self.pip(dists=(("tinylib", "1.0", ("absent-pkg",)),)):
            code, output = self.main("cuda")
        self.assertEqual(code, 65)
        self.assertIn("sparkdev: refused: tinylib requires absent-pkg, missing from requirements-cuda.lock", output)
        self.assertFalse(self.final.exists())
        self.assertEqual(self.leftovers(), [])

    @unittest.skipUnless(HAVE_PIP, "pip's vendored packaging is not importable")
    def test_cuda_unevaluable_marker_exits_65_without_publishing(self):
        self.lock("cuda")
        with self.pip(dists=(("tinylib", "1.0", ('bar; python_version ~= "abc"',)),)):
            code, output = self.main("cuda")
        self.assertEqual(code, 65)
        self.assertIn("sparkdev: refused: tinylib has an unevaluable requirement marker", output)
        self.assertFalse(self.final.exists())
        self.assertEqual(self.leftovers(), [])

    def test_symlinked_final_is_refused(self):
        self.lock("python")
        elsewhere = write(Path(self.tmp.name) / "elsewhere", "keep.py").parent
        self.envs.mkdir(parents=True)
        with symlinked_dir(self.final, elsewhere), self.pip():
            write(self.final, "keep.py")  # Through a real link this lands in elsewhere.
            code, output = self.main("python")
        self.assertEqual(code, 65)
        self.assertIn("sparkdev: refused: prepared environment path is a symlink", output)
        self.assertTrue((self.final / "keep.py").is_file())
        self.assertFalse((self.final / "tinylib").exists())
        self.assertEqual(self.leftovers(), [])

    def test_symlinked_env_root_is_refused_before_pip(self):
        self.lock("python")
        self.envs.parent.mkdir(parents=True)
        with symlinked_dir(self.envs, write(Path(self.tmp.name) / "elsewhere", "keep.py").parent), self.pip():
            code, output = self.main("python")
        self.assertEqual((code, self.calls), (65, []))
        self.assertIn("sparkdev: refused: prepared environment root is a symlink", output)

    def test_stale_pending_old_and_tmp_are_removed_at_start(self):
        self.lock("python")
        for name in (".pending-" + "0" * 64 + "/a.py", ".old-" + "1" * 64 + "/b.py", "2" * 64 + "/kept.py"):
            write(self.envs, name)
        write(self.cache_tmp, "junk/file")
        with self.pip(code=1):
            self.main("python")
        self.assertEqual(sorted(p.name for p in self.envs.iterdir()), ["2" * 64])
        self.assertFalse(self.cache_tmp.exists())

    def test_lockfile_change_during_install_is_refused(self):
        lock = self.lock("python")
        with self.pip(during=lambda: lock.write_text("changed==1.0" + HASH + "\n")):
            code, output = self.main("python")
        self.assertEqual(code, 65)
        self.assertIn("sparkdev: lockfile changed; preparation refused", output)
        self.assertEqual(self.leftovers(), [])
        self.assertFalse(self.final.exists())

    def test_gpu_device_is_refused_before_anything(self):
        self.lock("cuda")
        write(self.dev, "nvidia0")
        with self.pip():
            code, output = self.main("cuda")
        self.assertEqual((code, output.strip(), self.calls), (65, "sparkdev: refused: GPU device visible during preparation", []))
        self.assertFalse(self.envs.exists())

    def test_cuda_requires_its_own_lock(self):
        self.lock("python")
        with self.pip():
            code, output = self.main("cuda")
        self.assertEqual((code, output.strip(), self.calls), (65, prepare.CUDA_LOCK, []))

    def test_python_lock_lines_are_checked_before_pip(self):
        for text in ("evil-1.0.tar.gz" + HASH + "\n", "evil==1.0.tar.gz" + HASH + "\n", "-e .\n",
                     "--no-binary :all:\nsix==1.17.0" + HASH + "\n", "./vendor/six-1.17.0.tar.gz" + HASH + "\n",
                     "six @ https://files.pythonhosted.org/six-1.17.0.tar.gz" + HASH + "\n"):
            self.calls.clear()
            self.lock("python", text)
            with self.pip():
                code, output = self.main("python")
            self.assertEqual((code, self.calls), (65, []), text)
            self.assertIn("sparkdev: refused: unsupported lock line 1", output)
        self.lockfile.write_bytes(b"\xff\xfe\x00")
        with self.pip():
            code, output = self.main("python")
        self.assertEqual((code, self.calls), (65, []))
        self.assertIn("sparkdev: refused: requirements.lock is not UTF-8", output)
        # Package-name rules stay CUDA-only: the Python runtime may install CPU torch from PyPI.
        self.lock("python", "torch==2.5.1" + HASH + "\n")
        with self.pip():
            code, output = self.main("python")
        self.assertEqual((code, len(self.calls)), (0, 1), output)

    def test_cuda_precheck_refuses_before_download(self):
        dist(self.image, "numpy", "2.1.0")
        self.lock("cuda", "torch==2.9.0" + HASH + "\nnumpy==2.1.0" + HASH + "\n")
        with self.pip():
            code, output = self.main("cuda")
        self.assertEqual((code, self.calls), (65, []))
        self.assertIn("sparkdev: refused: torch is reserved for the CUDA image", output)
        self.assertIn("sparkdev: refused: numpy is provided by the CUDA image", output)


if __name__ == "__main__":
    unittest.main()
