"""Real CLI/runtime acceptance, using new private fixtures and the caller's setup.

Runs actual Codex-attributed Spark jobs, not a simulated Claude Code session.
No PhaseForge files, services, containers or images are written.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import tomllib
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cli", required=True)
    parser.add_argument("--receipt", required=True, type=Path)
    parser.add_argument("--workspace-lock", type=Path, default=Path("Cargo.lock"))
    args = parser.parse_args()
    run_id = uuid.uuid4().hex
    root = Path(os.environ.get("LOCALAPPDATA", tempfile.gettempdir())) / "SparkDevBridge/remote-validation" / run_id
    root.mkdir(parents=True)
    rust, gpu = root / "rust", root / "gpu"
    (rust / "src").mkdir(parents=True)
    gpu.mkdir()
    itoa = next(p for p in tomllib.loads(args.workspace_lock.read_text())["package"] if p["name"] == "itoa")
    (rust / "Cargo.toml").write_text('[package]\nname="sparkdev-runtime-probe"\nversion="0.1.0"\nedition="2024"\n[dependencies]\nitoa="=' + itoa["version"] + '"\n')
    (rust / "Cargo.lock").write_text('version=4\n[[package]]\nname="sparkdev-runtime-probe"\nversion="0.1.0"\ndependencies=["itoa"]\n[[package]]\nname="itoa"\nversion="' + itoa["version"] + '"\nsource="' + itoa["source"] + '"\nchecksum="' + itoa["checksum"] + '"\n')
    (rust / "src/main.rs").write_text('fn main(){let mut b=itoa::Buffer::new();println!("{}",b.format(42));}\n')
    (gpu / "probe.py").write_text("import json,torch\nx=torch.ones(1024*1024,device='cuda');torch.cuda.synchronize()\nprint(json.dumps({'device':torch.cuda.get_device_name(0),'bytes':x.numel()*x.element_size(),'sum':x.sum().item()}))\n")
    receipt = {"run_id": run_id, "kind": "physical_spark_runtime_acceptance", "actual_caller": "codex", "checks": [], "started_at": time.time()}

    def save():
        args.receipt.parent.mkdir(parents=True, exist_ok=True)
        args.receipt.write_text(json.dumps(receipt, indent=2))

    def cli(*values):
        result = subprocess.run([args.cli, "--json", "--client", "codex", *map(str, values)], capture_output=True, timeout=90)
        if result.returncode:
            raise RuntimeError("CLI failed: " + result.stdout.decode("utf-8", "replace") + result.stderr.decode("utf-8", "replace"))
        answer = json.loads(result.stdout)
        if answer.get("ok") is False:
            raise RuntimeError(str(answer))
        return answer

    def complete(job):
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            status = cli("wait", job, "--timeout", 15)
            if status["state"] in ("succeeded", "failed", "cancelled"):
                log = cli("logs", job, "--since", 0)
                return status, log
        raise RuntimeError("Job timed out")

    try:
        started = time.monotonic()
        prepared = cli("prepare", rust, "--runtime", "rust", "--request-id", "runtime-prepare-" + run_id)
        status, log = complete(prepared["job"])
        receipt["checks"].append({"name": "locked_dependency_prepare", "job": prepared["job"], "duration_seconds": time.monotonic()-started, "status": status, "log": log})
        save()
        assert status["exit_code"] == 0
        started = time.monotonic()
        run = cli("run", rust, "--runtime", "rust", "--where", "spark", "--detach", "--no-cache", "--", "cargo", "run", "--locked", "--offline")
        status, log = complete(run["job"])
        receipt["checks"].append({"name": "offline_rust_build", "job": run["job"], "duration_seconds": time.monotonic()-started, "status": status, "log": log})
        save()
        assert status["exit_code"] == 0 and "42" in log.get("text", "")
        run = cli("run", gpu, "--runtime", "cuda", "--where", "spark", "--detach", "--no-cache", "--", "python", "probe.py")
        status, log = complete(run["job"])
        receipt["checks"].append({"name": "isolated_cuda_job", "job": run["job"], "status": status, "log": log})
        save()
        assert status["exit_code"] == 0
        receipt["ok"] = True
    except Exception as error:
        receipt["ok"] = False
        receipt["error"] = str(error)
        raise
    finally:
        receipt["finished_at"] = time.time()
        save()
    print(json.dumps({"ok": receipt["ok"], "jobs": [c["job"] for c in receipt["checks"]], "receipt": str(args.receipt)}))


if __name__ == "__main__":
    main()
