import json
from pathlib import Path
import tempfile
import unittest

from compare_laya_training import summarize
from finalize_laya_training import metrics_by_area, selected_candidate
import test_compare_laya_training
from train_laya_project import sha256


class FinalizationTests(unittest.TestCase):
    def test_heldout_evaluation_cannot_switch_winner_or_use_changed_weights(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base = root/'base'; base.mkdir()
            (base/'model.safetensors').write_bytes(b'base')
            reports = test_compare_laya_training.ComparisonTests().reports()
            for report in reports:
                path = root/f'{report["config"]["loss"]}-{report["config"]["seed"]}'
                (path/'checkpoint').mkdir(parents=True)
                (path/'checkpoint/model.safetensors').write_bytes(b'candidate')
                report.update(base_weight_sha256=sha256(base/'model.safetensors'),
                              checkpoint_sha256=sha256(path/'checkpoint/model.safetensors'))
                (path/'training-report.json').write_text(json.dumps(report))
            comparison = summarize(reports)
            (root/'comparison.json').write_text(json.dumps(comparison))
            selected, _ = selected_candidate(root, {'source_sha256':'same'}, base)
            self.assertEqual(selected.parent.name, 'rlcd-20261015')
            with self.assertRaisesRegex(ValueError, '数据不符'):
                selected_candidate(root, {'source_sha256':'changed'}, base)
            comparison['selected_loss'] = 'soft-ce'
            (root/'comparison.json').write_text(json.dumps(comparison))
            with self.assertRaisesRegex(ValueError, '拒绝更换'):
                selected_candidate(root, {'source_sha256':'same'}, base)
            (root/'comparison.json').write_text(json.dumps(summarize(reports)))
            (selected/'model.safetensors').write_bytes(b'changed')
            with self.assertRaisesRegex(ValueError, '候选权重已变化'):
                selected_candidate(root, {'source_sha256':'same'}, base)

    def test_area_metrics_follow_question_order_and_reject_missing_records(self):
        records = [(0, [3., 0.], [1., 0.], 2), (0, [3., 0.], [0., 1.], 2),
                   (0, [0., 3.], [0., 1.], 2)]
        rows = [{'questions':{'a':{},'b':{}}, 'expected':{'a':'x','b':'y'}},
                {'questions':{'a':{}}, 'expected':{'a':'y'}}]
        samples = [{'area':'one'}, {'area':'two'}]
        result = metrics_by_area(records, rows, samples)
        self.assertEqual(result['areas']['one']['accuracy'], .5)
        self.assertEqual(result['areas']['two']['accuracy'], 1.)
        self.assertEqual(result['questions']['a']['items'], 2)
        self.assertEqual(result['questions']['a']['accuracy'], 1.)
        with self.assertRaisesRegex(ValueError, '记录数'):
            metrics_by_area(records[:-1], rows, samples)


if __name__ == '__main__':
    unittest.main()
