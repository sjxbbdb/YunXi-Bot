import json
from collections import Counter
import unittest

from prepare_laya_data import ROOT, canonical, training_coverage, validate


class ExpandedDatasetTests(unittest.TestCase):
    def setUp(self):
        self.document=json.loads((ROOT/'models/training/project-review-v4.json').read_text())
        self.samples=validate(self.document)

    def test_expanded_splits_cover_every_static_label_and_exclude_old_inputs(self):
        old=json.loads((ROOT/'models/training/project-review-v2.json').read_text())['samples']
        self.assertFalse({canonical(s['request']) for s in old}&{canonical(s['request']) for s in self.samples})
        for area in {s['area'] for s in self.samples}:
            rows=[s for s in self.samples if s['area']==area]
            self.assertEqual(Counter(s['split'] for s in rows),dict(train=36,validation=8,calibration=8,test=8))
        for coverage in training_coverage(self.samples).values():
            self.assertFalse(coverage['missing_train_labels'])
        self.assertEqual(Counter(s['proposed_answers']['task_decision'] for s in self.samples
                                 if s['area']=='task' and s['split']=='train'),dict(opt0=12,opt1=12,opt2=12))

    def test_fixed_questions_and_dynamic_task_options_keep_production_contract(self):
        old=json.loads((ROOT/'models/training/project-review-v2.json').read_text())['samples']
        questions={s['area']:s['request']['questions'] for s in old if s['area']!='task'}
        for sample in self.samples:
            request=sample['request'];state=request['state']
            if sample['area']=='task':
                self.assertEqual(request['questions'][0]['kind']['criteria'],state['criteria'])
                self.assertEqual(set(state['options']),set(state['criteria']))
            else:
                self.assertEqual(request['questions'],questions[sample['area']])
            if sample['area']=='tool':
                args=json.loads(state['arguments'])
                if state['tool']=='run_command': self.assertIsInstance(args['command'],list)
            if sample['area']=='mail':
                if state['received_at_known']:
                    self.assertEqual(state['age_minutes'],(state['now_ms']-state['received_at_ms'])//60000)
                else:
                    self.assertEqual(state['received_at_ms'],0)
                    self.assertIsNone(state['age_minutes'])


if __name__=='__main__':
    unittest.main()
