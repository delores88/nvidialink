"""Agent jobs: only the fixed driver, only with internet, each tool's own sign-in."""
import json
import os
import stat
import unittest
from unittest import mock

import controller
import test_controller as fixtures
from test_controller import IMAGE

AGENT_IMAGE = "sha256:" + "d" * 64
TOKEN = "sk-ant-oat01-" + "x" * 40


def env_args(args):
    values = [value for index, value in enumerate(args) if index and args[index - 1] == "--env"]
    return dict(value.split("=", 1) for value in values)


def mounts(args):
    return [value for index, value in enumerate(args) if index and args[index - 1] == "--mount"]


class AgentControllerTests(unittest.TestCase):
    rpc = fixtures.ControllerTests.rpc
    snapshot = fixtures.ControllerTests.snapshot
    run_job = fixtures.ControllerTests.run_job
    tearDown = fixtures.ControllerTests.tearDown

    def setUp(self):
        fixtures.ControllerTests.setUp(self)
        (self.root.parent / "runtimes.json").write_text(json.dumps({"v": 1, "runtimes": {
            "python": {"image": IMAGE, "provenance": "fixture"}, "agent": {"image": AGENT_IMAGE, "provenance": "fixture"}}}))
        self.c.db.execute("DELETE FROM settings WHERE key='internet_policy'")
        for patch in (mock.patch.object(controller, "internet_deny", return_value=["192.168.0.0/24"]),
                      mock.patch.object(controller, "guard_bridge_ipv6")):
            patch.start()
            self.addCleanup(patch.stop)

    def agent(self, tool="claude", mode="run", request_id="agent-1", **kwargs):
        return self.run_job(argv=["python3", controller.AGENT_DRIVER, mode, tool], runtime="agent", request_id=request_id, **kwargs)

    def test_an_agent_job_gets_the_driver_its_tool_home_and_internet(self):
        job, _ = self.agent()
        self.assertEqual(job["internet"], "on")
        root = self.root / "jobs" / job["job"]
        self.assertEqual((root / "agent.py").read_bytes(), (controller.Path(controller.__file__).with_name("agent.py")).read_bytes())
        self.c.tick()
        args = next(call for call in self.docker.calls if call[0] == "create" and call[call.index("--name") + 1].startswith("sparkdev-job-"))
        home = self.root / "agents" / "claude"
        self.assertIn(f"type=bind,src={root / 'agent.py'},dst=/sparkdev-agent.py,readonly", mounts(args))
        self.assertIn(f"type=bind,src={home},dst=/sparkdev-agent-home", mounts(args))
        # Only this tool's home is mounted, and HOME points at it.
        self.assertFalse(any("agents/codex" in mount for mount in mounts(args)))
        self.assertEqual(env_args(args)["HOME"], "/sparkdev-agent-home")
        self.assertEqual(stat.S_IMODE(home.stat().st_mode), 0o700)
        self.assertIn("--cap-drop", args)
        self.assertIn("--read-only", args)

    def test_agent_jobs_run_only_the_fixed_driver(self):
        manifest = self.snapshot()
        for index, argv in enumerate((["bash", "-c", "cat /sparkdev-agent-home/claude-oauth-token"], ["python3", "/sparkdev-agent.py", "run", "bash"],
                     ["python3", "/sparkdev-agent.py", "login", "claude"], ["python3", "/sparkdev-agent.py", "run"])):
            result = self.c.rpc({"v": 1, "op": "run", "project": fixtures.PROJECT, "manifest_id": manifest, "argv": argv,
                                 "runtime": "agent", "client": "claude", "request_id": f"bad-{index}"})
            self.assertEqual(result["error"]["code"], "invalid_argv", argv)

    def test_agents_need_internet_and_are_never_reused(self):
        self.rpc("resources", internet=False)
        manifest = self.snapshot()
        result = self.c.rpc({"v": 1, "op": "run", "project": fixtures.PROJECT, "manifest_id": manifest, "argv": ["python3", controller.AGENT_DRIVER, "run", "codex"],
                             "runtime": "agent", "client": "codex", "request_id": "offline"})
        self.assertEqual(result["error"]["code"], "agent_needs_internet")
        self.rpc("resources", internet=True)
        first, _ = self.agent(tool="codex", request_id="first")
        self.c.db.execute("UPDATE jobs SET state='succeeded',exit_code=0,finished=1 WHERE id=?", (first["job"],))
        again, _ = self.agent(tool="codex", request_id="again")
        self.assertFalse(again["cache_hit"])

    def test_claude_token_is_saved_privately_and_never_echoed_or_audited(self):
        self.assertEqual(self.rpc("agent_status")["agents"], {"claude": {"signed_in": False}, "codex": {"signed_in": False}})
        for bad in ("short", "has space " + "x" * 30, "x" * 600, 42):
            response = self.c.rpc({"v": 1, "op": "agent_token", "tool": "claude", "token": bad})
            self.assertEqual(response["error"]["code"], "invalid_token" if isinstance(bad, str) else "invalid_token")
        self.assertEqual(self.c.rpc({"v": 1, "op": "agent_token", "tool": "codex", "token": TOKEN})["error"]["code"], "invalid_request")
        saved = self.rpc("agent_token", tool="claude", token=TOKEN + "\n")
        self.assertNotIn(TOKEN, json.dumps(saved))
        path = self.root / "agents" / "claude" / "claude-oauth-token"
        self.assertEqual(path.read_text(), TOKEN)
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        self.assertTrue(self.rpc("agent_status")["agents"]["claude"]["signed_in"])
        audit = " ".join(row[0] for row in self.c.db.execute("SELECT data FROM audit"))
        self.assertNotIn(TOKEN, audit)
        self.rpc("agent_signout", tool="claude")
        self.assertFalse(path.exists())
        self.assertFalse(self.rpc("agent_status")["agents"]["claude"]["signed_in"])

    def test_codex_signs_in_with_a_login_job(self):
        job, _ = self.agent(tool="codex", mode="login", request_id="codex-login")
        self.assertEqual(job["state"], "queued")


if __name__ == "__main__":
    unittest.main()
