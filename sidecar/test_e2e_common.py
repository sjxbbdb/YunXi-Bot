import json
import tempfile
import unittest
from pathlib import Path

from e2e_common import read_ledger


class LedgerEvidenceTests(unittest.TestCase):
    def test_unicode_separators_inside_mail_content_are_not_jsonl_boundaries(self):
        rows = [{"body": "正文\u2028下一段\u2029末段\x85尾部"}, {"kind": "tool_called"}]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ledger.jsonl"
            path.write_text("\n".join(json.dumps(row, ensure_ascii=False) for row in rows) + "\n")
            self.assertEqual(read_ledger(path), rows)

    def test_invalid_line_is_rejected_instead_of_skipped(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ledger.jsonl"
            path.write_text('{}\n\n{}\n')
            with self.assertRaises(json.JSONDecodeError):
                read_ledger(path)

    def test_missing_ledger_has_no_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(read_ledger(Path(directory) / "missing"), [])


if __name__ == "__main__":
    unittest.main()
