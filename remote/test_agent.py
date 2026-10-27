import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
from unittest import mock

import agent

JOB = "0123456789abcdef0123456789abcdef"
TOKEN = "sk-ant-oat01-fixture-token-value"
# A stand-in tool: records what it was given, commits one change, leaves another
# uncommitted, and prints its token to prove the transcript is filtered.
FAKE = """#!/bin/sh
printf '{"argv":"%s","token":"%s","codex_home":"%s"}\\n' "$*" "$CLAUDE_CODE_OAUTH_TOKEN" "$CODEX_HOME"
echo one > one.txt && git add one.txt && git commit -q -m "first agent change"
echo two > two.txt
exit 0
"""


def sh(*args, cwd=None):
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


class AgentDriver(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.work, self.output, self.home, self.bin = (root / name for name in ("work", "output", "home", "bin"))
        for path in (self.work, self.output, self.home, self.bin):
            path.mkdir()
        source = root / "source"
        sh("git", "init", "-q", "-b", "feature/x", str(source))
        sh("git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "base", cwd=source)
        sh("git", "bundle", "create", str(self.work / "in.bundle"), "feature/x", cwd=source)
        (self.work / "prompt.md").write_text("Add the files.")
        (self.work / "agent.json").write_text(json.dumps({"v": 1, "branch": "feature/x", "job": JOB}))
        for name in ("claude", "codex"):
            tool = self.bin / name
            tool.write_text(FAKE)
            tool.chmod(tool.stat().st_mode | stat.S_IEXEC)
        self.path = mock.patch.dict(os.environ, {"PATH": f"{self.bin}{os.pathsep}{os.environ['PATH']}", "GIT_CONFIG_GLOBAL": str(root / "gitconfig")})
        self.path.start()

    def tearDown(self):
        self.path.stop()
        self.temp.cleanup()

    def test_claude_work_comes_back_as_new_commits_only_with_the_token_filtered(self):
        (self.home / agent.TOKEN_FILE).write_text(TOKEN + "\n")
        self.assertEqual(agent.run("claude", self.work, self.output, self.home), 0)
        result = json.loads((self.output / "result.json").read_text())
        self.assertEqual((result["commits"], result["ref"], result["branch"], result["exit_code"]), (2, "agent/" + JOB, "feature/x", 0))
        transcript = (self.output / "transcript.jsonl").read_text()
        self.assertNotIn(TOKEN, transcript)
        self.assertIn("[redacted]", transcript)
        self.assertIn("--permission-mode bypassPermissions", transcript)
        # The bundle carries the two agent commits on top of the submitted base.
        check = Path(self.temp.name) / "check"
        sh("git", "clone", "-q", "--branch", "feature/x", str(self.work / "in.bundle"), str(check))
        sh("git", "fetch", "-q", str(self.output / "result.bundle"), f"agent/{JOB}:refs/remotes/rtx/agent/{JOB}", cwd=check)
        log = subprocess.run(["git", "log", "--format=%s", f"refs/remotes/rtx/agent/{JOB}"], cwd=check, capture_output=True, text=True).stdout.split("\n")
        self.assertEqual(log[:3], [f"Changes left by the claude agent (job {JOB})", "first agent change", "base"])

    def test_codex_reads_its_own_home_and_needs_a_sign_in(self):
        with self.assertRaisesRegex(PermissionError, "Codex's sign-in did not arrive"):
            agent.run("codex", self.work, self.output, self.home)
        (self.home / ".codex").mkdir()
        (self.home / ".codex" / "auth.json").write_text("{}")
        (self.work / "repo").exists() and subprocess.run(["rm", "-rf", str(self.work / "repo")])
        self.assertEqual(agent.run("codex", self.work, self.output, self.home), 0)
        transcript = (self.output / "transcript.jsonl").read_text()
        self.assertIn(f'"codex_home":"{self.home / ".codex"}"', transcript)
        self.assertIn("--dangerously-bypass-approvals-and-sandbox", transcript)

    def test_claude_without_a_token_is_refused_before_it_starts(self):
        with self.assertRaisesRegex(PermissionError, "Claude Code's sign-in did not arrive"):
            agent.run("claude", self.work, self.output, self.home)
        self.assertFalse((self.output / "result.json").exists())

    def test_requests_are_checked(self):
        for spec in ({"v": 1, "branch": "../x", "job": JOB}, {"v": 1, "branch": "-x", "job": JOB}, {"v": 1, "branch": "x", "job": "nope"},
                     {"v": 2, "branch": "x", "job": JOB}, {"v": 1, "branch": "x", "job": JOB, "extra": 1}):
            (self.work / "agent.json").write_text(json.dumps(spec))
            with self.assertRaises(ValueError, msg=spec):
                agent.request(self.work)
        (self.work / "agent.json").write_text(json.dumps({"v": 1, "branch": "x", "job": JOB}))
        (self.work / "prompt.md").write_text("   ")
        with self.assertRaisesRegex(ValueError, "prompt"):
            agent.request(self.work)

    def test_the_command_line_takes_only_known_modes_and_tools(self):
        for argv in (["agent.py"], ["agent.py", "run", "bash"], ["agent.py", "login", "codex"], ["agent.py", "run", "claude", "x"]):
            self.assertEqual(agent.main(argv), 2, argv)


if __name__ == "__main__":
    unittest.main()
