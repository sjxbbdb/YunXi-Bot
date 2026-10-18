import hashlib
import importlib.util
import io
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("computer_build", Path(__file__).with_name("build-macos-computer.py"))
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


class ComputerBundleTests(unittest.TestCase):
    def archive(self, directory, name="bundle/file", link=False):
        path = Path(directory) / "input.tar.gz"
        with tarfile.open(path, "w:gz") as tar:
            info = tarfile.TarInfo(name)
            if link:
                info.type = tarfile.SYMTYPE
                info.linkname = "/etc/passwd"
                tar.addfile(info)
            else:
                info.size = 2
                tar.addfile(info, io.BytesIO(b"ok"))
        return path

    def test_bad_digest_never_creates_output(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = self.archive(directory)
            output = Path(directory) / "output"
            with self.assertRaises(ValueError):
                builder.extract_verified(archive, output)
            self.assertFalse(output.exists())

    def test_verified_archive_still_rejects_traversal_absolute_and_links(self):
        for name, link in [("../escape", False), ("/escape", False), ("bundle/link", True)]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                archive = self.archive(directory, name, link)
                with patch.object(builder, "SHA256", hashlib.sha256(archive.read_bytes()).hexdigest()):
                    with self.assertRaises(ValueError):
                        builder.extract_verified(archive, Path(directory) / "output")

    def test_verified_regular_files_extract(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = self.archive(directory)
            output = Path(directory) / "output"
            with patch.object(builder, "SHA256", hashlib.sha256(archive.read_bytes()).hexdigest()):
                builder.extract_verified(archive, output)
            self.assertEqual((output / "bundle/file").read_bytes(), b"ok")

    def test_rejected_signature_never_executes_driver(self):
        with patch.object(builder, "run", side_effect=subprocess.CalledProcessError(1, "codesign")) as run:
            with self.assertRaises(subprocess.CalledProcessError):
                builder.verified_driver(Path("/test.app"))
            self.assertEqual(run.call_count, 1)
            self.assertIn("YCK386LBJ7", run.call_args.args[-2])

    def test_other_driver_version_is_not_silently_accepted(self):
        with patch.object(builder, "run", side_effect=["", "cua-driver 99.0"]):
            with self.assertRaises(ValueError):
                builder.verified_driver(Path("/test.app"))


if __name__ == "__main__":
    unittest.main()
