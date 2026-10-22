#!/usr/bin/env python3
"""Physical, resumable automatic-return acceptance; never changes owner policy.

Prepare is local only. Run submit/verify only after the release owner has deployed
the candidate and coordinated Spark access. Use distinct empty project/receipt
folders for LAN and paired passes; --expect-route verifies the observed route,
not whether a paired connection traverses a relay or phone hotspot.

Example (repeat common arguments on every invocation):
  python verify_returns.py prepare --cli CLI --state-dir STATE \
      --project-dir EMPTY_FIXTURE_FOLDER --receipt-dir RECEIPTS --expect-route lan_ssh
  python verify_returns.py submit ... --case files
  python verify_returns.py checkpoint ... --case files
  python verify_returns.py verify ... --case files
  python verify_returns.py submit ... --case cache
  python verify_returns.py verify ... --case cache
  python verify_returns.py submit ... --case cache-repeat
  python verify_returns.py verify ... --case cache-repeat
  python verify_returns.py submit ... --case no-return
  python verify_returns.py verify ... --case no-return

For a real mid-transfer interruption, checkpoint a partial staging file, let the
release owner perform a narrowly coordinated interruption, checkpoint the actual
transport failure, restore connectivity, then verify --require-resume. This
script never disrupts networking, kills shared workers, impersonates Claude or
claims an interruption merely because a test fixture would survive one.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import time
import uuid

MIB = 1024 * 1024
COUNT = 130
BLOCK = b"DGXBridge result fixture\n"
BLOCK = (BLOCK * (MIB // len(BLOCK) + 1))[:MIB]
ORIGINAL = b"laptop original\n"
REMOTE_ORIGINAL = b"Spark replaced the unchanged original\n"
CONFLICT_ORIGINAL = b"original conflict source\n"
LOCAL_EDIT = b"laptop edit made after submission\n"
REMOTE_CONFLICT = b"Spark result alongside a newer local edit\n"
NEW = b"new source created on Spark\n"
CACHE = b"immutable cached result\n"
PROGRAM = '''from pathlib import Path
import hashlib,json,sys,time
case=sys.argv[1]
out=Path('/sparkdev-output')
print('dgxbridge-fixture-start', flush=True)
if case=='files':
    time.sleep(40)
    Path('/work/original.txt').write_bytes(%r)
    Path('/work/conflict.txt').write_bytes(%r)
    Path('/work/returned_new.txt').write_bytes(%r)
    block=%r
    block=(block*(1048576//len(block)+1))[:1048576]
    digest=hashlib.sha256()
    with (out/'render.bin').open('wb') as f:
        for _ in range(130): f.write(block);digest.update(block)
    print(json.dumps({'artifact':'render.bin','bytes':130*1048576,'sha256':digest.hexdigest()}),flush=True)
elif case=='cache':
    (out/'cached.txt').write_bytes(%r)
else:
    Path('/work/returned_new.txt').write_bytes(%r)
    (out/'disabled.txt').write_text('must stay on Spark')
print('dgxbridge-fixture-complete',flush=True)
''' % (REMOTE_ORIGINAL, REMOTE_CONFLICT, NEW, b"DGXBridge result fixture\n", CACHE, NEW)


class CLIError(RuntimeError):
    def __init__(self, code):
        self.code = code
        super().__init__("CLI failed with code " + str(code))


def load(path):
    return json.loads(Path(path).read_text(encoding="utf-8-sig"))


def save(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    pending = path.with_name(path.name + ".pending-" + uuid.uuid4().hex)
    with pending.open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(value, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(pending, path)


def file_hash(path):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
        raise RuntimeError("Acceptance result is a link or nonregular file")
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while data := stream.read(MIB):
            digest.update(data)
    return digest.hexdigest(), info.st_size


def same_location(left, right, *, directory=False):
    """Compare existing filesystem identities, including Windows path aliases."""
    left, right = Path(left), Path(right)
    expected_type = stat.S_ISDIR if directory else stat.S_ISREG
    for path in (left, right):
        info = path.lstat()
        if not expected_type(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
            raise RuntimeError("Acceptance location is a link or has the wrong file type")
    return left.samefile(right)


def expected_large_hash():
    digest = hashlib.sha256()
    for _ in range(COUNT):
        digest.update(BLOCK)
    return digest.hexdigest()


class Acceptance:
    def __init__(self, args):
        self.args = args
        self.project = Path(args.project_dir).absolute()
        self.receipts = Path(args.receipt_dir).absolute()
        self.state = Path(args.state_dir).absolute()
        if self.receipts == self.project or self.project in self.receipts.parents:
            raise RuntimeError("Keep receipts outside fixture projects so receipts are never synced")
        self.record_file = self.receipts / "fixture.json"
        self.env = dict(os.environ, SPARKDEV_HOME=str(self.state))

    def cli(self, *args, timeout=90):
        child = subprocess.run([str(self.args.cli), "--client", "codex", "--json", *map(str, args)],
                               env=self.env, stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout)
        try:
            value = json.loads(child.stdout)
        except ValueError:
            raise RuntimeError("CLI did not return JSON; raw process output withheld") from None
        if child.returncode or value.get("ok") is False:
            error = value.get("error", {})
            raise CLIError(error.get("code", child.returncode))
        return value

    def prepare(self):
        if self.record_file.exists():
            record = load(self.record_file)
            if record["project"] != str(self.project) or record["state_dir"] != str(self.state):
                raise RuntimeError("These receipts belong to a different fixture/state")
            return {"prepared": True, "resumed_existing": True, "fixture": record["id"]}
        if self.project.exists() and any(self.project.iterdir()):
            raise RuntimeError("Use a new empty dedicated fixture directory; existing files are preserved")
        self.project.mkdir(parents=True, exist_ok=True)
        for case in ("files", "cache", "no-return"):
            folder = self.project / case
            folder.mkdir()
            (folder / "fixture.py").write_text(PROGRAM, encoding="utf-8", newline="\n")
            if case == "files":
                (folder / "original.txt").write_bytes(ORIGINAL)
                (folder / "conflict.txt").write_bytes(CONFLICT_ORIGINAL)
        record = {"v": 1, "id": uuid.uuid4().hex, "project": str(self.project), "state_dir": str(self.state),
                  "expected_route": self.args.expect_route, "submissions": {}}
        save(self.record_file, record)
        return {"prepared": True, "fixture": record["id"], "real_jobs_started": False}

    def record(self):
        record = load(self.record_file)
        if record["project"] != str(self.project) or record["state_dir"] != str(self.state) or record["expected_route"] != self.args.expect_route:
            raise RuntimeError("Fixture paths/route differ from retained receipt")
        return record

    def route(self, require_idle=False):
        board = self.cli("status", "--all")
        observed = board.get("transport", {})
        if board.get("remote_available") is False:
            raise CLIError(board.get("remote_error", {}).get("code", "unknown"))
        if observed.get("authenticated") is not True or observed.get("route") != self.args.expect_route:
            raise RuntimeError("Observed authenticated transport differs from --expect-route")
        if require_idle and any(job.get("state") in ("queued", "starting", "running") for job in board.get("jobs", [])):
            raise RuntimeError("Spark has active jobs; coordinate priority and retry after they finish")
        return {"route": observed["route"], "authenticated": True, "observed_at": time.time(),
                "relay_or_hotspot_proven": False}

    def submit(self):
        record = self.record()
        case = self.args.case
        base_case = "cache" if case == "cache-repeat" else case
        if case == "cache-repeat" and not (self.receipts / "cache-verified.json").exists():
            raise RuntimeError("Verify the initial cache job before submitting its identical repeat")
        saved = record["submissions"].setdefault(case, {"request_id": "return-" + record["id"] + "-" + case})
        save(self.record_file, record)  # Retain exact identity before any uncertain submission.
        if saved.get("job"):
            self.edit_conflict(record, case, saved)
            return {"job": saved["job"], "resumed_existing": True}
        # A lost foreground response may already have a durable background receipt.
        owner_receipt = self.state / "returns" / (saved["request_id"] + ".json")
        recovered = load(owner_receipt).get("submission") if owner_receipt.exists() else None
        route = self.route(require_idle=not bool(recovered))
        if recovered:
            response = recovered
        else:
            argv = self.receipts / (case + "-argv.json")
            save(argv, ["python3", "fixture.py", base_case])
            command = ["run", str(self.project / base_case), "--where", "spark", "--runtime", "python", "--detach",
                       "--request-id", saved["request_id"], "--argv-json", str(argv)]
            if case == "no-return":
                command.append("--no-return")
            response = self.cli(*command, timeout=120)
        saved.update(job=response["job"], response=response, route=route, submitting_cli_exited=True, submitted_at=time.time())
        save(self.record_file, record)
        self.edit_conflict(record, case, saved)
        return {"job": saved["job"], "case": case, "submitting_cli_exited": True, "route": route}

    def edit_conflict(self, record, case, saved):
        if case == "files" and "conflict_edited_at" not in saved:
            conflict = self.project / "files" / "conflict.txt"
            if conflict.read_bytes() not in (CONFLICT_ORIGINAL, LOCAL_EDIT):
                raise RuntimeError("Return raced the conflict edit; this run cannot prove conflict preservation")
            conflict.write_bytes(LOCAL_EDIT)
            saved["conflict_edit_sha256"] = hashlib.sha256(LOCAL_EDIT).hexdigest()
            saved["conflict_edited_at"] = time.time()
            save(self.record_file, record)

    def checkpoint(self):
        record = self.record()
        saved = record["submissions"][self.args.case]
        job = saved["job"]
        key = hashlib.sha256(f"{job}:output:render.bin".encode()).hexdigest()
        staging = self.state / "returns" / "staging" / (key + ".part")
        result = {"at": time.time(), "job": job, "staging_bytes": staging.stat().st_size if staging.is_file() else 0,
                  "expected_bytes": COUNT * MIB, "transport_error": None, "check_error": None}
        try:
            result["route"] = self.route()
            result["job_status"] = self.cli("wait", job, "--timeout", "0")
        except CLIError as error:
            key = "transport_error" if str(error.code) == "4" else "check_error"
            result[key] = str(error)
        except (RuntimeError, subprocess.TimeoutExpired) as error:
            result["check_error"] = type(error).__name__ + ": " + (str(error) if isinstance(error, RuntimeError) else "CLI deadline exceeded; cause unconfirmed")
        previous = self.receipts / (self.args.case + "-checkpoints.json")
        checkpoints = load(previous) if previous.exists() else []
        checkpoints.append(result)
        save(previous, checkpoints)
        return result

    def verify(self):
        record = self.record()
        case, saved = self.args.case, record["submissions"][self.args.case]
        job = saved["job"]
        until = time.monotonic() + self.args.timeout
        while True:
            final = self.cli("wait", job, "--timeout", "20")
            save(self.receipts / (case + "-last-status.json"), final)
            if final.get("state") in ("succeeded", "failed", "cancelled"):
                break
            if time.monotonic() >= until:
                raise RuntimeError("Acceptance is still pending; retained job ID is safe to poll again")
            time.sleep(1)
        if final["state"] != "succeeded" or final.get("exit_code") != 0:
            raise RuntimeError("The physical Spark workload failed; inspect the retained status")
        route = self.route()
        logs = self.cli("logs", job, "--since", "0")
        text = base64.b64decode(logs["data"]).decode("utf-8")
        if text.count("dgxbridge-fixture-complete") != 1:
            raise RuntimeError("Expected exactly one completed fixture execution in saved logs")
        verified = []
        if case == "no-return":
            if final.get("return_status") != "disabled" or final.get("returned"):
                raise RuntimeError("Explicit opt-out did not disable automatic return")
            if (self.project / "no-return" / "returned_new.txt").exists():
                raise RuntimeError("An opted-out work file unexpectedly returned")
            if (self.state / "results" / job / "disabled.txt").exists():
                raise RuntimeError("An opted-out output file unexpectedly returned")
        else:
            if final.get("return_status") != "complete":
                raise RuntimeError("Wait reported completion before verified automatic return")
            expected = {("output", "cached.txt"): (hashlib.sha256(CACHE).hexdigest(), len(CACHE))}
            if case == "files":
                expected = {("work", "original.txt"): (hashlib.sha256(REMOTE_ORIGINAL).hexdigest(), len(REMOTE_ORIGINAL)),
                            ("work", "conflict.txt"): (hashlib.sha256(REMOTE_CONFLICT).hexdigest(), len(REMOTE_CONFLICT)),
                            ("work", "returned_new.txt"): (hashlib.sha256(NEW).hexdigest(), len(NEW)),
                            ("output", "render.bin"): (expected_large_hash(), COUNT * MIB)}
                if (self.project / "files" / "conflict.txt").read_bytes() != LOCAL_EDIT:
                    raise RuntimeError("A newer laptop edit was overwritten")
            returned = {(r["scope"], r["source_path"]): r for r in final["returned"]}
            if set(returned) != set(expected):
                raise RuntimeError("Returned file set differs from the physical fixture")
            for key, (digest, size) in expected.items():
                receipt = returned[key]
                path = Path(receipt["path"])
                if file_hash(path) != (digest, size) or receipt["sha256"] != digest or receipt["bytes"] != size:
                    raise RuntimeError("A returned file failed its independent full-file SHA-256/size check")
                if key == ("work", "conflict.txt") and receipt["status"] != "conflict":
                    raise RuntimeError("The newer local edit was not reported as a conflict")
                if key[0] == "work":
                    desired = self.project / "files" / key[1]
                    if key[1] == "conflict.txt":
                        if not same_location(path.parent, desired.parent, directory=True) or same_location(path, desired):
                            raise RuntimeError("Conflict output is not a sibling of the preserved source file")
                    elif not same_location(path, desired) or receipt["status"] != "written":
                        raise RuntimeError("New/unchanged-original work did not return to its original source path")
                elif not same_location(path, self.state / "results" / job / key[1]):
                    raise RuntimeError("Output did not land under the predictable owner results folder")
                verified.append({"scope": key[0], "path": str(path), "bytes": size, "sha256": digest, "status": receipt["status"]})
            if case == "cache-repeat" and (not final.get("cache_hit") or final.get("cached_from") != record["submissions"]["cache"]["job"]):
                raise RuntimeError("The identical repeated job did not reuse its original immutable result")
        history = self.receipts / (case + "-checkpoints.json")
        checkpoints = load(history) if history.exists() else []
        partial = [c for c in checkpoints if 0 < c["staging_bytes"] < c["expected_bytes"]]
        drop = any(c["transport_error"] and any(p["at"] <= c["at"] for p in partial) for c in checkpoints)
        if self.args.require_resume and not (partial and drop):
            raise RuntimeError("No partial-transfer plus actual transport-failure checkpoint proves the requested resume test")
        result = {"job": job, "request_id": saved["request_id"], "case": case, "client": "codex", "state": final["state"],
                  "exit_code": final["exit_code"], "return_status": final["return_status"], "verified": verified,
                  "submitting_cli_exited": saved["submitting_cli_exited"], "route": route,
                  "partial_chunk_checkpoint_observed": bool(partial), "transport_failure_after_partial_observed": drop,
                  "cache_hit": bool(final.get("cache_hit")), "cached_from": final.get("cached_from"), "verified_at": time.time()}
        save(self.receipts / (case + "-verified.json"), result)
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("action", choices=("prepare", "submit", "checkpoint", "verify"))
    parser.add_argument("--cli", required=True)
    parser.add_argument("--state-dir", required=True)
    parser.add_argument("--project-dir", required=True)
    parser.add_argument("--receipt-dir", required=True)
    parser.add_argument("--expect-route", choices=("lan_ssh", "paired_iroh"), required=True)
    parser.add_argument("--case", choices=("files", "cache", "cache-repeat", "no-return"), default="files")
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--require-resume", action="store_true")
    args = parser.parse_args()
    try:
        result = getattr(Acceptance(args), args.action)()
    except (OSError, ValueError, RuntimeError, KeyError, subprocess.TimeoutExpired) as error:
        print(json.dumps({"ok": False, "error": str(error)}))
        return 1
    print(json.dumps({"ok": True, **result}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
