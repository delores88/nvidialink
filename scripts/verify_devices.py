"""Check release discovery/plan honesty with isolated synthetic configuration."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    receipt = {"schema": 1, "binary_sha256": hashlib.sha256(Path(args.binary).read_bytes()).hexdigest(), "checks": []}
    with tempfile.TemporaryDirectory(prefix="sparkdev-discovery-") as temporary:
        root = Path(temporary)
        source = root / "source"
        source.mkdir()
        (source / "probe.py").write_text("print('hello')\n")
        state = root / "state"
        env = dict(os.environ, SPARKDEV_HOME=str(state))
        def call(*words):
            result = subprocess.run([args.binary, "--json", *map(str, words)], env=env, text=True, capture_output=True, timeout=40)
            assert result.returncode == 0, result.stdout + result.stderr
            return json.loads(result.stdout)
        planned = call("plan", source, "--", "python3", "probe.py")
        assert not state.exists(), "Planning unexpectedly created application state"
        assert planned["placement"] == "spark" and not planned["submitted"] and not planned["claimed"]
        receipt["checks"].append({"name": "offline_plan_without_state_upload_or_claim", "passed": True})
        devices = call("devices")
        laptop, spark = devices["devices"]
        assert laptop["availability"] == "available" and laptop["can_submit"] is False
        assert spark["availability"] == "unconfigured" and spark["transport"]["route"] == "none"
        assert spark["network"] is None, "An unconfigured Spark must not report a network link"
        assert laptop["admission"]["available"] == "unavailable"
        receipt["checks"].append({"name": "unconfigured_spark_does_not_imply_offline_laptop_admission", "passed": True})
        # Refused loopback port, without reading/copying an owner key or changing
        # known_hosts. This proves classification, not a physical LAN outage.
        call("setup", "--ssh-alias", "sparkdev-loopback-probe", "--host", "127.0.0.1", "--host-key-alias", "sparkdev-loopback-probe", "--port", "1")
        devices = call("devices")
        laptop, spark = devices["devices"]
        assert spark["availability"] == "unreachable" and spark["transport"]["authenticated"] is False
        assert spark["network"] is None, "An unreachable Spark must not report a network link"
        assert spark["transport"]["route"] == "none"
        attempts = spark["transport"]["attempted_routes"]
        assert any(a["route"] == "lan_ssh" and a["status"] == "host_unreachable" for a in attempts)
        assert any(a["route"] == "paired_iroh" and not a["attempted"] and a["status"] == "disabled" for a in attempts)
        assert laptop["availability"] == "available" and laptop["can_submit"] is False
        receipt["checks"].append({"name": "refused_ssh_route_and_disabled_remote_reported_separately", "passed": True, "attempted_routes": attempts})
    receipt["status"] = "passed"
    receipt["limitations"] = ["Synthetic loopback refusal is not physical LAN drop or hotspot acceptance", "SSH authentication failure classification has unit coverage; this test does not alter owner host pins"]
    Path(args.output).write_text(json.dumps(receipt, indent=2) + "\n")
    print("Device and planning release checks passed", flush=True)


if __name__ == "__main__":
    main()
