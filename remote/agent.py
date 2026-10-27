#!/usr/bin/env python3
"""Fixed in-container agent driver. It runs only inside an admitted `agent` job.

`run TOOL`: clone the submitted branch from /work/in.bundle, let the tool work on
branch agent/<job> with the prompt in /work/prompt.md, commit what it left, and
write only the new commits to /sparkdev-output/result.bundle with result.json.

`login codex`: start Codex's device sign-in, which prints its own link and code
to the job log; the sign-in is saved in the tool's home for later jobs.

The tool's sign-in lives in HOME (/sparkdev-agent-home), mounted only into that
tool's jobs. Claude's token is read from there and given to the claude process
alone, never to argv or the log.
"""
import json
import os
from pathlib import Path
import re
import subprocess
import sys

WORK = Path("/work")
OUTPUT = Path("/sparkdev-output")
HOME = Path(os.environ.get("HOME", "/sparkdev-agent-home"))
TOOLS = ("claude", "codex")
BRANCH = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._/-]{0,199}$")
JOB = re.compile(r"^[0-9a-f]{32}$")
MAX_PROMPT = 256 * 1024
TOKEN_FILE = "claude-oauth-token"


def git(repo, *args, check=True, env=None):
    return subprocess.run(["git", "-C", str(repo), *args], check=check, capture_output=True, text=True, env=env)


def request(work):
    spec = json.loads((work / "agent.json").read_text())
    if set(spec) - {"v", "branch", "job"} or spec.get("v") != 1:
        raise ValueError("unknown agent request")
    branch, job = spec.get("branch"), spec.get("job")
    if not isinstance(branch, str) or not BRANCH.match(branch) or ".." in branch or branch.endswith((".lock", "/")):
        raise ValueError("invalid branch name")
    if not isinstance(job, str) or not JOB.match(job):
        raise ValueError("invalid job name")
    prompt = (work / "prompt.md").read_text()
    if not prompt.strip() or len(prompt.encode()) > MAX_PROMPT:
        raise ValueError("the prompt must be non-empty text of at most 256 KiB")
    return branch, job, prompt


def tool_command(tool, prompt, home):
    """The headless command and its private environment for one tool."""
    env = {key: value for key, value in os.environ.items() if key not in ("CLAUDE_CODE_OAUTH_TOKEN", "OPENAI_API_KEY", "ANTHROPIC_API_KEY")}
    if tool == "claude":
        token_path = home / TOKEN_FILE
        if not token_path.is_file():
            raise PermissionError("Claude Code is not signed in on this device")
        env["CLAUDE_CODE_OAUTH_TOKEN"] = token_path.read_text().strip()
        # The container is the sandbox, so the tool's own permission prompts are off.
        return ["claude", "-p", prompt, "--output-format", "stream-json", "--verbose", "--permission-mode", "bypassPermissions"], env
    if tool == "codex":
        env["CODEX_HOME"] = str(home / ".codex")
        if not (home / ".codex" / "auth.json").is_file():
            raise PermissionError("Codex is not signed in on this device")
        return ["codex", "exec", "--json", "--dangerously-bypass-approvals-and-sandbox", prompt], env
    raise ValueError("unknown tool")


def redact(text, secret):
    return text.replace(secret, "[redacted]") if secret else text


def run(tool, work=WORK, output=OUTPUT, home=HOME):
    branch, job, prompt = request(work)
    repo = work / "repo"
    subprocess.run(["git", "clone", "--quiet", "--branch", branch, str(work / "in.bundle"), str(repo)], check=True)
    base = git(repo, "rev-parse", "HEAD").stdout.strip()
    git(repo, "checkout", "--quiet", "-b", "agent/" + job)
    git(repo, "config", "user.name", f"RTXBridge agent ({tool})")
    git(repo, "config", "user.email", f"agent-{tool}@rtxbridge.invalid")
    command, env = tool_command(tool, prompt, home)
    secret = env.get("CLAUDE_CODE_OAUTH_TOKEN", "")
    with open(output / "transcript.jsonl", "w", encoding="utf-8") as transcript, \
            subprocess.Popen(command, cwd=repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace") as process:
        for line in process.stdout:
            line = redact(line, secret)
            transcript.write(line)
            sys.stdout.write(line)
            sys.stdout.flush()
        code = process.wait()
    git(repo, "add", "-A")
    if git(repo, "diff", "--cached", "--quiet", check=False).returncode:
        git(repo, "commit", "--quiet", "-m", f"Changes left by the {tool} agent (job {job})")
    head = git(repo, "rev-parse", "HEAD").stdout.strip()
    commits = int(git(repo, "rev-list", "--count", f"{base}..{head}").stdout.strip())
    if commits:
        git(repo, "bundle", "create", str(output / "result.bundle"), f"{base}..agent/{job}")
    result = {"v": 1, "tool": tool, "branch": branch, "base": base, "head": head, "ref": "agent/" + job,
              "commits": commits, "exit_code": code}
    (output / "result.json").write_text(json.dumps(result, sort_keys=True))
    print(f"\nsparkdev agent: {commits} new commit{'s' if commits != 1 else ''} on agent/{job}; tool exit {code}")
    return code


def login(tool, home=HOME):
    if tool != "codex":
        raise ValueError("only Codex signs in on the device; Claude Code uses a pasted token")
    env = dict(os.environ, CODEX_HOME=str(home / ".codex"))
    (home / ".codex").mkdir(mode=0o700, parents=True, exist_ok=True)
    return subprocess.run(["codex", "login", "--device-auth"], env=env).returncode


def main(argv):
    if len(argv) != 3 or argv[1] not in ("run", "login") or argv[2] not in TOOLS:
        print("usage: agent.py run|login claude|codex", file=sys.stderr)
        return 2
    try:
        return run(argv[2]) if argv[1] == "run" else login(argv[2])
    except (ValueError, PermissionError, subprocess.CalledProcessError, OSError) as error:
        print(f"sparkdev agent: {error}", file=sys.stderr)
        return 3


if __name__ == "__main__":
    sys.exit(main(sys.argv))
