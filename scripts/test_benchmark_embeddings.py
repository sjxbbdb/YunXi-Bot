import unittest
import sys
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from scripts.benchmark_embeddings import ranking, choose_threshold, cosine, validate_suite
import json
from unittest.mock import patch
from scripts import benchmark_embeddings as bench

class BenchmarkTests(unittest.TestCase):
    def test_scope_filter_precedes_ranking(self):
        docs=[dict(id='other',scope='b'),dict(id='mine',scope='a'),dict(id='global',scope=None)]
        self.assertEqual([id for id,_ in ranking({'scope':'a'},[1.,0.],docs,[[1.,0.],[.8,.2],[.7,.3]])],['mine','global'])
    def test_threshold_never_uses_evaluation_answers(self):
        rows=[dict(split='calibration',relevant=['x'],top=[['x',.8]]),dict(split='calibration',relevant=[],top=[['x',.4]])]
        before=choose_threshold(rows)
        rows.append(dict(split='evaluation',relevant=[],top=[['x',.99]]))
        self.assertEqual(before,choose_threshold(rows))
        self.assertGreater(before,.4);self.assertLess(before,.8)
    def test_invalid_vector_is_rejected(self):
        for b in [[0.], [float('nan'),0.]]:
            with self.assertRaises(ValueError):cosine([1.,0.],b)
    def test_frozen_suite_references_are_valid(self):
        validate_suite(json.loads((Path(__file__).resolve().parents[1]/'models/eval/embedding-retrieval-v1.json').read_text()))
    def test_missing_second_candidate_prevents_any_model_run(self):
        with patch.object(sys, 'argv', ['benchmark']), patch.object(bench, 'model_ready', side_effect=[True, False]), patch.object(bench.subprocess, 'run') as run:
            with self.assertRaises(SystemExit):
                bench.main()
            run.assert_not_called()

if __name__=='__main__':unittest.main()
