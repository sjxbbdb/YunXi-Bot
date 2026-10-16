import copy
import json
from pathlib import Path
import tempfile
import unittest

from prepare_laya_data import DEFAULT, export
from train_laya_project import load_reviewed_splits, validation_rank


class TrainingBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.document = json.loads(DEFAULT.read_text())
        for sample in self.document['samples']:
            sample['review_status']='approved'
            sample['reviewed_answers']=copy.deepcopy(sample['proposed_answers'])

    def test_modified_export_cannot_enter_training(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp); source=root/'review.json'; source.write_text(json.dumps(self.document))
            export(self.document,root/'data')
            splits,_=load_reviewed_splits(root/'data',source)
            self.assertEqual(len(splits['train']),16)
            (root/'data/train.jsonl').write_text('{}\n')
            with self.assertRaisesRegex(ValueError,'被修改'):
                load_reviewed_splits(root/'data',source)

    def test_changed_review_invalidates_previous_export(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp); source=root/'review.json'
            export(self.document,root/'data')
            self.document['samples'][0]['reviewed_answers']['input_kind']='task'
            source.write_text(json.dumps(self.document))
            with self.assertRaisesRegex(ValueError,'哈希不符'):
                load_reviewed_splits(root/'data',source)

    def test_pending_review_is_rejected_before_model_loading(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp); source=root/'review.json'
            self.document['samples'][0].update(review_status='pending',reviewed_answers=None)
            source.write_text(json.dumps(self.document))
            with self.assertRaisesRegex(ValueError,'未审核'):
                load_reviewed_splits(root/'missing-data',source)

    def test_selection_prefers_accuracy_then_loss_and_rejects_nan(self):
        self.assertGreater(validation_rank({'items':2,'accuracy':1.,'loss':.5}),
                           validation_rank({'items':2,'accuracy':.5,'loss':.1}))
        self.assertGreater(validation_rank({'items':2,'accuracy':1.,'loss':.1}),
                           validation_rank({'items':2,'accuracy':1.,'loss':.5}))
        with self.assertRaises(ValueError):
            validation_rank({'items':2,'accuracy':1.,'loss':float('nan')})

    def test_candidate_export_preserves_training_precision(self):
        import torch
        from safetensors.torch import load_file
        from unittest.mock import patch
        from train_laya_project import save_candidate
        model=torch.nn.Linear(1,1,bias=False)
        with torch.no_grad():
            model.weight.fill_(1.0001)
        with tempfile.TemporaryDirectory() as tmp:
            with patch('laya.train.save_checkpoint'):
                save_candidate(model,None,{},Path(tmp))
            restored=load_file(str(Path(tmp)/'model.safetensors'))
            self.assertEqual(restored['weight'].dtype,torch.float32)
            self.assertTrue(torch.equal(restored['weight'],model.weight))

    def test_method_selection_never_fits_calibration_or_scores_test(self):
        from train_laya_project import finish_evaluation
        calls=[]
        def measure(split,*unused):
            if split not in ('train','validation'):
                self.fail('held-out data entered method selection')
            calls.append(split)
            return {'items':2,'accuracy':.5,'loss':1.}
        def calibrate():
            self.fail('calibration fitted during method selection')
        fitted,calibration,metrics=finish_evaluation(measure,calibrate,True)
        self.assertEqual(calls,['train','validation'])
        self.assertEqual(set(metrics),{'train','validation'})
        self.assertEqual(fitted['temperature'],[1.,1.,1.])
        self.assertEqual(calibration['status'],'deferred_until_selection_complete')


if __name__=='__main__':
    unittest.main()
