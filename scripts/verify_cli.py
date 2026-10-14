"""Real installed-binary acceptance against an explicitly configured owner Spark.

This only creates its own application-state source trees and sparkdev jobs.
It never uses another agent's client label or changes PhaseForge resources.
"""
from __future__ import annotations
import argparse
import base64
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import time
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    binary = Path(args.binary).resolve()
    root = Path(os.environ.get("SPARKDEV_HOME", str(Path(os.environ["USERPROFILE"]) / "AppData/Local/SparkDevBridge")))
    run_id = str(uuid.uuid4())
    source = root / "acceptance" / run_id / "spark"
    laptop = root / "acceptance" / run_id / "laptop"
    source.mkdir(parents=True)
    laptop.mkdir(parents=True)
    (source / "argv.py").write_text("import json,sys\nprint(json.dumps(sys.argv[1:],ensure_ascii=False))\n", encoding="utf-8")
    (source / "sleep.py").write_text("import sys,time\nprint('SPARK_STARTED',flush=True)\ntime.sleep(float(sys.argv[1]))\nprint('SPARK_FINISHED',flush=True)\n", encoding="utf-8")
    (laptop / "README.txt").write_text("Owned laptop acceptance source\n", encoding="utf-8")
    receipt = {"schema": 1, "run_id": run_id, "client": "codex", "platform": "Windows", "source": "physical Spark via authenticated SSH", "checks": [], "limitations": ["Not an actual Claude Code invocation", "No phone hotspot, laptop sleep or native UI acceptance in this receipt"]}
    def save():
        Path(args.output).parent.mkdir(parents=True, exist_ok=True)
        Path(args.output).write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    def record(name, result):
        receipt["checks"].append({"name": name, "result": result})
        save()
        print(name, "PASS", flush=True)
    def call(*words, shell=None):
        command = [str(binary), "--json", "--client", "codex", *map(str, words)]
        if shell == "PowerShell":
            ps = "& " + " ".join("'" + word.replace("'", "''") + "'" for word in command)
            process = subprocess.run([shutil.which("pwsh"), "-NoProfile", "-NonInteractive", "-Command", ps], text=True, capture_output=True, timeout=120)
        elif shell == "cmd":
            # cmd parses its /c tail itself; passing a Python argv list would
            # apply C-runtime escaping to that shell program a second time.
            cmdline = subprocess.list2cmdline([os.environ.get("COMSPEC", "cmd.exe")]) + ' /d /s /c "' + subprocess.list2cmdline(command) + '"'
            process = subprocess.run(cmdline, text=True, capture_output=True, timeout=120)
        elif shell == "Git Bash":
            unix_binary = "/" + binary.drive[0].lower() + binary.as_posix()[2:]
            command[0] = unix_binary
            env = dict(os.environ, MSYS_NO_PATHCONV="1")
            process = subprocess.run(["C:/Program Files/Git/bin/bash.exe", "--noprofile", "--norc", "-c", shlex.join(command)], env=env, text=True, capture_output=True, timeout=120)
        else:
            process = subprocess.run(command, text=True, capture_output=True, timeout=120)
        if process.returncode != 0:
            raise AssertionError(f"CLI failed ({process.returncode}): {process.stdout[-1500:]} {process.stderr[-200:]}")
        return json.loads(process.stdout)
    def finish(job, max_seconds=90):
        deadline = time.monotonic() + max_seconds
        while time.monotonic() < deadline:
            value = call("wait", job, "--timeout", "5")
            if value["state"] in ("succeeded", "failed", "cancelled"):
                return value
        raise AssertionError("Job did not reach terminal state before acceptance deadline")
    def text(job):
        return base64.b64decode(call("logs", job, "--since", "0")["data"]).decode()
    try:
        doctor = call("doctor")
        assert doctor["transport"]["selected"] == "ssh"
        record("authenticated_ssh_doctor", {"ok": True, "selected": "ssh"})
        expected = ["two words", "", "$literal; & text", 'quotes"inside']
        first = None
        for shell in ("PowerShell", "cmd", "Git Bash"):
            response = call("run", source, "--where", "spark", "--runtime", "python", "--detach", "--", "python3", "argv.py", *expected, shell=shell)
            end = finish(response["job"])
            assert end["exit_code"] == 0, end
            assert json.loads(text(response["job"])) == expected
            record("exact_argv_" + shell, {"job": response["job"], "exit_code": end["exit_code"], "argv_verified": expected, "cache_hit": response.get("cache_hit")})
            first = first or response
        repeated = call("run", source, "--where", "spark", "--runtime", "python", "--detach", "--", "python3", "argv.py", *expected)
        assert repeated["cache_hit"] is True, repeated
        assert json.loads(text(repeated["job"])) == expected
        record("successful_result_reuse", {"job": repeated["job"], "cached_from": repeated.get("cached_from"), "cache_hit": True})
        request = "acceptance-" + run_id
        one = call("run", source, "--where", "spark", "--runtime", "python", "--detach", "--request-id", request, "--", "python3", "argv.py", "idempotent")
        finish(one["job"])
        two = call("run", source, "--where", "spark", "--runtime", "python", "--detach", "--request-id", request, "--", "python3", "argv.py", "idempotent")
        assert one["job"] == two["job"]
        record("request_idempotency", {"job": one["job"], "duplicate_run": False})
        chunk = call("logs", first["job"], "--since", "0", "--limit", "7")
        chunk2 = call("logs", first["job"], "--since", "0", "--limit", "7")
        rest = call("logs", first["job"], "--since", "7")
        assert chunk["data"] == chunk2["data"]
        assert json.loads((base64.b64decode(chunk["data"]) + base64.b64decode(rest["data"])).decode()) == expected
        record("lossless_offset_logs", {"first_offset": 0, "next_offset": chunk["next_offset"], "complete_bytes": rest["next_offset"], "repeat_identical": True})
        synced = call("sync", source)
        assert synced["uploaded_bytes"] == 0
        record("no_change_sync", {"uploaded_bytes": 0, "sync_seconds": synced["sync_seconds"]})
        started = time.monotonic()
        local = call("run", laptop, "--detach", "--", "powershell.exe", "-NoProfile", "-NonInteractive", "-Command", "Write-Output 'LAPTOP_STARTED'; Start-Sleep -Seconds 20; Write-Output 'LAPTOP_FINISHED'")
        detach_seconds = time.monotonic() - started
        assert detach_seconds < 8, f"Detached laptop admission took {detach_seconds:.2f}s for a 20s job"
        remote = call("run", source, "--detach", "--no-cache", "--", "python3", "sleep.py", "15")
        for _ in range(8):
            board = call("status", "--all")
            jobs = board.get("jobs", []) + board.get("laptop_jobs", [])
            states = {j["job"]: j["state"] for j in jobs}
            if states[local["job"]] == states[remote["job"]] == "running":
                break
            time.sleep(0.5)
        assert states[local["job"]] == states[remote["job"]] == "running", states
        local_end, remote_end = finish(local["job"]), finish(remote["job"])
        assert local_end["exit_code"] == remote_end["exit_code"] == 0
        assert "LAPTOP_FINISHED" in text(local["job"])
        assert "SPARK_FINISHED" in text(remote["job"])
        record("concurrent_laptop_and_spark", {"laptop_job": local["job"], "spark_job": remote["job"], "board_states": {local["job"]: states[local["job"]], remote["job"]: states[remote["job"]]}, "exit_codes": [0, 0], "actual_client": "codex", "auto_placement": [local["placement"], remote["placement"]], "detached_admission_seconds": detach_seconds})
        sleeper = call("run", source, "--where", "spark", "--detach", "--no-cache", "--", "python3", "sleep.py", "60")
        call("cancel", sleeper["job"])
        stopped = finish(sleeper["job"])
        assert stopped["state"] == "cancelled", stopped
        record("remote_cancel", {"job": sleeper["job"], "state": stopped["state"], "exit_code": stopped["exit_code"]})
        receipt["status"] = "passed"
    except Exception as error:
        receipt["status"] = "failed"
        receipt["failure"] = str(error)
        raise
    finally:
        save()


if __name__ == "__main__":
    main()
