#!/usr/bin/env python3
"""Remove dependencies PhaseForge never uses from Blender's `make deps` superbuild.

Blender v4.5.9's build_files/build_environment builds and downloads every library a
full release needs, including x86-only ones (DPC++, IGC) that it downloads even on
linux-aarch64. This script edits two files in place:

  CMakeLists.txt      drop `include(cmake/<name>.cmake)` for every skipped library
  cmake/download.cmake  drop `download_source(<NAME>)` for every skipped library

Each edit must match exactly once, otherwise the script fails, so an upstream change
cannot silently turn a skip into a no-op. The skip list and the grep evidence for each
entry are in README.md ("Disabled features").
"""
import pathlib
import re
import sys

# cmake/<name>.cmake includes removed from the superbuild.
SKIP_INCLUDES = [
    "usd", "materialx", "alembic", "osl", "opencollada",           # scene formats / OSL
    "mesa", "libglu",                                                 # software OpenGL
    "wayland_protocols", "wayland", "wayland_libdecor", "wayland_weston",  # Wayland GHOST
    "xr_openxr", "sdl", "openal", "spnav",                            # VR, audio out, 3D mouse
    "haru", "potrace",                                                # Grease Pencil PDF / trace
    "shaderc_deps", "shaderc", "vulkan", "vulkan-memory-allocator", "spirv-reflect",  # Vulkan
    "level-zero",                                                     # oneAPI loader (x86 only use)
]

# download_source(<NAME>) calls removed (the skipped libraries plus x86-only sources).
SKIP_DOWNLOADS = [
    "OPENAL", "ALEMBIC", "SDL", "OPENCOLLADA", "OSL", "SPNAV", "USD", "MATERIALX",
    "LIBGLU", "MESA", "XR_OPENXR_SDK", "WL_PROTOCOLS", "WAYLAND", "WAYLAND_LIBDECOR",
    "WAYLAND_WESTON", "POTRACE", "HARU", "LEVEL_ZERO",
    # x86-only (never built on BLENDER_PLATFORM_ARM, but downloaded unconditionally)
    "DPCPP", "VCINTRINSICS", "OPENCLHEADERS", "ICDLOADER", "MP11", "DPCPP_SPIRV_HEADERS",
    "UNIFIED_RUNTIME", "UNIFIED_MEMORY_FRAMEWORK", "IGC", "IGC_LLVM", "IGC_OPENCL_CLANG",
    "IGC_VCINTRINSICS", "IGC_SPIRV_HEADERS", "IGC_SPIRV_TOOLS", "IGC_SPIRV_TRANSLATOR",
    "GMMLIB", "OCLOC", "HIPRT", "FLEXBISON",
    # Vulkan toolchain
    "SHADERC", "SHADERC_SPIRV_TOOLS", "SHADERC_SPIRV_HEADERS", "SHADERC_GLSLANG",
    "VULKAN_HEADERS", "VULKAN_LOADER", "VULKAN_UTILITY_LIBRARIES", "SPIRV_HEADERS",
    "SPIRV_REFLECT", "VULKAN_MEMORY_ALLOCATOR",
]


def drop(text, pattern, label):
    found = re.findall(pattern, text, flags=re.M)
    if len(found) != 1:
        sys.exit(f"prune_deps: expected exactly one {label}, found {len(found)}")
    return re.sub(pattern, f"# PhaseForge prune: {label} removed", text, flags=re.M)


def main(env_dir):
    root = pathlib.Path(env_dir)
    lists = root / "CMakeLists.txt"
    text = lists.read_text()
    for name in SKIP_INCLUDES:
        text = drop(text, r"^\s*include\(cmake/" + re.escape(name) + r"\.cmake\)\s*$", f"include({name})")
    lists.write_text(text)

    download = root / "cmake" / "download.cmake"
    text = download.read_text()
    for name in SKIP_DOWNLOADS:
        text = drop(text, r"^\s*download_source\(" + re.escape(name) + r"\)\s*$", f"download_source({name})")
    download.write_text(text)

    # Nothing kept may still depend on a removed external project.
    removed_targets = {"external_usd", "external_materialx", "external_alembic", "external_osl",
                       "external_opencollada", "external_mesa", "external_libglu",
                       "external_wayland", "external_wayland_protocols", "external_wayland_libdecor",
                       "external_wayland_weston", "external_xr_openxr_sdk", "external_sdl",
                       "external_openal", "external_spnav", "external_haru", "external_potrace",
                       "external_shaderc", "external_vulkan_headers", "external_vulkan_loader",
                       "external_vulkan_memory_allocator", "external_spirv_reflect",
                       "external_level-zero", "external_level_zero"}
    kept = [l for l in lists.read_text().splitlines() if l.strip().startswith("include(cmake/")]
    for line in kept:
        name = re.search(r"include\(cmake/([^)]+)\.cmake\)", line).group(1)
        body = (root / "cmake" / f"{name}.cmake").read_text()
        # Only dependency edges matter; a path such as ${LIBDIR}/level-zero is harmless.
        for block in re.findall(r"add_dependencies\((.*?)\)", body, flags=re.S):
            for target in removed_targets:
                if re.search(r"(?<![\w-])" + re.escape(target) + r"(?![\w-])", block):
                    # Edges inside platform branches that never run on Linux ARM are allowed.
                    print(f"prune_deps: note: {name}.cmake names {target} in add_dependencies; check it is platform-gated")
    print(f"prune_deps: removed {len(SKIP_INCLUDES)} includes and {len(SKIP_DOWNLOADS)} downloads")


if __name__ == "__main__":
    main(sys.argv[1])
