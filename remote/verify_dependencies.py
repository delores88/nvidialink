"""Physical Node/Python cache and controller-restart checks via the built CLI."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time
import urllib.request
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cli", required=True)
    parser.add_argument("--receipt", required=True, type=Path)
    args = parser.parse_args()
    run_id = uuid.uuid4().hex
    state = Path(os.environ.get("SPARKDEV_HOME", str(Path(os.environ["LOCALAPPDATA"]) / "SparkDevBridge")))
    root = state / "remote-validation" / run_id
    root.mkdir(parents=True)
    receipt = {"kind": "physical_dependency_cache_and_restart", "actual_caller": "codex", "started_at": time.time(), "checks": []}

    def cli(*values):
        result = subprocess.run([args.cli, "--json", "--client", "codex", *map(str, values)], capture_output=True, timeout=90)
        answer = json.loads(result.stdout)
        if result.returncode or answer.get("ok") is False:
            raise RuntimeError(str(answer))
        return answer

    def save():
        args.receipt.parent.mkdir(parents=True, exist_ok=True)
        args.receipt.write_text(json.dumps(receipt, indent=2))

    def complete(job):
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            status = cli("wait", job, "--timeout", 15)
            if status["state"] in ("succeeded", "failed", "cancelled"):
                return status, cli("logs", job, "--since", 0)
        raise RuntimeError("Job did not complete")

    try:
        with urllib.request.urlopen("https://registry.npmjs.org/is-number/7.0.0", timeout=20) as response:
            npm = json.load(response)
        with urllib.request.urlopen("https://pypi.org/pypi/six/1.17.0/json", timeout=20) as response:
            pypi = json.load(response)
        wheel = next(f for f in pypi["urls"] if f["filename"].endswith("py2.py3-none-any.whl"))
        node = root / "node"
        node.mkdir()
        package = {"name": "sparkdev-node-cache-proof", "version": "1.0.0", "dependencies": {"is-number": "7.0.0"}}
        (node / "package.json").write_text(json.dumps(package))
        lock = {"name": package["name"], "version": "1.0.0", "lockfileVersion": 3, "requires": True,
                "packages": {"": package, "node_modules/is-number": {"version": "7.0.0", "resolved": npm["dist"]["tarball"], "integrity": npm["dist"]["integrity"]}}}
        (node / "package-lock.json").write_text(json.dumps(lock))
        (node / "probe.js").write_text("console.log(JSON.stringify({number:require('is-number')(42),module:require.resolve('is-number')}))\n")
        python = root / "python"
        python.mkdir()
        (python / "requirements.lock").write_text("six==1.17.0 --hash=sha256:" + wheel["digests"]["sha256"] + "\n")
        (python / "probe.py").write_text("import json,six\nprint(json.dumps({'six':six.__version__,'module':six.__file__}))\n")
        for runtime, source, command in (("node", node, ["node", "probe.js"]), ("python", python, ["python3", "probe.py"])):
            prepared = cli("prepare", source, "--runtime", runtime, "--request-id", run_id + "-prepare-" + runtime)
            status, log = complete(prepared["job"])
            receipt["checks"].append({"name": runtime+"_locked_prepare", "status": status, "log": log})
            save()
            assert status["exit_code"] == 0
            job = cli("run", source, "--runtime", runtime, "--where", "spark", "--detach", "--no-cache", "--", *command)
            status, log = complete(job["job"])
            receipt["checks"].append({"name": runtime+"_offline_cache", "status": status, "log": log})
            save()
            assert status["exit_code"] == 0
            if runtime == "python":
                assert "/pip-cache/envs/" in log["text"]
        (python / "restart.py").write_text("import time\nprint('started-once',flush=True)\ntime.sleep(12)\nprint('finished-once',flush=True)\n")
        job = cli("run", python, "--runtime", "python", "--where", "spark", "--detach", "--no-cache", "--", "python3", "restart.py")
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            status = cli("wait", job["job"], "--timeout", 0)
            if status["state"] == "running":
                break
            time.sleep(.3)
        assert status["state"] == "running"
        config = json.loads((state / "config.json").read_text())
        ssh = ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=8", "-o", "HostName="+config["hosts"][0], "-o", "HostKeyAlias="+config["host_key_alias"], config["ssh_alias"]]
        subprocess.run([*ssh, "systemctl --user restart sparkdev-controller.service"], check=True, capture_output=True, timeout=20)
        status, log = complete(job["job"])
        assert status["exit_code"] == 0 and log["text"].count("started-once") == 1 and log["text"].count("finished-once") == 1
        inspect = subprocess.check_output([*ssh, "docker inspect sparkdev-job-" + job["job"]], timeout=20)
        container = json.loads(inspect)[0]
        assert container["RestartCount"] == 0
        receipt["checks"].append({"name": "controller_restart_no_duplicate", "status": status, "log": log, "docker_restart_count": 0})
        receipt["ok"] = True
    except Exception as error:
        receipt["ok"] = False
        receipt["error"] = str(error)
        raise
    finally:
        receipt["finished_at"] = time.time()
        save()
    print(json.dumps({"ok": True, "checks": len(receipt["checks"]), "receipt": str(args.receipt)}))


if __name__ == "__main__":
    main()
