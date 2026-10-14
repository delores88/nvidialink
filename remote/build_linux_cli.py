"""Run inside the pinned ARM64 runtime after locked dependency preparation.

The build-only cargo-auditable archive is downloaded from its official release
and checksum-verified before sync; verify it again before executing its sole tool.
"""
from pathlib import Path
import hashlib
import json
import os
import subprocess
import tarfile


def main():
    archive = Path("/work/build-tools/cargo-auditable-aarch64-unknown-linux-gnu.tar.xz")
    expected = "bcb2d46002a8dd8e94d0d669cb39d38c9651db2ae5d1f3f1f1bab516ee1e212c"
    if hashlib.sha256(archive.read_bytes()).hexdigest() != expected:
        raise RuntimeError("Unexpected cargo-auditable archive")
    with tarfile.open(archive) as package:
        members = [m for m in package.getmembers() if m.isfile() and Path(m.name).name == "cargo-auditable"]
        if len(members) != 1:
            raise RuntimeError("Expected exactly one cargo-auditable executable")
        raw = package.extractfile(members[0]).read()
    tool_digest = hashlib.sha256(raw).hexdigest()
    if tool_digest != "f1fe64c099129892a42c11c863a4e7c1fac94d61e16d16b7382690cf8a8c6f72":
        raise RuntimeError("Unexpected cargo-auditable executable")
    tool = Path("/work/build-tools/cargo-auditable")
    tool.write_bytes(raw)
    tool.chmod(0o700)
    os.environ["PATH"] = str(tool.parent) + ":" + os.environ["PATH"]
    print(json.dumps({"tool": "cargo-auditable", "version": "0.7.7", "archive_sha256": expected,
                      "executable_sha256": tool_digest}), flush=True)
    commands = [
        ["cargo", "auditable", "build", "--release", "--locked", "--offline", "-p", "sparkdev"],
        ["cargo", "test", "--locked", "--offline", "-p", "sparkdev"],
        ["/target/release/sparkdev", "--help"],
        ["/target/release/sparkdev", "--version"],
    ]
    for command in commands:
        print(json.dumps({"command": command}), flush=True)
        code = subprocess.call(command)
        if code:
            return code
    # This fresh Linux client deliberately has no owner keys. Verify honest
    # unconfigured-device output and that planning never needs credentials.
    os.environ["SPARKDEV_HOME"] = "/tmp/sparkdev-linux-cli-check"
    fixture = Path("/tmp/sparkdev-plan-fixture")
    fixture.mkdir()
    (fixture / "probe.py").write_text("print('plan-only')\n")
    executable = "/target/release/sparkdev"
    devices = json.loads(subprocess.check_output([executable, "--json", "devices"]))
    assert devices["observation_only"] and not devices["claimed"]
    assert devices["devices"][0]["os"] == "linux"
    assert not devices["devices"][1]["reachable"]
    state = Path(os.environ["SPARKDEV_HOME"])
    before = sorted(str(p.relative_to(state)) for p in state.rglob("*"))
    plan = json.loads(subprocess.check_output([executable, "--json", "plan", str(fixture), "--", "python3", "probe.py", "a b", ""]))
    assert plan["argv"] == ["python3", "probe.py", "a b", ""]
    assert plan["observation_only"] and not any(plan[k] for k in ("claimed", "uploaded", "submitted"))
    assert sorted(str(p.relative_to(state)) for p in state.rglob("*")) == before
    requests = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "sparkdev-linux-check", "version": "1"}}},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "plan", "arguments": {"dir": str(fixture), "argv": ["python3", "probe.py", "a b", ""]}}},
    ]
    output = subprocess.check_output([executable, "--client", "codex", "mcp"], input=("\n".join(json.dumps(r) for r in requests) + "\n").encode())
    replies = {r["id"]: r for r in (json.loads(line) for line in output.splitlines())}
    names = [t["name"] for t in replies[2]["result"]["tools"]]
    assert set(names) == {"devices", "plan", "prepare", "run", "wait", "logs", "status", "cancel"}
    assert not replies[3]["result"].get("isError", False)
    print(json.dumps({"cli_devices": devices, "cli_plan": plan, "mcp_tool_names": names,
                      "mcp_plan_response": replies[3]["result"], "plan_state_unchanged": True}), flush=True)
    artifact = Path("/target/release/sparkdev").read_bytes()
    print(json.dumps({"artifact": "sparkdev", "bytes": len(artifact), "sha256": hashlib.sha256(artifact).hexdigest()}), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
