import hashlib
import contextlib
import io
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import fetch_model as fetch


class IntegrityTests(unittest.TestCase):
    def test_system_http_proxy_survives_no_proxy_normalization(self):
        with patch.dict(os.environ, {}, clear=True), patch.object(
            fetch.urllib.request, "getproxies", return_value={"https": "http://127.0.0.1:7890"}
        ):
            fetch.sanitize_proxy_env()
            self.assertEqual(os.environ.get("HTTPS_PROXY"), "http://127.0.0.1:7890")

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        self.entry = {
            "name": "test", "dir": "test", "source_repo": "test/test",
            "revision": "fixed", "required_files": ["model.safetensors"],
            "sha256_files": {"model.safetensors": hashlib.sha256(b"good").hexdigest()},
        }
        self.dest = self.home / "models/test/model.safetensors"
        self.dest.parent.mkdir(parents=True)
        self.dest.write_bytes(b"good")

    def test_check_rejects_tampered_existing_weight(self):
        self.dest.write_bytes(b"bad")
        self.assertNotEqual(fetch.install(self.entry, self.home, False, True), 0)

    def test_failed_replacement_preserves_old_weight(self):
        def corrupt(entry, staging):
            staging.mkdir(parents=True)
            (staging / "model.safetensors").write_bytes(b"bad")
            return True
        with patch.object(fetch, "fetch_from_huggingface", side_effect=corrupt):
            self.assertNotEqual(fetch.install(self.entry, self.home, True, False), 0)
        self.assertEqual(self.dest.read_bytes(), b"good")

    def test_verified_existing_weight_needs_no_network(self):
        with patch.object(fetch, "fetch_from_huggingface") as download:
            self.assertEqual(fetch.install(self.entry, self.home, False, True), 0)
            download.assert_not_called()

    def test_embedding_download_hint_does_not_launch_a_decision_model(self):
        out = io.StringIO()
        with patch.object(fetch.sys, 'argv', ['fetch', '--only', 'embeddinggemma-2']), patch.object(fetch, 'install', return_value=0), contextlib.redirect_stdout(out):
            self.assertEqual(fetch.main(), 0)
        self.assertIn('benchmark_embeddings.py', out.getvalue())
        self.assertNotIn('verdict_server.py', out.getvalue())
