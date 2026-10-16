import hashlib
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

from fetch_mlx_candidates import verify
import fetch_mlx_candidates as fetch


class IntegrityTests(unittest.TestCase):
    def test_single_model_selection_respects_home_without_redownloading_discarded_models(self):
        name = "gemma-4-26B-A4B-it-qat-4bit"
        with patch("sys.argv", ["fetch", "--model", name, "--home", "/tmp/yunxi custom"]), \
             patch.object(fetch, "download") as download:
            self.assertEqual(fetch.main(), 0)
        download.assert_called_once()
        self.assertEqual(download.call_args.args[0]["repo_id"].split("/")[-1], name)
        self.assertEqual(download.call_args.args[1], Path("/tmp/yunxi custom"))

    def test_unknown_model_fails_before_any_download(self):
        with patch("sys.argv", ["fetch", "--model", "unknown"]), patch.object(fetch, "download") as download:
            with self.assertRaises(SystemExit):
                fetch.main()
        download.assert_not_called()

    def test_partial_and_tampered_downloads_are_not_ready(self):
        model = {"files": [{"path": "model.safetensors", "size": 4,
                            "sha256": hashlib.sha256(b"good").hexdigest()}]}
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp)
            with self.assertRaises(ValueError):
                verify(model, dest)
            (dest / "model.safetensors").write_bytes(b"evil")
            with self.assertRaises(ValueError):
                verify(model, dest)
            (dest / "model.safetensors").write_bytes(b"good")
            verify(model, dest)
