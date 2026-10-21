"""Owner-run physical acceptance of CUDA dependency preparation through the installed CLI.

Only the sparkdev CLI touches the Spark: this script never runs ssh and never reads or
changes owner state. The laptop itself reads public PyPI JSON to pick wheels.
Pass the client that actually runs it (an owner at a terminal is --client human).

Before the first run the owner records, outside this script, the CUDA image's
Config.Env, `docker info` (default runtime) and the image's `pip --version`; the
listing step below repeats the in-container part and saves it in the receipt.
"""
import argparse
import json
from pathlib import Path
import re
import subprocess
import tempfile
import time
import urllib.request
import uuid

# (distribution, top-level module) candidates; the first the image lacks is used.
PURE = (("tomli-w", "tomli_w"), ("pyfiglet", "pyfiglet"), ("termcolor", "termcolor"), ("humanize", "humanize"))
NATIVE = (("xxhash", "xxhash"), ("ujson", "ujson"), ("mmh3", "mmh3"), ("bitarray", "bitarray"))
NEEDS_DEPENDENCY = ("python-slugify", "coloredlogs", "rich-click")
UNUSED_HASH = " --hash=sha256:" + "0" * 64  # Never fetched: these locks must be refused first.
TERMINAL = ("succeeded", "failed", "cancelled")

LISTING = r"""import importlib.metadata, json, os, pkgutil, re, subprocess, sys
names = {}
for dist in importlib.metadata.distributions():
    name = dist.metadata["Name"]
    if name:
        names.setdefault(re.sub(r"[-_.]+", "-", name).lower(), dist.version)
pip = subprocess.run([sys.executable, "-I", "-m", "pip", "--version"], capture_output=True, text=True)
print("SPARKDEV-LISTING " + json.dumps({
    "python": sys.version, "executable": sys.executable, "pip": pip.stdout.strip() or pip.stderr.strip(),
    "env": {k: v for k, v in sorted(os.environ.items()) if k.startswith(("PIP_", "PYTHON", "NVIDIA_", "CUDA"))},
    "distributions": names, "modules": sorted({m.name for m in pkgutil.iter_modules()})}), flush=True)
"""

PROBE = r"""import importlib, json, torch
modules = {name: importlib.import_module(name).__file__ for name in MODULES}
print("SPARKDEV-PROBE " + json.dumps({"modules": modules, "torch": torch.__file__, "torch_version": torch.__version__,
                                       "cuda": torch.cuda.is_available()}), flush=True)
"""


def normalize(name):
    return re.sub(r"[-_.]+", "-", name).lower()


def pypi(name):
    with urllib.request.urlopen("https://pypi.org/pypi/" + name + "/json", timeout=20) as response:
        return json.load(response)


def hard_requirements(info):
    """Requires-Dist entries that apply without an extra (conservatively: any marker but extra)."""
    result = []
    for value in info.get("requires_dist") or ():
        marker = value.split(";", 1)[1] if ";" in value else ""
        if "extra" not in marker:
            result.append(normalize(re.match(r"[A-Za-z0-9][A-Za-z0-9._-]*", value.strip()).group()))
    return result


def wheel(name, predicate):
    data = pypi(name)
    files = [f for f in data["urls"] if f["packagetype"] == "bdist_wheel" and predicate(f["filename"]) and not f.get("yanked")]
    if not files:
        return None
    chosen = sorted(files, key=lambda f: f["filename"])[0]
    return {"name": normalize(data["info"]["name"]), "version": data["info"]["version"], "filename": chosen["filename"],
            "sha256": chosen["digests"]["sha256"], "requires": hard_requirements(data["info"])}


def pure_wheel(filename):
    return filename.endswith(("-py3-none-any.whl", "-py2.py3-none-any.whl"))


def native_wheel(filename):
    return "-cp312-cp312-" in filename and "manylinux" in filename and filename.endswith("aarch64.whl")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--client", required=True, choices=("claude", "codex", "human"))
    parser.add_argument("--cli", required=True)
    parser.add_argument("--receipt", required=True, type=Path)
    parser.add_argument("--workdir", type=Path)
    args = parser.parse_args()
    run_id = uuid.uuid4().hex
    root = Path(tempfile.mkdtemp(prefix="sparkdev-cuda-acceptance-", dir=args.workdir))
    receipt = {"kind": "physical_cuda_dependency_preparation", "actual_caller": args.client, "run_id": run_id,
               "started_at": time.time(), "evidence": "real Spark jobs submitted through the installed CLI", "checks": []}

    def cli(*values, timeout=120):
        result = subprocess.run([args.cli, "--json", "--client", args.client, *map(str, values)], capture_output=True, timeout=timeout)
        answer = json.loads(result.stdout)
        if result.returncode or answer.get("ok") is False:
            raise RuntimeError(str(answer))
        return answer

    def save():
        args.receipt.parent.mkdir(parents=True, exist_ok=True)
        args.receipt.write_text(json.dumps(receipt, indent=2))

    def complete(job, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            status = cli("wait", job, "--timeout", 20)
            if status["state"] in TERMINAL:
                text, offset = "", 0
                while True:
                    chunk = cli("logs", job, "--since", offset)
                    text += chunk["text"]
                    if chunk["next_offset"] == offset:
                        return status, text
                    offset = chunk["next_offset"]
        raise RuntimeError("Job did not complete: " + job)

    def project(name, files):
        directory = root / name
        directory.mkdir()
        for filename, text in files.items():
            (directory / filename).write_text(text, newline="\n")
        return directory

    def record(name, status, text, **extra):
        receipt["checks"].append({"name": name, "job": status["job"], "state": status["state"], "exit_code": status["exit_code"],
                                  "log": text[-20000:], **extra})
        save()

    def prepare(name, directory):
        job = cli("prepare", directory, "--runtime", "cuda", "--request-id", run_id + "-prepare-" + name)
        status, text = complete(job["job"], 3600)
        record("prepare_" + name, status, text)
        return status, text

    def refused_before_download(name, directory, expected):
        status, text = prepare(name, directory)
        assert (status["state"], status["exit_code"]) == ("failed", 65), status
        assert expected in text, text
        assert "Collecting" not in text and "Downloading" not in text, text

    try:
        listing_dir = project("listing", {"listing.py": LISTING})
        job = cli("run", listing_dir, "--runtime", "cuda", "--where", "spark", "--detach", "--no-cache",
                  "--request-id", run_id + "-listing", "--", "python3", "listing.py")
        status, text = complete(job["job"], 900)
        assert status["exit_code"] == 0, status
        listing = json.loads(next(line for line in text.splitlines() if line.startswith("SPARKDEV-LISTING "))[17:])
        image, modules = listing["distributions"], set(listing["modules"])
        record("image_listing", status, "", python=listing["python"], executable=listing["executable"], pip=listing["pip"],
               env=listing["env"], distribution_count=len(image), distributions=image)
        assert "torch" in image, "CUDA image has no torch distribution"

        chosen = []
        for candidates, predicate in ((PURE, pure_wheel), (NATIVE, native_wheel)):
            for dist, module in candidates:
                if normalize(dist) in image or module in modules:
                    continue
                found = wheel(dist, predicate)
                if found and not found["requires"]:
                    chosen.append({**found, "module": module})
                    break
            else:
                raise RuntimeError("No dependency-free candidate wheel is absent from the CUDA image")
        receipt["wheels"] = chosen
        save()

        lock = "".join(f"{w['name']}=={w['version']} --hash=sha256:{w['sha256']}\n" for w in chosen)
        probe = PROBE.replace("MODULES", json.dumps([w["module"] for w in chosen]))
        accepted = project("accepted", {"requirements-cuda.lock": lock, "probe.py": probe})
        status, text = prepare("accepted", accepted)
        assert status["exit_code"] == 0, status
        job = cli("run", accepted, "--runtime", "cuda", "--where", "spark", "--detach", "--no-cache",
                  "--request-id", run_id + "-probe", "--", "python3", "probe.py")
        status, text = complete(job["job"], 900)
        record("offline_cuda_probe", status, text)
        assert status["exit_code"] == 0, status
        result = json.loads(next(line for line in text.splitlines() if line.startswith("SPARKDEV-PROBE "))[15:])
        assert all(path.startswith("/pip-cache/envs/") for path in result["modules"].values()), result
        assert not result["torch"].startswith("/pip-cache/"), result
        assert result["cuda"] is True, result

        numpy = image.get("numpy")
        assert numpy, "CUDA image has no numpy distribution for the overlap refusal"
        refused_before_download("numpy", project("numpy", {"requirements-cuda.lock": f"numpy=={numpy}{UNUSED_HASH}\n"}),
                                "sparkdev: refused: numpy is provided by the CUDA image")
        refused_before_download("torch", project("torch", {"requirements-cuda.lock": f"torch=={image['torch']}{UNUSED_HASH}\n"}),
                                "sparkdev: refused: torch is reserved for the CUDA image")

        for dist in NEEDS_DEPENDENCY:
            found = None if normalize(dist) in image else wheel(dist, pure_wheel)
            if found and any(r not in image for r in found["requires"]):
                break
        else:
            raise RuntimeError("No candidate wheel has a dependency the CUDA image lacks")
        receipt["missing_dependency_wheel"] = found
        lock = f"{found['name']}=={found['version']} --hash=sha256:{found['sha256']}\n"
        status, text = prepare("missing_dependency", project("missing", {"requirements-cuda.lock": lock}))
        assert (status["state"], status["exit_code"]) == ("failed", 65), status
        assert "missing from requirements-cuda.lock" in text, text
        receipt["ok"] = True
    except Exception as error:
        receipt["ok"] = False
        receipt["error"] = str(error)
        raise
    finally:
        receipt["finished_at"] = time.time()
        save()
    print(json.dumps({"ok": True, "checks": len(receipt["checks"]), "receipt": str(args.receipt)}))


if __name__ == "__main__":
    main()
