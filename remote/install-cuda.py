#!/usr/bin/env python3
"""Provision only the approved optional CUDA image and run a bounded device probe."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

BASE = "nvcr.io/nvidia/pytorch@sha256:f3b4a33fd60d5e0358287bb44d57e91eda45e8f1681fef1bfd6c969371417130"


def atomic(path, value):
    temporary = path.with_suffix(".pending")
    with temporary.open("w") as stream:
        json.dump(value, stream, sort_keys=True)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.chmod(0o600)
    temporary.replace(path)


def main():
    os.umask(0o077)
    root = Path.home() / "sparkdev"
    available = next(int(s.split()[1]) for s in Path("/proc/meminfo").read_text().splitlines() if s.startswith("MemAvailable:"))
    if available < 36 * 1024**2:
        raise RuntimeError("GPU probe requires 36 GiB available headroom")
    receipt = {"started_at": time.time(), "state": "building", "base": BASE, "tag": "sparkdev/cuda:25.09-arm64"}
    status = root / "runtime-cuda-build.json"
    atomic(status, receipt)
    env = os.environ.copy()
    env["DOCKER_BUILDKIT"] = "0"
    os.nice(10)
    with (root / "runtime-cuda-build.log").open("wb") as log:
        code = subprocess.run(["docker", "build", "--cpu-period", "100000", "--cpu-quota", "100000", "--memory", "1g", "--memory-swap", "1g",
                               "--network", "none", "--file", str(root / "runtime/Dockerfile.cuda"), "--tag", receipt["tag"], str(root / "runtime")],
                              stdout=log, stderr=subprocess.STDOUT, env=env, check=False).returncode
    if code:
        receipt.update(state="failed", exit_code=code, finished_at=time.time())
        atomic(status, receipt)
        return code
    image = json.loads(subprocess.check_output(["docker", "image", "inspect", receipt["tag"]]))[0]
    image_id = image["Id"]
    if image.get("Architecture") != "arm64" or not image_id.startswith("sha256:") or len(image_id) != 71:
        raise ValueError("Unexpected image identity")
    program = "import json,torch; assert torch.cuda.is_available(); x=torch.ones(4*1024*1024,device='cuda'); torch.cuda.synchronize(); print(json.dumps({'torch':torch.__version__,'cuda':torch.version.cuda,'device':torch.cuda.get_device_name(0),'bytes':x.numel()*x.element_size(),'sum':x.sum().item()}))"
    probe = subprocess.check_output(["docker", "run", "--rm", "--pull", "never", "--network", "none", "--read-only", "--cap-drop", "ALL",
                                     "--security-opt", "no-new-privileges", "--user", "1000:1000", "--cpus", "1", "--memory", "4g", "--memory-swap", "4g",
                                     "--pids-limit", "512", "--gpus", "all", "--env", "NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics",
                                     "--env", "OMP_NUM_THREADS=1", "--tmpfs", "/tmp:rw,nosuid,nodev,size=268435456",
                                     "--entrypoint", "python", image_id, "-c", program], timeout=120)
    import runpy
    register = runpy.run_path(str(root / "install-runtime.py"))["register_runtimes"]
    register(root, {"cuda": {"image": image_id, "gpu": True, "provenance": "sparkdev CUDA Dockerfile; base " + BASE}})
    receipt.update(state="succeeded", image=image_id, probe=json.loads(probe), probe_sha256=hashlib.sha256(probe).hexdigest(), finished_at=time.time(), exit_code=0)
    atomic(status, receipt)
    print(json.dumps(receipt))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
