"""Bounded real Claude Code/Codex MCP acceptance; no login or model overrides.

Raw agent traces and MCP configuration stay in owner-local application state.
Only the sanitized receipt is written to --output. Existing sessions are never
resumed. A harness label alone cannot satisfy this test: both model tool-call
traces, completed jobs, logs, and observed shared-board overlap are required.
"""
from __future__ import annotations
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
import uuid


def events(path):
    out = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            out.append(json.loads(line))
        except ValueError:
            pass
    return out


def tool_calls(items, client):
    out = []
    for event in items:
        if client == "claude":
            for content in event.get("message", {}).get("content", []):
                if isinstance(content, dict) and content.get("type") == "tool_use":
                    name = content.get("name", "")
                    if name.startswith("mcp__sparkdev__"):
                        out.append({"tool": name, "arguments": content.get("input", {})})
        else:
            item = event.get("item", {})
            if event.get("type") == "item.completed" and item.get("type") == "mcp_tool_call" and item.get("server") == "sparkdev":
                out.append({"tool": "mcp__sparkdev__" + item.get("tool", ""), "arguments": item.get("arguments", {}), "status": item.get("status")})
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--claude", required=True)
    parser.add_argument("--codex", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--deadline", type=int, default=300)
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    root = Path(os.environ["USERPROFILE"]) / "AppData/Local/SparkDevBridge"
    run_id = str(uuid.uuid4())
    work = root / "acceptance" / ("agents-" + run_id)
    work.mkdir(parents=True)
    receipt = {"schema": 1, "status": "running", "binary_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(), "actual_agent_sessions": {}, "both_jobs_observed_running": False, "limitations": ["LAN SSH acceptance only; no hotspot or sleep claim"]}
    output = Path(args.output)
    def save():
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    def cli(*words):
        result = subprocess.run([binary, "--json", "--client", "human", *map(str, words)], capture_output=True, text=True, timeout=90)
        if result.returncode:
            raise RuntimeError("Acceptance observation failed: " + result.stdout[-500:])
        return json.loads(result.stdout)
    projects = {}
    for client in ("claude", "codex"):
        project = work / client
        project.mkdir()
        (project / "probe.py").write_text("import time\nprint('" + client.upper() + "_MCP_STARTED', flush=True)\ntime.sleep(60)\nprint('" + client.upper() + "_MCP_FINISHED', flush=True)\n", encoding="utf-8")
        projects[client] = {"path": project, "id": cli("sync", project, "--check")["project"]}
    config = work / "claude-mcp.json"
    config.write_text(json.dumps({"mcpServers": {"sparkdev": {"type": "stdio", "command": binary, "args": ["--client", "claude", "mcp"]}}}), encoding="utf-8")
    tools = ["mcp__sparkdev__" + name for name in ("devices", "plan", "prepare", "run", "wait", "logs", "status", "cancel")]
    prompts = {}
    for client in projects:
        prompts[client] = (
            "Perform this bounded Spark Dev Bridge acceptance, then stop. Do not inspect files, change settings, use other servers, or delegate. "
            "The fixture already exists. Using only the sparkdev MCP tools, first call devices to inspect the shared machines. Then call plan with "
            + json.dumps({"dir": str(projects[client]["path"]), "argv": ["python3", "probe.py"], "where": "spark", "runtime": "python"})
            + ". It must report spark placement with no claim or submission. Then call run exactly once with "
            + json.dumps({"dir": str(projects[client]["path"]), "argv": ["python3", "probe.py"], "where": "spark", "runtime": "python", "request_id": "real-" + client + "-" + run_id, "no_cache": True})
            + ". Save its job ID. Call wait with timeout 60, repeating at most twice if still active, then logs with since 0. "
            "Return the job ID, terminal state, exit code, and printed markers. A tool error is a failed test: report it without trying other commands. Keep reasoning and output concise."
        )
    # Explicit writable app state is needed because doctor persists connection state.
    # The shell call remains in workspace-write with network enabled, never bypass mode.
    doctor_cmd = "& '" + binary.replace("'", "''") + "' --json doctor"
    prompts["codex"] = (
        "First make exactly one shell/process tool call, under the configured workspace-write sandbox, running this PowerShell command: "
        + doctor_cmd + ". This verifies permitted process launch and strict SSH network access. Do not escalate permissions. "
        "Report failure if the sandbox denies it, then proceed with the independent MCP check. " + prompts["codex"]
    )
    commands = {
        "claude": [args.claude, "--print", "--verbose", "--output-format", "stream-json", "--no-session-persistence", "--session-id", str(uuid.uuid4()), "--strict-mcp-config", "--mcp-config", str(config), "--tools", "", "--allowedTools", ",".join(tools), "--permission-prompts", "none", "--max-budget-usd", "2.00", "--disable-slash-commands", "--no-chrome", prompts["claude"]],
        "codex": [args.codex, "exec", "--ephemeral", "--json", "--color", "never", "--skip-git-repo-check", "--sandbox", "workspace-write", "--add-dir", str(root), "-c", 'approval_policy="never"', "-c", "sandbox_workspace_write.network_access=true", "-c", "mcp_servers.sparkdev.command=" + json.dumps(binary.replace("\\", "/")), "-c", 'mcp_servers.sparkdev.args=["--client","codex","mcp"]', "-c", "mcp_servers.sparkdev.tool_timeout_sec=90", "-C", str(projects["codex"]["path"]), prompts["codex"]],
    }
    procs, handles = {}, []
    try:
        cli("doctor")
        for client, command in commands.items():
            stdout = (work / (client + ".jsonl")).open("wb")
            stderr = (work / (client + ".stderr")).open("wb")
            handles.extend([stdout, stderr])
            procs[client] = subprocess.Popen(command, cwd=projects[client]["path"], stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
            receipt["actual_agent_sessions"][client] = {"fresh_process": True, "model_override": False, "session_resume": False}
        save()
        deadline = time.monotonic() + min(max(args.deadline, 90), 600)
        jobs = {}
        while time.monotonic() < deadline:
            board = cli("status", "--all")
            for entry in board.get("jobs", []):
                for client, project in projects.items():
                    if entry.get("project") == project["id"]:
                        jobs[client] = entry
            if len(jobs) == 2 and all(j.get("state") == "running" for j in jobs.values()):
                receipt["both_jobs_observed_running"] = True
                receipt.setdefault("overlap_observed_unix", time.time())
                save()
            if all(process.poll() is not None for process in procs.values()):
                break
            time.sleep(5)
        for client, process in procs.items():
            if process.poll() is None:
                if os.name == "nt":
                    subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], capture_output=True, timeout=20)
                else:
                    process.terminate()
                receipt["actual_agent_sessions"][client]["deadline_exceeded"] = True
            process.wait(timeout=20)
        for handle in handles:
            handle.close()
        handles.clear()
        for client, process in procs.items():
            trace = events(work / (client + ".jsonl"))
            calls = tool_calls(trace, client)
            for call in calls:
                if isinstance(call.get("arguments"), dict) and "dir" in call["arguments"]:
                    call["arguments"]["dir"] = "<owner-local-acceptance>/" + client
            session = receipt["actual_agent_sessions"][client]
            session.update({"process_exit_code": process.returncode, "tool_calls": calls, "trace_sha256": hashlib.sha256((work / (client + ".jsonl")).read_bytes()).hexdigest()})
            if client == "codex":
                session["process_sandbox"] = "workspace-write; network explicitly permitted; application state writable; approval never"
                shell_items = [e["item"] for e in trace if e.get("type") == "item.completed" and e.get("item", {}).get("type") == "command_execution"]
                session["shell_receipts"] = [{"purpose": "installed sparkdev doctor", "exit_code": item.get("exit_code"), "status": item.get("status"), "authenticated_ssh": '"selected":"ssh"' in item.get("aggregated_output", "").replace(" ", "")} for item in shell_items]
                session["restricted_process_and_network_verified"] = any(item["exit_code"] == 0 and item["authenticated_ssh"] for item in session["shell_receipts"])
        save()
        for client, process in procs.items():
            session = receipt["actual_agent_sessions"][client]
            calls = session["tool_calls"]
            assert process.returncode == 0, client + " session failed (raw trace retained owner-locally)"
            assert all(any(c["tool"] == "mcp__sparkdev__" + name for c in calls) for name in ("devices", "plan", "run", "wait", "logs")), client + " did not make required actual MCP tool calls"
            assert sum(c["tool"] == "mcp__sparkdev__run" for c in calls) == 1, client + " submitted more than the requested one job"
            assert client in jobs, client + " job missing from board"
            final = cli("wait", jobs[client]["job"], "--timeout", "2")
            logs = cli("logs", jobs[client]["job"], "--since", "0")
            text = base64.b64decode(logs["data"]).decode()
            assert final.get("exit_code") == 0 and final.get("state") == "succeeded", client + " workload failed"
            assert client.upper() + "_MCP_STARTED" in text and client.upper() + "_MCP_FINISHED" in text
            session["job"] = {"job": jobs[client]["job"], "state": final["state"], "exit_code": final["exit_code"], "logs": text, "next_offset": logs["next_offset"]}
        assert receipt["both_jobs_observed_running"], "Actual agent jobs did not overlap"
        receipt["status"] = "passed"
    except Exception as exc:
        receipt["status"] = "failed"
        receipt["failure"] = str(exc)
        raise
    finally:
        for process in procs.values():
            if process.poll() is None:
                if os.name == "nt":
                    subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], capture_output=True, timeout=20)
                else:
                    process.terminate()
        for handle in handles:
            handle.close()
        save()
        print(json.dumps({"status": receipt["status"], "receipt": str(output), "private_trace_directory": str(work)}), flush=True)


if __name__ == "__main__":
    main()
