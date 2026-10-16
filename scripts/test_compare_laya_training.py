import math
import unittest

from compare_laya_training import LOSSES, SEEDS, summarize


class ComparisonTests(unittest.TestCase):
    def reports(self):
        return [dict(status='completed_candidate_only',selection_only=True,
                     config=dict(loss=loss,seed=seed,epochs=8,micro_batch=1),
                     data_manifest={'source_sha256':'same'},base_weight_sha256='base',
                     items={'train':324,'validation':72},optimizer_updates=328,
                     final={'train':{'items':324,'accuracy':1.,'loss':.01},
                            'validation':{'items':72,'accuracy':.75,'loss':.5}})
                for seed in SEEDS for loss in LOSSES]

    def test_selection_uses_three_seed_mean_not_one_lucky_run(self):
        rows=self.reports()
        for report in rows:
            if report['config']['loss']=='soft-ce':
                report['final']['validation']['accuracy']=1. if report['config']['seed']==SEEDS[0] else .5
        result=summarize(rows)
        self.assertEqual(result['selected_loss'],'rlcd')
        self.assertFalse(result['test_evaluated'])

    def test_incomplete_or_duplicate_panel_is_rejected(self):
        rows=self.reports()
        for bad in (rows[:-1],rows[:-1]+[rows[0]]):
            with self.assertRaises(ValueError): summarize(bad)

    def test_changed_training_contract_or_seen_test_is_rejected(self):
        for field,value in [('config',{'epochs':12}),('data_manifest',{'source_sha256':'other'}),
                            ('final',{'test':{'accuracy':1.}})]:
            rows=self.reports();rows[0][field].update(value)
            with self.assertRaises(ValueError): summarize(rows)

    def test_failed_or_nonfinite_metrics_cannot_win(self):
        rows=self.reports();rows[0]['status']='failed'
        with self.assertRaises(ValueError): summarize(rows)
        rows=self.reports();rows[0]['final']['validation']['accuracy']=math.nan
        with self.assertRaises(ValueError): summarize(rows)


if __name__=='__main__':
    unittest.main()
