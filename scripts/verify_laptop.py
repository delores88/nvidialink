"""Focused regression for stable Windows state, true detach and shared scheduling."""
from __future__ import annotations
import argparse
import base64
import json
import os
from pathlib import Path
import subprocess
import time
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    root = Path(os.environ["USERPROFILE"]) / "AppData/Local/SparkDevBridge"
    source = root / "acceptance" / str(uuid.uuid4())
    laptop, spark = source / "laptop", source / "spark"
    laptop.mkdir(parents=True)
    spark.mkdir()
    (laptop / "README.txt").write_text("Windows local detached acceptance\n")
    (spark / "sleep.py").write_text("import time\nprint('SPARK_STARTED',flush=True)\ntime.sleep(12)\nprint('SPARK_FINISHED',flush=True)\n")
    receipt = {"schema": 1, "client": "codex", "checks": [], "limitations": ["No actual Claude Code session in this receipt", "No hotspot or laptop sleep acceptance"]}
    def save():
        Path(args.output).parent.mkdir(parents=True, exist_ok=True)
        Path(args.output).write_text(json.dumps(receipt, indent=2) + "\n")
    def record(name, result):
        receipt["checks"].append({"name": name, "result": result})
        save()
        print(name, "PASS", flush=True)
    def call(*words, env=None, expected=0):
        out = subprocess.run([args.binary, "--json", "--client", "codex", *map(str, words)], env=env, text=True, capture_output=True, timeout=90)
        assert out.returncode == expected, out.stdout[-2000:] + out.stderr[-200:]
        return json.loads(out.stdout)
    def finish(job):
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            out = call("wait", job, "--timeout", "5")
            if out["state"] in ("succeeded", "failed", "cancelled"):
                return out
        raise AssertionError("Terminal state deadline expired")
    try:
        call("doctor")
        assert (root / "config.json").is_file(), "Unredirected Windows config was not created"
        simulated_explorer = dict(os.environ, LOCALAPPDATA=str(root.parent))
        call("doctor", env=simulated_explorer)
        record("same_state_in_codex_and_explorer_environment", {"known_folder_without_package_redirection": True, "both_ssh_doctors_pass": True})
        started = time.monotonic()
        local = call("run", laptop, "--detach", "--", "powershell.exe", "-NoProfile", "-NonInteractive", "-Command", "Write-Output LAPTOP_STARTED; Start-Sleep -Seconds 20; Write-Output LAPTOP_FINISHED")
        elapsed = time.monotonic() - started
        assert elapsed < 8, f"20 second local work blocked detached submission for {elapsed:.2f}s"
        remote = call("run", spark, "--detach", "--no-cache", "--", "python3", "sleep.py")
        for _ in range(8):
            board = call("status", "--all")
            states = {j["job"]: j["state"] for j in board.get("jobs", []) + board.get("laptop_jobs", [])}
            if states[local["job"]] == states[remote["job"]] == "running":
                break
            time.sleep(0.5)
        assert states[local["job"]] == states[remote["job"]] == "running", states
        rejected = call("run", laptop, "--where", "spark", "--detach", "--", "python3", "-c", "print('must not run')", expected=6)
        assert not rejected["ok"]
        ends = [finish(local["job"]), finish(remote["job"])]
        assert all(end["exit_code"] == 0 for end in ends), ends
        for submitted, marker in [(local, "LAPTOP_FINISHED"), (remote, "SPARK_FINISHED")]:
            assert marker in base64.b64decode(call("logs", submitted["job"], "--since", "0")["data"]).decode()
        record("detached_auto_laptop_and_spark_overlap", {"laptop_job": local["job"], "spark_job": remote["job"], "both_observed_running": True, "exit_codes": [0, 0], "admission_seconds_for_20_second_job": elapsed, "placements": [local["placement"], remote["placement"]], "same_worktree_claim_refused": True})
        cancelled = call("run", laptop, "--detach", "--", "powershell.exe", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 60")
        call("cancel", cancelled["job"])
        end = finish(cancelled["job"])
        assert end["state"] == "cancelled", end
        call("status", "--all")  # Delivers any persisted final board/log heartbeat.
        record("owned_laptop_process_tree_cancel", {"job": cancelled["job"], "state": end["state"], "exit_code": end["exit_code"]})
        receipt["status"] = "passed"
    except Exception as error:
        receipt["status"] = "failed"
        receipt["failure"] = str(error)
        raise
    finally:
        save()


if __name__ == "__main__":
    main()
