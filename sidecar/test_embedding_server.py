import unittest
from embedding_server import validate

class ValidationTests(unittest.TestCase):
    def test_invalid_or_unbounded_requests_are_rejected(self):
        for v in [None, [], {}, {'texts': [], 'kind': 'query'}, {'texts': ['x']*9, 'kind': 'passage'},
                  {'texts': ['x'], 'kind': 'instruction'}, {'texts': [''], 'kind': 'query'},
                  {'texts': [3], 'kind': 'query'}, {'texts': ['x'*2001], 'kind': 'query'},
                  {'texts': ['x'], 'kind': 'query', 'path': '/tmp'}]:
            with self.subTest(value=str(v)[:60]), self.assertRaises(ValueError): validate(v)
    def test_unicode_is_preserved(self):
        self.assertEqual(validate({'texts': ['中文🙂'], 'kind': 'passage'}), (['中文🙂'], 'passage'))

if __name__ == '__main__': unittest.main()
