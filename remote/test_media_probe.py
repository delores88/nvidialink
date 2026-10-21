"""Media declarations must come from actual operations, never encoder names."""
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import media_probe


class VideoProbeTests(unittest.TestCase):
    def test_compiled_encoder_without_working_hardware_is_not_success(self):
        with mock.patch.object(media_probe, "command", side_effect=[(0, "V h264_nvenc", ""), (255, "", "Cannot load libnvidia-encode.so.1")]):
            result = media_probe.probe_video()
        self.assertEqual(result["listed_encoders"], ["h264_nvenc"])
        self.assertFalse(result["nvenc"])
        self.assertIsNone(result["nvdec"])
        self.assertEqual(result["encoders"], [])

    def encode(self, frames="10", decode_code=0):
        def command(args, **kwargs):
            if "-encoders" in args:
                return 0, "V h264_nvenc", ""
            if args[0] == "ffprobe":
                return 0, json.dumps({"streams": [{"codec_name": "h264", "nb_read_frames": frames}]}), ""
            if "-hwaccel" in args:
                self.assertIn("h264_cuvid", args)
                return decode_code, "", "decode_failure" if decode_code else ""
            self.assertIn("h264_nvenc", args)
            Path(args[-1]).write_bytes(b"verified tiny fixture")
            return 0, "", ""
        with mock.patch.object(media_probe, "command", side_effect=command):
            return media_probe.probe_video()

    def test_success_requires_ten_verified_frames_and_explicit_gpu_decoder(self):
        result = self.encode()
        self.assertTrue(result["nvenc"])
        self.assertTrue(result["nvdec"])
        self.assertEqual(result["tests"][0]["frames"], 10)
        self.assertEqual(len(result["tests"][0]["sha256"]), 64)
        self.assertFalse(self.encode(frames="0")["nvenc"])

    def test_decoder_failure_does_not_erase_working_encoder(self):
        result = self.encode(decode_code=1)
        self.assertTrue(result["nvenc"])
        self.assertFalse(result["nvdec"])

    def test_missing_ffmpeg_is_unknown_not_hardware_unsupported(self):
        with mock.patch.object(media_probe, "command", return_value=(127, "", "executable_unavailable")):
            result = media_probe.probe_video()
        self.assertEqual(result["state"], "unavailable")
        self.assertIsNone(result["nvenc"])


class CachedProbeTests(unittest.TestCase):
    def test_status_is_cached_bound_to_succeeded_image_and_never_runs_commands(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(media_probe, "command") as command:
            path = Path(directory) / "runtime-cuda-build.json"
            image = "sha256:" + "a" * 64
            video = {"v": 1, "state": "probed", "nvenc": True, "nvdec": False,
                     "encoders": ["h264_nvenc"], "listed_encoders": list(media_probe.ENCODERS), "observed_at": 100}
            receipt = {"state": "succeeded", "image": image, "probe": {"gpu_video": video}}
            path.write_text(json.dumps(receipt))
            self.assertTrue(media_probe.cached_gpu_video(directory, image)["nvenc"])
            self.assertEqual(media_probe.cached_gpu_video(directory, "sha256:" + "b" * 64)["state"], "unknown")
            receipt["state"] = "failed"
            path.write_text(json.dumps(receipt))
            self.assertIsNone(media_probe.cached_gpu_video(directory, image)["nvenc"])
            command.assert_not_called()

    def test_invalid_cached_claim_does_not_promote_encoder_inventory_to_success(self):
        with tempfile.TemporaryDirectory() as directory:
            image = "sha256:" + "a" * 64
            receipt = {"state": "succeeded", "image": image, "probe": {"gpu_video": {"v": 1,
                "state": "probed", "nvenc": True, "nvdec": None, "encoders": [], "listed_encoders": ["h264_nvenc"]}}}
            (Path(directory) / "runtime-cuda-build.json").write_text(json.dumps(receipt))
            self.assertIsNone(media_probe.cached_gpu_video(directory, image)["nvenc"])


if __name__ == "__main__":
    unittest.main()
