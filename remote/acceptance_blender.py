"""Fixed ten-frame physical acceptance, run by an admitted CUDA job's Blender.

blender -b --factory-startup --debug-cycles --verbose 3 --threads 2
  --python-exit-code 3 --python acceptance_blender.py -- CUDA /sparkdev-output
Use OPTIX or CPU in separate worktrees/jobs. The receipt records requested OIDN
selection; Cycles' retained debug log is evidence of the actual denoising device.
"""
import hashlib
import json
from pathlib import Path
import sys
import time

import bpy


def main():
    args = sys.argv[sys.argv.index("--") + 1:]
    if len(args) != 2 or args[0] not in ("OPTIX", "CUDA", "CPU"):
        raise ValueError("Expected OPTIX|CUDA|CPU and output directory")
    backend, destination = args[0], Path(args[1])
    destination.mkdir(parents=True, exist_ok=True)
    assert bpy.app.version == (4, 5, 9), bpy.app.version_string
    scene = bpy.context.scene
    prefs = bpy.context.preferences.addons["cycles"].preferences
    selected = []
    if backend == "CPU":
        prefs.compute_device_type = "NONE"
        scene.cycles.device = "CPU"
    else:
        prefs.compute_device_type = backend
        prefs.get_devices()
        for device in prefs.devices:
            device.use = device.type == backend
            if device.use:
                selected.append(device.name)
        assert selected, backend + " device unavailable; no CPU fallback accepted"
        scene.cycles.device = "GPU"
    scene.render.engine = "CYCLES"
    scene.cycles.samples = 8
    scene.cycles.use_adaptive_sampling = False
    scene.cycles.use_denoising = True
    scene.cycles.denoiser = "OPENIMAGEDENOISE"
    scene.cycles.denoising_use_gpu = backend != "CPU"
    scene.render.resolution_x = 128
    scene.render.resolution_y = 128
    scene.render.resolution_percentage = 100
    scene.render.image_settings.file_format = "PNG"
    scene.render.image_settings.color_mode = "RGB"
    scene.render.threads_mode = "FIXED"
    scene.render.threads = 2
    cube = bpy.data.objects["Cube"]
    frames = []
    started = time.monotonic()
    for frame in range(1, 11):
        scene.frame_set(frame)
        cube.rotation_euler.z = frame * 0.1
        path = destination / (backend.lower() + "-%02d.png" % frame)
        scene.render.filepath = str(path)
        bpy.ops.render.render(write_still=True)
        raw = path.read_bytes()
        assert raw.startswith(b"\x89PNG\r\n\x1a\n") and len(raw) > 100
        frames.append({"path": path.name, "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()})
    assert len({frame["sha256"] for frame in frames}) == 10, "Frames must depict the changing scene"
    receipt = {"blender": bpy.app.version_string, "backend": backend, "selected_devices": selected,
               "denoiser_requested": "OPENIMAGEDENOISE", "denoising_gpu_requested": backend != "CPU",
               "frames": frames, "seconds": time.monotonic() - started}
    (destination / ("blender-" + backend.lower() + "-receipt.json")).write_text(json.dumps(receipt, indent=2) + "\n")
    print("SPARKDEV_RENDER " + json.dumps(receipt))


if __name__ == "__main__":
    main()
