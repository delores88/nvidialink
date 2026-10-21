"""Fixed tiny media checks; cached status never launches a workload.

Run --probe only inside the bounded installer container. Encoder enumeration is
reported separately from actual encode/decode results. No caller media is read.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

ENCODERS = {"h264_nvenc": ("h264", "h264_cuvid"), "hevc_nvenc": ("hevc", "hevc_cuvid"), "av1_nvenc": ("av1", "av1_cuvid")}


def command(argv, timeout=25):
    try:
        result = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout,
                                env={**os.environ, "OMP_NUM_THREADS": "1"}, check=False)
        return result.returncode, result.stdout[-131072:].decode(errors="replace"), result.stderr[-2048:].decode(errors="replace")
    except FileNotFoundError:
        return 127, "", "executable_unavailable"
    except subprocess.TimeoutExpired:
        return 124, "", "probe_timeout"


def probe_video():
    code, listing, error = command(["ffmpeg", "-hide_banner", "-encoders"])
    available = [name for name in ENCODERS if re.search(r"\b" + name + r"\b", listing)] if code == 0 else []
    result = {"v": 1, "observed_at": time.time(), "listed_encoders": available, "encoders": [],
              "nvenc": False if code == 0 else None, "nvdec": None, "tests": [], "state": "probed" if code == 0 else "unavailable"}
    if code:
        result["error"] = error
        return result
    with tempfile.TemporaryDirectory(prefix="sparkdev-video-") as directory:
        for encoder, (codec, decoder) in ENCODERS.items():
            if encoder not in available:
                continue
            target = Path(directory) / (codec + ".mkv")
            args = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi",
                    "-i", "color=c=blue:size=320x180:rate=10", "-frames:v", "10", "-an", "-threads", "1",
                    "-pix_fmt", "yuv420p", "-c:v", encoder, str(target)]
            exit_code, _, detail = command(args)
            test = {"encoder": encoder, "exit_code": exit_code, "encode": False, "decode": None}
            if exit_code == 0 and target.is_file() and 0 < target.stat().st_size <= 8 * 1024**2:
                inspect_code, raw, _ = command(["ffprobe", "-v", "error", "-count_frames", "-select_streams", "v:0",
                    "-show_entries", "stream=codec_name,nb_read_frames", "-of", "json", str(target)])
                try:
                    streams = json.loads(raw)["streams"]
                    test["encode"] = inspect_code == 0 and len(streams) == 1 and streams[0]["codec_name"] == codec and int(streams[0]["nb_read_frames"]) == 10
                except (ValueError, KeyError, TypeError):
                    pass
                if test["encode"]:
                    test.update(bytes=target.stat().st_size, sha256=hashlib.sha256(target.read_bytes()).hexdigest(), frames=10)
                    result["encoders"].append(encoder)
                    # The explicit CUVID decoder prevents a successful CPU fallback
                    # from being reported as an NVDEC success.
                    decode_code, _, decode_error = command(["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error",
                        "-hwaccel", "cuda", "-hwaccel_output_format", "cuda", "-c:v", decoder, "-i", str(target),
                        "-frames:v", "10", "-an", "-f", "null", "-"])
                    test.update(decode=decode_code == 0, decode_exit_code=decode_code)
                    if decode_code:
                        test["decode_error"] = decode_error
            if not test["encode"]:
                test["error"] = detail or "encoded_stream_verification_failed"
            result["tests"].append(test)
    result["nvenc"] = bool(result["encoders"])
    decoded = [item["decode"] for item in result["tests"] if item["decode"] is not None]
    result["nvdec"] = any(decoded) if decoded else None
    return result


BLENDER_PROGRAM = r'''
import bpy, ctypes, json
from pathlib import Path
assert bpy.app.version == (4, 5, 9), bpy.app.version_string
assert bpy.app.build_options.cycles
prefs = bpy.context.preferences.addons['cycles'].preferences
devices = {}
for backend in ('CUDA', 'OPTIX'):
    prefs.compute_device_type = backend
    devices[backend] = [entry[0] for entry in prefs.get_device_list(backend) if entry[1] == backend]
    assert devices[backend], backend + ' device unavailable'
kernel = Path('/opt/blender/4.5/scripts/addons_core/cycles/lib/kernel_sm_121.cubin.zst')
assert kernel.is_file(), 'GB10 CUDA kernel unavailable'
lib = ctypes.CDLL('/opt/blender/lib/libOpenImageDenoise.so.2')
lib.oidnIsCUDADeviceSupported.restype = ctypes.c_bool
lib.oidnIsCUDADeviceSupported.argtypes = [ctypes.c_int]
lib.oidnNewDevice.restype = ctypes.c_void_p
lib.oidnNewDevice.argtypes = [ctypes.c_int]
lib.oidnReleaseDevice.argtypes = [ctypes.c_void_p]
assert lib.oidnIsCUDADeviceSupported(0), 'OIDN CUDA device unavailable'
device = lib.oidnNewDevice(3)
assert device, 'OIDN CUDA device creation failed'
lib.oidnReleaseDevice(device)
print('SPARKDEV_BLENDER ' + json.dumps({'version': bpy.app.version_string, 'devices': devices,
    'sm_121_kernel': True, 'oidn_cuda_device_created': True, 'render_tested': False}))
'''


def probe_blender():
    code, stdout, _ = command(["blender", "--background", "--factory-startup", "--python-exit-code", "3",
                               "--python-expr", BLENDER_PROGRAM], timeout=60)
    lines = [line[len("SPARKDEV_BLENDER "):] for line in stdout.splitlines() if line.startswith("SPARKDEV_BLENDER ")]
    if code or len(lines) != 1:
        raise ValueError("Blender version, GPU devices or OIDN activation probe failed")
    return json.loads(lines[0])


def cached_gpu_video(root, image):
    """Only report the saved probe bound to the currently registered image."""
    unknown = {"state": "unknown", "nvenc": None, "nvdec": None, "encoders": [], "listed_encoders": []}
    if not isinstance(image, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", image):
        return unknown
    try:
        path = Path(root) / "runtime-cuda-build.json"
        if path.is_symlink() or path.stat().st_size > 1024**2:
            return unknown
        receipt = json.loads(path.read_text())
        video = receipt["probe"]["gpu_video"]
        if receipt.get("state") != "succeeded" or receipt.get("image") != image or video.get("v") != 1:
            return unknown
        if video.get("state") not in ("probed", "unavailable") or any(video.get(k) is not None and type(video.get(k)) is not bool for k in ("nvenc", "nvdec")):
            return unknown
        for key in ("encoders", "listed_encoders"):
            if not isinstance(video.get(key), list) or len(video[key]) > 3 or any(name not in ENCODERS for name in video[key]):
                return unknown
        if video["nvenc"] is True and not video["encoders"]:
            return unknown
        return {key: video.get(key) for key in ("state", "nvenc", "nvdec", "encoders", "listed_encoders", "observed_at")} | {"image": image, "cached": True}
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        return unknown


if __name__ == "__main__":
    import sys
    if sys.argv[1:] != ["--probe"]:
        raise SystemExit("Only the fixed --probe operation is supported")
    print(json.dumps({"gpu_video": probe_video(), "blender": probe_blender()}))
