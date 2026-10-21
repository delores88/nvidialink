#!/usr/bin/env python3
"""Refresh the approved CUDA runtime under the same idle lock as CPU refreshes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import runpy
import sqlite3
import subprocess
import time
import uuid

from bootstrap import CUDA_SOURCES, RuntimeRefreshError, bounded_json, ensure_runtime_idle, runtime_current, runtime_lock, write_runtime_attempt

BASE = "nvcr.io/nvidia/pytorch@sha256:f3b4a33fd60d5e0358287bb44d57e91eda45e8f1681fef1bfd6c969371417130"
BLENDER_EXISTING = "sha256:69504fa0aa66263511d356be871df4ab56c8332ea629525fbb832a7249abff62"
BLENDER_FILES = ("Dockerfile.blender", "prune_deps.py", "patch_deps_cuda13.py", "patch_deps_toolchain.py",
                 "patch_deps_ispc.py", "patch_deps_oidn_cudart.py", "patch_deps_numpy.py", "10_nvidia.json")


def helpers():
    return runpy.run_path(str(Path(__file__).with_name("install-runtime.py")))


def require_probe_headroom():
    available = next(int(s.split()[1]) for s in Path("/proc/meminfo").read_text().splitlines() if s.startswith("MemAvailable:"))
    if available < 36 * 1024**2:
        raise RuntimeRefreshError("cuda_headroom_unavailable", "GPU probe requires 36 GiB available headroom")


def inspect_image(identity):
    image = json.loads(subprocess.check_output(["docker", "image", "inspect", identity], timeout=10))[0]
    if image.get("Architecture") != "arm64" or not re.fullmatch(r"sha256:[0-9a-f]{64}", image.get("Id", "")):
        raise ValueError("Unexpected image identity")
    if identity.startswith("sha256:") and image["Id"] != identity:
        raise ValueError("Pinned image identity differs")
    return image["Id"]


def blender_image(root, context, env):
    # Reading/reusing content-addressed layers never retags or edits PhaseForge.
    try:
        return inspect_image(BLENDER_EXISTING), "existing immutable owner image"
    except subprocess.CalledProcessError:
        pass
    digest = hashlib.sha256()
    for name in BLENDER_FILES:
        digest.update(name.encode() + b"\0" + (context / name).read_bytes() + b"\0")
    source_digest = digest.hexdigest()
    try:
        saved = bounded_json(root / "runtime-blender-build.json")
        if saved.get("state") == "succeeded" and saved.get("source_digest") == source_digest:
            return inspect_image(saved["image"]), "DGXBridge source build " + source_digest
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        pass
    tag = "sparkdev/blender:4.5.9-arm64"
    with (root / "runtime-blender-build.log").open("wb") as log:
        code = subprocess.run(["docker", "build", "--cpu-period", "100000", "--cpu-quota", "200000",
            "--memory", "8g", "--memory-swap", "8g", "--build-arg", "JOBS=2", "--file", str(context / "Dockerfile.blender"),
            "--tag", tag, str(context)], stdout=log, stderr=subprocess.STDOUT, env=env, check=False).returncode
    if code:
        raise RuntimeRefreshError("blender_source_build_failed", "Pinned standalone Blender source build failed")
    identity = inspect_image(tag)
    helpers()["atomic"](root / "runtime-blender-build.json", {"state": "succeeded", "source_digest": source_digest,
        "image": identity, "finished_at": time.time()})
    return identity, "DGXBridge source build " + source_digest


def build_cuda(root, expected=None, attempt_id=None):
    # Caller holds runtime-refresh.lock exclusively for the whole refresh.
    ensure_runtime_idle(root)
    cpu = helpers()
    source_digest, context = cpu["runtime_inputs"](root, expected, CUDA_SOURCES)
    if runtime_current(root, source_digest, cuda=True):
        if attempt_id:
            write_runtime_attempt(root, attempt_id, {**bounded_json(root / "runtime-cuda-build.json"), "reused": True})
        return 0
    require_probe_headroom()
    receipt = {"started_at": time.time(), "state": "building", "kind": "cuda", "base": BASE,
               "tag": "sparkdev/cuda:25.09-arm64", "source_digest": source_digest}
    publish = lambda: cpu["publish_runtime_status"](root, receipt, attempt_id, cuda=True)
    publish()
    env = {**os.environ, "DOCKER_BUILDKIT": "0"}
    try:
        blender, blender_origin = blender_image(root, context, env)
        receipt.update(blender_image=blender, blender_provenance=blender_origin)
        publish()
        with (root / "runtime-cuda-build.log").open("wb") as log:
            code = subprocess.run(["docker", "build", "--cpu-period", "100000", "--cpu-quota", "100000",
                "--memory", "1g", "--memory-swap", "1g", "--build-arg", "BLENDER_IMAGE=" + blender, "--file", str(context / "Dockerfile.cuda"),
                "--tag", receipt["tag"], str(context)], stdout=log, stderr=subprocess.STDOUT, env=env, check=False).returncode
        if code:
            receipt.update(state="failed", exit_code=code, finished_at=time.time())
            publish()
            return code
        image = json.loads(subprocess.check_output(["docker", "image", "inspect", receipt["tag"]], timeout=10))[0]
        image_id = image["Id"]
        if image.get("Architecture") != "arm64" or not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
            raise ValueError("Unexpected image identity")
        require_probe_headroom()
        # Probe the worker's exact interpreter, not the image's PATH alias.
        program = "import io,json,site,pip,torch,PIL; from PIL import Image; assert site.ENABLE_USER_SITE; assert PIL.__version__ == '13.0.0'; b=io.BytesIO(); Image.new('RGB',(2,2),(1,2,3)).save(b,format='PNG'); b.seek(0); im=Image.open(b); im.load(); assert im.resize((1,1)).getpixel((0,0)) == (1,2,3); assert torch.cuda.is_available(); x=torch.ones(4*1024*1024,device='cuda'); torch.cuda.synchronize(); print(json.dumps({'torch':torch.__version__,'pillow':PIL.__version__,'pillow_png_decode_resize':True,'cuda':torch.version.cuda,'device':torch.cuda.get_device_name(0),'bytes':x.numel()*x.element_size(),'sum':x.sum().item(),'user_site':site.ENABLE_USER_SITE}))"
        probe = subprocess.check_output(["docker", "run", "--rm", "--pull", "never", "--network", "none", "--read-only",
            "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", "1000:1000", "--cpus", "1", "--memory", "4g", "--memory-swap", "4g",
            "--pids-limit", "512", "--gpus", "all", "--env", "NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics,video", "--env", "OMP_NUM_THREADS=1",
            "--tmpfs", "/tmp:rw,nosuid,nodev,size=268435456", "--entrypoint", "/usr/bin/python3", image_id, "-c", program], timeout=120)
        probe_value = json.loads(probe)
        media = subprocess.check_output(["docker", "run", "--rm", "--pull", "never", "--network", "none", "--read-only",
            "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", "1000:1000", "--cpus", "1", "--memory", "4g", "--memory-swap", "4g",
            "--pids-limit", "512", "--gpus", "all", "--env", "NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics,video",
            "--env", "HOME=/tmp", "--env", "OMP_NUM_THREADS=1", "--tmpfs", "/tmp:rw,nosuid,nodev,size=536870912",
            "--entrypoint", "/usr/bin/python3", image_id, "/opt/sparkdev/media_probe.py", "--probe"], timeout=300)
        probe_value.update(json.loads(media))
        # A listed encoder is only a build capability, not hardware acceptance.
        # Require the promised FFmpeg codecs, while reporting actual successes separately.
        if set(probe_value["gpu_video"]["listed_encoders"]) != {"h264_nvenc", "hevc_nvenc", "av1_nvenc"}:
            raise ValueError("FFmpeg lacks the required compiled NVENC encoders")
        cpu["register_runtimes"](root, {"cuda": {"image": image_id, "gpu": True,
            "provenance": "sparkdev CUDA Dockerfile; source " + source_digest + "; base " + BASE}})
        receipt.update(state="succeeded", image=image_id, probe=probe_value, probe_sha256=hashlib.sha256(probe).hexdigest(),
                       media_probe_sha256=hashlib.sha256(media).hexdigest(), finished_at=time.time(), exit_code=0)
        publish()
        print(json.dumps(receipt))
        return 0
    except (OSError, ValueError, TypeError, KeyError, IndexError, subprocess.SubprocessError):
        receipt.update(state="failed", exit_code=1, finished_at=time.time(), error="runtime_verification_failed")
        publish()
        return 1


def refresh_locked(root, expected=None, attempt_id=None):
    try:
        return build_cuda(root, expected, attempt_id)
    except (OSError, ValueError, TypeError, KeyError, StopIteration, sqlite3.Error, subprocess.SubprocessError) as error:
        receipt = {"state": "not_started", "kind": "cuda", "source_digest": expected, "finished_at": time.time(),
                   "error": getattr(error, "code", "runtime_refresh_unavailable"), "message": "CUDA runtime refresh did not start; the existing runtime registry is preserved"}
        try:
            recorded = bounded_json(root / "runtime-cuda-build.json")
            if recorded.get("attempt_id") == attempt_id and recorded.get("state") in ("building", "succeeded", "failed"):
                receipt = {**recorded, "error": "runtime_status_publish_failed"}
        except (OSError, ValueError, TypeError, AttributeError):
            pass
        helpers()["publish_runtime_status"](root, receipt, attempt_id, cuda=True)
        print(json.dumps(receipt))
        return 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-digest")
    parser.add_argument("--attempt-id")
    args = parser.parse_args(argv)
    if args.source_digest is not None and not re.fullmatch(r"[0-9a-f]{64}", args.source_digest):
        parser.error("source digest must be a SHA-256")
    if args.attempt_id is not None and not re.fullmatch(r"[0-9a-f]{32}", args.attempt_id):
        parser.error("attempt id must be a 32-character lowercase hexadecimal ID")
    os.umask(0o077)
    os.nice(10)
    root, attempt = Path.home() / "sparkdev", args.attempt_id or uuid.uuid4().hex
    acquired = False
    try:
        with runtime_lock(root, wait=30):
            acquired = True
            return refresh_locked(root, args.source_digest, attempt)
    except (OSError, ValueError, sqlite3.Error, subprocess.SubprocessError) as error:
        receipt = {"state": "unknown" if acquired else "not_started", "kind": "cuda", "source_digest": args.source_digest, "finished_at": time.time(),
                   "error": getattr(error, "code", "runtime_refresh_unavailable")}
        # A lock timeout must not overwrite another builder's current receipt.
        try:
            write_runtime_attempt(root, attempt, receipt)
        except (OSError, ValueError):
            receipt["receipt_saved"] = False
        print(json.dumps(receipt))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
