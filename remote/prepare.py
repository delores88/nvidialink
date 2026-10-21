#!/usr/bin/env python3
"""Fixed lockfile-enforcing dependency commands inside an isolated Docker job.

No package code runs while this container has network access. pip installs wheels
only, runs isolated (-I keeps /work off sys.path) with a fixed environment, every lock
line is checked before pip reads it, and every CUDA check reads metadata or the
filesystem without importing staged code.
"""
import codecs
import hashlib
import importlib.machinery
import importlib.metadata
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys

# Module constants so the tests can redirect them on any platform.
WORK = Path("/work")
ENVS = Path("/pip-cache/envs")
TMP = Path("/pip-cache/.tmp")
DEV = Path("/dev")
LOCKS = {"python": "requirements.lock", "cuda": "requirements-cuda.lock"}
# torch and the CUDA stack come only from the pinned image, never from PyPI.
DENY = frozenset({"torch", "torchvision", "torchaudio", "triton", "pytorch-triton", "cuda-toolkit"})
# Every nvidia-* distribution is reserved: CUDA 13 wheels (nvidia-cublas, nvidia-cuda-runtime)
# carry no -cuNN suffix. Only these non-CUDA-library bindings are allowed.
NVIDIA_ALLOW = frozenset({"nvidia-ml-py"})
STARTUP = frozenset({"sitecustomize", "usercustomize"})
SUFFIXES = tuple(importlib.machinery.all_suffixes())
NAME = re.compile(r"[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?")
COMMENT = re.compile(r"(^|\s+)#.*$")
HASH = re.compile(r"--hash=(?:sha256|sha384|sha512):[0-9A-Fa-f]+")
ARCHIVES = (".whl", ".zip", ".tar", ".tgz", ".tbz", ".txz", ".tlz", ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.lz", ".tar.lzma")
EXTRAS = re.compile(r"\[[^\]]*\]$")
# pip's PEP 263 cookie rule for requirements files: a '#' line among the first two.
COOKIE = re.compile(rb"coding[:=]\s*([-\w.]+)")
GENERIC = "sparkdev: dependency preparation requires supported, hash-locked manifests"
CUDA_LOCK = ("sparkdev: cuda preparation requires requirements-cuda.lock: hash-locked, no-deps closure "
             "excluding packages the CUDA image provides")


class MissingLock(ValueError):
    """No lockfile of the required name exists below the project root."""


class Refusal(ValueError):
    """A refusal whose message is fixed by this program and safe to print."""


def say(line):
    print(line, flush=True)


def normalize(name):
    """PEP 503 project name normalization."""
    return re.sub(r"[-_.]+", "-", name).lower()


def reserved(name):
    # Separator-insensitive, so torch-vision or Torch_Vision cannot slip past torchvision.
    squashed = normalize(name).replace("-", "")
    if squashed in {d.replace("-", "") for d in DENY}:
        return True
    return squashed.startswith("nvidia") and squashed not in {a.replace("-", "") for a in NVIDIA_ALLOW}


def linked(path):
    return os.path.islink(path)


def remove(path):
    path = Path(path)
    if linked(path) or (path.exists() and not path.is_dir()):
        path.unlink()
    elif path.is_dir():
        shutil.rmtree(path)


def files(root, name):
    result = []
    for directory, names, filenames in os.walk(root, followlinks=False):
        names[:] = [n for n in names if n not in {".git", "target", "node_modules", ".venv", "build", "dist"}]
        if name in filenames:
            path = Path(directory) / name
            if path.is_symlink():
                raise ValueError("symlink lockfile refused")
            result.append(path)
    if not result:
        raise MissingLock("no lockfile")
    if len(result) > 32:
        raise ValueError("one to thirty-two lockfiles required")
    return sorted(result)


def commands(runtime, root):
    if runtime == "rust":
        return [(lock.parent, ["cargo", "fetch", "--locked", "--manifest-path", str(lock.parent / "Cargo.toml")], lock)
                for lock in files(root, "Cargo.lock") if (lock.parent / "Cargo.toml").is_file()]
    if runtime == "node":
        return [(lock.parent, ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"], lock)
                for lock in files(root, "package-lock.json") if (lock.parent / "package.json").is_file()]
    if runtime in LOCKS:
        python, extra = "python3", []
        if runtime == "cuda":
            # Checks use this interpreter's sys.path as the image's view, so pip and the
            # job's python3 must be the very same interpreter.
            found = shutil.which("python3")
            if not found or os.path.realpath(found) != os.path.realpath(sys.executable):
                raise Refusal("python3 on PATH is not the CUDA image interpreter")
            python, extra = sys.executable, ["--no-deps"]
        # -I keeps the lockfile directory (/work) off sys.path while the network is attached.
        # UTF-8 mode makes pip decode a cookie-less lock exactly as precheck() does, whatever the locale.
        return [(lock.parent, [python, "-I", "-X", "utf8", "-m", "pip", "install", "--require-hashes", "--only-binary=:all:", *extra,
                               "--target", str(ENVS / (".pending-" + hashlib.sha256(lock.read_bytes()).hexdigest())),
                               "-r", str(lock)], lock)
                for lock in files(root, LOCKS[runtime])]
    raise ValueError("runtime has no dependency preparation")


def pip_env():
    env = {k: v for k, v in os.environ.items() if not k.upper().startswith(("PIP_", "PYTHON"))}
    env.update(PIP_CONFIG_FILE=os.devnull, PIP_CACHE_DIR="/prepare-cache/pip", PIP_INDEX_URL="https://pypi.org/simple",
               PIP_DISABLE_PIP_VERSION_CHECK="1", PIP_NO_INPUT="1", TMPDIR=str(TMP))
    return env


def image_path():
    # sys.path[0] is the directory of /sparkdev-prepare.py, i.e. "/": never the image's packages.
    # An image PYTHONSAFEPATH omits that entry, and then every entry belongs to the image.
    return list(sys.path) if sys.flags.safe_path else sys.path[1:]


def dist_name(dist):
    try:
        name = dist.metadata["Name"]
    except (OSError, ValueError, TypeError, KeyError):
        return None
    return normalize(name) if isinstance(name, str) and NAME.fullmatch(name) else None


def versions(path):
    """Normalized distribution name to version; the first entry on the path wins, as for imports."""
    found = {}
    for dist in importlib.metadata.distributions(path=[str(p) for p in path]):
        name = dist_name(dist)
        if name and name not in found:
            try:
                found[name] = dist.version or ""
            except (OSError, ValueError, TypeError, KeyError):
                found[name] = ""
    return found


def logical_lines(text):
    """pip's requirements-file preprocessing: join continuations, then strip comments."""
    joined, number, parts = [], None, []
    for index, line in enumerate(text.splitlines(), 1):
        if not line.endswith("\\") or COMMENT.match(line):
            if COMMENT.match(line):
                line = " " + line
            if parts:
                parts.append(line)
                joined.append((number, "".join(parts)))
                parts = []
            else:
                joined.append((index, line))
        else:
            if not parts:
                number = index
            parts.append(line.strip("\\"))
    if parts:
        joined.append((number, "".join(parts)))
    for index, line in joined:
        line = COMMENT.sub("", line).strip()
        if line:
            yield index, line


def lock_name(line):
    """The normalized project name of a supported hash-locked line, or None."""
    if line.startswith("-") or "@" in line:
        return None
    tokens = line.split()
    options = next((i for i, t in enumerate(tokens) if t.startswith("-")), len(tokens))
    rest = tokens[options:]
    while rest:  # Only per-requirement hashes, as --hash=ALG:HEX or --hash ALG:HEX.
        if HASH.fullmatch(rest[0]):
            rest = rest[1:]
        elif rest[0] == "--hash" and len(rest) > 1 and HASH.fullmatch("--hash=" + rest[1]):
            rest = rest[2:]
        else:
            return None
    requirement = " ".join(tokens[:options])
    # pip reads a requirement holding a path separator, or whose text before any marker
    # ends in an archive suffix once trailing extras are stripped (`evil==1.0.tar.gz`,
    # `x.whl[a]`), as a local file or directory, and builds an sdist from it.
    if "/" in requirement or "\\" in requirement:
        return None
    target = requirement.split(";", 1)[0].strip()
    while True:
        if target.lower().endswith(ARCHIVES):
            return None
        stripped = EXTRAS.sub("", target).rstrip()
        if stripped == target:
            break
        target = stripped
    match = NAME.match(requirement)
    if not match:
        return None
    tail = requirement[match.end():]
    if tail and tail[0] not in " [(<>=!~;":
        return None
    return normalize(match.group())


def utf8_cookie(value):
    try:
        return codecs.lookup(value.decode("ascii")).name in ("utf-8", "utf-8-sig")
    except (LookupError, UnicodeDecodeError):
        return False


def precheck(raw, provided, name=LOCKS["cuda"], cuda=True):
    """Refuse a lock before any download; one message per hit. Package-name rules apply to CUDA only."""
    try:
        text = raw.decode("utf-8-sig")
    except UnicodeDecodeError:
        return ["sparkdev: refused: " + name + " is not UTF-8"]
    for line in raw.split(b"\n")[:2]:
        cookie = COOKIE.search(line) if line[:1] == b"#" else None
        if cookie and not utf8_cookie(cookie.group(1)):
            # pip would decode the whole file with that codec and read lines this check never saw.
            return ["sparkdev: refused: " + name + " declares a coding other than UTF-8"]
    hits = []
    for number, line in logical_lines(text):
        project = lock_name(line)
        if project is None:
            hits.append("sparkdev: refused: unsupported lock line " + str(number))
        elif cuda and reserved(project):
            hits.append("sparkdev: refused: " + project + " is reserved for the CUDA image")
        elif cuda and project in provided:
            hits.append("sparkdev: refused: " + project + " is provided by the CUDA image")
    return hits


def regular_package(directory):
    return any((directory / ("__init__" + suffix)).is_file() for suffix in SUFFIXES)


def modules(directory, top):
    for entry in sorted(directory.iterdir(), key=lambda p: p.name):
        name = entry.name
        if name.endswith((".dist-info", ".data", ".pth")) or name == "__pycache__" or (top and name == "bin"):
            continue
        if entry.is_dir():
            module = name
        elif name.endswith(SUFFIXES):
            module = name.split(".", 1)[0]
        else:
            continue
        if module.isidentifier():
            yield entry, module


def shadows(directory, prefix, search):
    hits = []
    for entry, module in modules(directory, prefix is None):
        fullname = module if prefix is None else prefix + "." + module
        if prefix is None and module in STARTUP:
            hits.append("sparkdev: refused: " + module + " would run at every CUDA interpreter startup")
            continue
        if prefix is None and module == "nvidia":
            # PyPI CUDA libraries live in the nvidia namespace, whatever distribution ships them.
            hits.append("sparkdev: refused: nvidia is reserved for the CUDA image")
            continue
        if prefix is None and (module in sys.stdlib_module_names or module in sys.builtin_module_names):
            hits.append("sparkdev: refused: " + module + " shadows the Python standard library")
            continue
        spec = importlib.machinery.PathFinder.find_spec(fullname, search)
        if spec is None:
            continue
        if entry.is_dir() and not regular_package(entry) and spec.loader is None and spec.submodule_search_locations is not None:
            # Both sides are namespace portions: they merge, so compare their children instead.
            portions = [os.path.join(p, module) for p in search if os.path.isdir(os.path.join(p, module))]
            hits += shadows(entry, fullname, portions)
            continue
        hits.append("sparkdev: refused: " + fullname + " shadows a module the CUDA image provides")
    return hits


def overlap(staging, image):
    """Refuse anything staged that would duplicate or shadow the image on PYTHONPATH."""
    hits, staged = [], set()
    for dist in importlib.metadata.distributions(path=[str(staging)]):
        name = dist_name(dist)
        if name is None:
            hits.append("sparkdev: refused: staged distribution metadata is unreadable")
        else:
            staged.add(name)
    provided = set(versions(image))
    for name in sorted(staged):
        if reserved(name):
            hits.append("sparkdev: refused: " + name + " is reserved for the CUDA image")
        elif name in provided:
            hits.append("sparkdev: refused: " + name + " is provided by the CUDA image")
    return hits + shadows(Path(staging), None, list(image))


def satisfied(requirement, version):
    try:
        return requirement.specifier.contains(version, prereleases=True)
    except (TypeError, ValueError):  # An unparseable version never satisfies.
        return False


def unmet(staging, image):
    """Metadata-only dependency check. Returns (refusals, warnings)."""
    from pip._vendor.packaging.requirements import InvalidRequirement, Requirement
    staged, provided = versions([staging]), versions(image)
    refusals, warnings = [], []
    for dist in importlib.metadata.distributions(path=[str(staging)]):
        owner = dist_name(dist)
        if owner is None:
            continue  # overlap() already refuses it.
        for line in dist.requires or ():
            try:
                requirement = Requirement(line)
            except InvalidRequirement:
                refusals.append("sparkdev: refused: " + owner + " has an unreadable requirement")
                continue
            try:
                # A valid marker can still fail to evaluate here, for example `platform_release >= "5"`
                # against "6.8.0-1015-nvidia" (InvalidVersion) or `~=` with a non-version (UndefinedComparison).
                if requirement.marker is not None and not requirement.marker.evaluate({"extra": ""}):
                    continue
            except (ValueError, TypeError):
                refusals.append("sparkdev: refused: " + owner + " has an unevaluable requirement marker")
                continue
            name = normalize(requirement.name)
            if name in staged:
                if not satisfied(requirement, staged[name]):
                    refusals.append("sparkdev: refused: " + owner + " requires " + str(requirement) +
                                    "; requirements-cuda.lock has " + staged[name])
            elif name in provided:
                if not satisfied(requirement, provided[name]):
                    warnings.append("sparkdev: warning: " + owner + " requires " + str(requirement) +
                                    "; CUDA image has " + provided[name])
            else:
                refusals.append("sparkdev: refused: " + owner + " requires " + str(requirement) +
                                ", missing from requirements-cuda.lock")
    return refusals, warnings


def providers(staging):
    """Relative path parts -> {(distribution, version)} whose RECORD lists a file at or below them."""
    owners = {}
    for dist in importlib.metadata.distributions(path=[str(staging)]):
        name = dist_name(dist)
        try:
            version, files = dist.version or "", dist.files or ()
        except (OSError, ValueError, TypeError, KeyError):
            continue
        if name is None:
            continue
        for file in files:
            parts = PurePosixPath(str(file)).parts
            for depth in range(1, len(parts) + 1):
                owners.setdefault(parts[:depth], set()).add((name, version))
    return owners


def clashes(here, there, prefix, mine, theirs, label, other):
    hits = []
    for entry, module in modules(here.joinpath(*prefix), not prefix):
        fullname = ".".join(prefix + (module,))
        spec = importlib.machinery.PathFinder.find_spec(fullname, [str(there.joinpath(*prefix))])
        if spec is None:
            continue
        if entry.is_dir() and not regular_package(entry) and spec.loader is None and spec.submodule_search_locations is not None:
            # Both sides are namespace portions: they merge, so compare their children.
            hits += clashes(here, there, prefix + (module,), mine, theirs, label, other)
            continue
        # A package is keyed by its directory, a module by its file.
        found = module if spec.submodule_search_locations is not None else Path(spec.origin or "").name
        owner = mine.get(prefix + (entry.name,))
        if not owner or owner != theirs.get(prefix + (found,)):
            hits.append("sparkdev: refused: " + fullname + " in " + label + " shadows the same module in " + other)
    return hits


def collisions(staging, label, earlier):
    """Refuse a lock whose env differs from an env this run prepared earlier.

    Every prepared env of a runtime goes on one PYTHONPATH, where the first one wins an
    import, so a distribution or module may repeat across locks only as an identical copy.
    """
    hits, mine, owned = [], versions([staging]), providers(staging)
    for other, env in earlier:
        theirs = versions([env])
        for name in sorted(mine.keys() & theirs.keys()):
            if mine[name] != theirs[name]:
                hits.append("sparkdev: refused: " + label + " pins " + name + " " + mine[name] + "; " + other + " pins " +
                            theirs[name])
        hits += clashes(Path(staging), Path(env), (), owned, providers(env), label, other)
    return hits


def checks(staging, image):
    hits = overlap(staging, image)
    try:
        refusals, warnings = unmet(staging, image)
    except ImportError:
        refusals, warnings = ["sparkdev: refused: requirements cannot be checked without pip"], []
    return hits + refusals, warnings


def publish(pending, final, old):
    """Replace final without POSIX rename-over semantics; the final path never changes."""
    if linked(final):
        say("sparkdev: refused: prepared environment path is a symlink")
        return False
    remove(old)
    if final.exists():
        os.rename(final, old)
    os.rename(pending, final)
    remove(old)
    return True


def main():
    if len(sys.argv) != 2:
        return 64
    runtime = sys.argv[1]
    if runtime == "cuda" and any(DEV.glob("nvidia*")):
        say("sparkdev: refused: GPU device visible during preparation")
        return 65
    try:
        actions = commands(runtime, WORK)
        if not actions:
            raise ValueError("no matching manifest")
    except MissingLock:
        # System-only projects have no language lock. The controller sets this
        # marker only after selecting a verified derived image for this lock.
        if os.environ.get("SPARKDEV_SYSTEM_PREPARED") == "1" and (WORK / "sparkdev-system.lock").is_file():
            say("sparkdev: locked system packages are prepared in the pinned project image")
            return 0
        say(CUDA_LOCK if runtime == "cuda" else GENERIC)
        return 65
    except Refusal as error:
        say("sparkdev: refused: " + str(error))
        return 65
    except ValueError:
        say(GENERIC)
        return 65
    if runtime in LOCKS:
        if linked(ENVS):
            say("sparkdev: refused: prepared environment root is a symlink")
            return 65
        ENVS.mkdir(parents=True, exist_ok=True)
        for entry in ENVS.iterdir():
            if entry.name.startswith((".pending-", ".old-")):
                remove(entry)
        remove(TMP)
    image = image_path() if runtime == "cuda" else []
    provided = set(versions(image)) if runtime == "cuda" else set()
    published = []  # (lock, env) prepared earlier in this run; jobs load them all, in this order.
    for cwd, argv, lock in actions:
        raw = lock.read_bytes()
        original = hashlib.sha256(raw).hexdigest()
        label = lock.relative_to(WORK).as_posix()
        say("sparkdev: preparing " + label)
        pending = Path(argv[argv.index("--target") + 1]) if runtime in LOCKS else None
        if pending is not None and pending.name != ".pending-" + original:
            say("sparkdev: lockfile changed; preparation refused")
            return 65
        if runtime in LOCKS:
            # Both pip runtimes: no option, URL, path or local-archive line ever reaches pip.
            hits = precheck(raw, provided, LOCKS[runtime], cuda=runtime == "cuda")
            for hit in hits:
                say(hit)
            if hits:
                return 65
        try:
            if pending is not None:
                remove(pending)
                remove(TMP)
                TMP.mkdir(parents=True)
            env = pip_env() if pending is not None else None
            result = subprocess.run(argv, cwd=cwd, stdin=subprocess.DEVNULL, env=env, check=False)
            if hashlib.sha256(lock.read_bytes()).hexdigest() != original:
                say("sparkdev: lockfile changed; preparation refused")
                return 65
            if result.returncode:
                return result.returncode
            if pending is not None:
                pending.mkdir(exist_ok=True)  # An empty lock installs nothing but still prepares.
            if runtime == "cuda":
                hits, warnings = checks(pending, image)
                for line in warnings + hits:
                    say(line)
                if hits:
                    return 65
            if pending is not None:
                hits = collisions(pending, label, published)
                for hit in hits:
                    say(hit)
                if hits:
                    return 65
            if pending is not None and not publish(pending, ENVS / original, ENVS / (".old-" + original)):
                return 65
            if pending is not None:
                published.append((label, ENVS / original))
            if runtime == "node":
                tree = Path("/npm-cache/trees") / original
                tree.parent.mkdir(parents=True, exist_ok=True)
                if tree.is_symlink():
                    tree.unlink()
                elif tree.exists():
                    shutil.rmtree(tree)
                shutil.copytree(cwd / "node_modules", tree, symlinks=True)
        finally:
            if pending is not None:
                remove(pending)
                remove(TMP)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
