"""Benchmark a caller-supplied, isolated PhaseForge archive using sparkdev only.

Never point this at a live PhaseForge checkout. The parent directory must be this
repository's ignored .local/phaseforge-benchmark; one temporary source comment is
restored in finally. Run receipts distinguish actual executions from cache hits.
"""
import argparse
import base64
import json
from pathlib import Path
import re
import subprocess
import time

SUMMARY = re.compile(r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cli", required=True)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    args = parser.parse_args()
    source = args.source.resolve()
    allowed = Path(__file__).resolve().parent.parent / ".local/phaseforge-benchmark"
    if not source.is_relative_to(allowed.resolve()) or source == allowed.resolve():
        raise RuntimeError("Benchmark source must be an isolated archive under this repo's .local/phaseforge-benchmark")
    receipt = {"kind": "physical_phaseforge_backend_benchmark", "source_commit": "ac30e04fbf0ddc03ae0144fb06b6350cacdce1fc", "actual_caller": "codex",
               "platform": "aarch64 Linux on physical Spark", "phases": [], "started_at": time.time(), "laptop_comparison": "pending separate receipt"}
    args.receipt.parent.mkdir(parents=True, exist_ok=True)

    def save():
        args.receipt.write_text(json.dumps(receipt, indent=2))

    def cli(*values):
        result = subprocess.run([args.cli, "--json", "--client", "codex", *map(str, values)], capture_output=True, timeout=180)
        answer = json.loads(result.stdout)
        if result.returncode or answer.get("ok") is False:
            raise RuntimeError(str(answer))
        return answer

    command = ["cargo", "test", "--locked", "--offline", "--all-targets", "--no-fail-fast", "--manifest-path", "backend/Cargo.toml"]
    changed = source / "backend/src/main.rs"
    original = changed.read_bytes()
    try:
        for phase in ("cold", "no_change", "one_file_change"):
            if phase == "one_file_change":
                changed.write_bytes(original + b"\n// Spark Dev Bridge isolated benchmark: one-file rebuild probe.\n")
            started = time.time()
            sync = cli("sync", source)
            submitted = cli("run", source, "--where", "spark", "--runtime", "rust", "--detach", "--no-cache", "--request-id", "pf-benchmark-"+phase+"-"+str(time.time_ns()), "--", *command)
            record = {"phase": phase, "command": command, "sync": sync, "submitted": submitted, "started_at": started}
            receipt["phases"].append(record)
            save()
            print(json.dumps({"phase": phase, "job": submitted["job"], "state": submitted["state"]}), flush=True)
            log_path = args.receipt.with_name("phaseforge-"+phase+".log")
            offset = 0
            deadline = time.monotonic() + 4*3600
            with log_path.open("wb") as output:
                while time.monotonic() < deadline:
                    status = cli("wait", submitted["job"], "--timeout", 15)
                    logs = cli("logs", submitted["job"], "--since", offset)
                    raw = base64.b64decode(logs["data"])
                    output.write(raw)
                    output.flush()
                    offset = logs["next_offset"]
                    record["status"] = status
                    record["log_bytes"] = offset
                    save()
                    if status["state"] in ("succeeded", "failed", "cancelled"):
                        while True:
                            logs = cli("logs", submitted["job"], "--since", offset)
                            raw = base64.b64decode(logs["data"])
                            output.write(raw)
                            offset = logs["next_offset"]
                            if not raw:
                                break
                        break
                else:
                    cli("cancel", submitted["job"])
                    raise RuntimeError("Benchmark reached its four-hour bound; cancellation queued")
            record["finished_at"] = time.time()
            record["wall_seconds"] = record["finished_at"] - started
            record["execution_seconds"] = status["finished_at"] - status["started_at"] if status.get("started_at") and status.get("finished_at") else None
            text = log_path.read_text(errors="replace")
            record["cargo_build_reports"] = re.findall(r"Finished `[^`]+` profile[^\n]*target\(s\) in ([^\r\n]+)", text)
            record["test_summaries"] = [{"passed": int(a), "failed": int(b), "ignored": int(c), "measured": int(d), "filtered": int(e)} for a,b,c,d,e in SUMMARY.findall(text)]
            record["log"] = log_path.name
            save()
            print(json.dumps({"phase": phase, "exit_code": status["exit_code"], "seconds": record["execution_seconds"], "counts": record["test_summaries"]}), flush=True)
            if not record["test_summaries"] and status["exit_code"] != 0:
                receipt["blocked"] = "Compilation or setup failed before tests; further timing runs would only repeat the blocker"
                break
        receipt["completed"] = len(receipt["phases"]) == 3
    finally:
        changed.write_bytes(original)
        receipt["finished_at"] = time.time()
        save()


if __name__ == "__main__":
    main()
