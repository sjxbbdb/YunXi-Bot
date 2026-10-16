import copy
import json
from pathlib import Path
import tempfile
import unittest

from prepare_laya_data import DEFAULT, ROOT, export, row, validate, training_coverage
from evaluate_laya_project import compare, reverse_options


class LayaPreparationTests(unittest.TestCase):
    def setUp(self):
        self.document = json.loads(DEFAULT.read_text())
        for sample in self.document['samples']:
            sample['review_status'] = 'pending'
            sample['reviewed_answers'] = None

    def approve(self):
        for sample in self.document['samples']:
            sample['review_status'] = 'approved'
            sample['reviewed_answers'] = copy.deepcopy(sample['proposed_answers'])

    def test_drafts_cannot_be_exported_or_used_as_reviewed_labels(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp)/'dataset'
            with self.assertRaisesRegex(ValueError,'待审核'):
                export(self.document, output)
            self.assertFalse(output.exists())
        with self.assertRaisesRegex(ValueError,'尚未人工审核'):
            row(self.document['samples'][0])

    def test_group_leakage_is_rejected(self):
        self.document['samples'][1]['split'] = 'test'
        with self.assertRaisesRegex(ValueError,'不能跨'):
            validate(self.document)

    def test_duplicate_input_is_rejected_even_with_other_id_and_split(self):
        duplicate = copy.deepcopy(self.document['samples'][0])
        duplicate.update(id='duplicate', group_id='other', split='test')
        self.document['samples'].append(duplicate)
        with self.assertRaisesRegex(ValueError,'重复输入'):
            validate(self.document)

    def test_unknown_label_and_incomplete_answer_are_rejected(self):
        for answers in ({'input_kind':'maybe'},{}):
            with self.subTest(answers=answers):
                self.document['samples'][0]['proposed_answers'] = answers
                with self.assertRaises(ValueError):
                    validate(self.document)

    def test_boolean_labels_do_not_accept_string_true(self):
        sample = next(s for s in self.document['samples'] if s['area']=='companion')
        sample['proposed_answers']['needs_support'] = 'true'
        with self.assertRaises(ValueError):
            validate(self.document)

    def test_reviewed_answers_are_used_and_metadata_does_not_enter_input(self):
        self.approve()
        sample = self.document['samples'][0]
        sample['reviewed_answers']['input_kind'] = 'task'
        output = row(sample)
        self.assertEqual(output['expected']['input_kind'],'task')
        self.assertEqual(set(output), {'state','questions','expected'})
        self.assertEqual(output['state'], sample['request']['state'])
        self.assertNotIn(sample['proposal_reason'], json.dumps(output,ensure_ascii=False))

    def test_export_keeps_four_splits_and_refuses_overwrite(self):
        self.approve()
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp)/'dataset'
            export(self.document, output)
            self.assertEqual(len(list(output.glob('*.jsonl'))),4)
            self.assertEqual(sum(len(p.read_text().splitlines()) for p in output.glob('*.jsonl')),32)
            with self.assertRaisesRegex(ValueError,'已存在'):
                export(self.document, output)

    def test_pending_status_cannot_carry_approved_answers(self):
        self.document['samples'][0]['reviewed_answers'] = {'input_kind':'chat'}
        with self.assertRaises(ValueError):
            validate(self.document)

    def test_fixed_question_text_matches_production_literals(self):
        paths = {'input':'think/router.rs','memory':'recall_gate.rs','tool':'tool/mod.rs',
                 'companion':'companion.rs','mail':'triage.rs'}
        for sample in self.document['samples']:
            if sample['area'] not in paths:
                continue
            source = (ROOT/'crates/yunxi-bot-core/src'/paths[sample['area']]).read_text()
            for q in sample['request']['questions']:
                for text in [q['id'],q['instructions'],*q['kind'].get('criteria',{}).values()]:
                    self.assertIn(json.dumps(text,ensure_ascii=False),source)

    def test_state_shapes_match_production_inputs(self):
        for sample in self.document['samples']:
            state = sample['request']['state']
            if sample['area']=='input': self.assertEqual(set(state),{'input'})
            elif sample['area']=='memory': self.assertEqual(set(state),{'使用者这一句'})
            elif sample['area']=='task':
                self.assertEqual(set(state),{'question','options','criteria','context'})
                self.assertEqual(set(state['context']),{'goal','inputs'})
                self.assertIsInstance(state['context']['inputs'],list)
            elif sample['area']=='tool':
                self.assertEqual(set(state),{'tool','capability','scope','arguments','cwd','inside_cwd'})
                json.loads(state['arguments'])

    def test_reversing_options_preserves_labels_and_does_not_mutate_input(self):
        request = self.document['samples'][0]['request']
        original = copy.deepcopy(request)
        reversed_request = reverse_options(request)
        self.assertEqual(request,original)
        self.assertEqual(list(reversed_request['questions'][0]['kind']['criteria']),
                         list(reversed(list(request['questions'][0]['kind']['criteria']))))
        self.assertEqual(reversed_request['state'],request['state'])

    def test_invalid_model_outputs_fail_evaluation(self):
        sample = self.document['samples'][0]
        with self.assertRaises(ValueError):
            compare({'input_kind':{'choice':'unknown'}},sample['proposed_answers'],sample['request']['questions'])

    def test_http_test_filter_sends_only_the_requested_split(self):
        import contextlib
        import io
        from unittest.mock import patch
        from evaluate_laya_project import main
        from prepare_laya_data import canonical
        self.approve()
        selected = [s for s in self.document['samples'] if s['split'] == 'test']
        allowed = {canonical(s['request']):s for s in selected}
        def response(request, **unused):
            sample = allowed[canonical(json.loads(request.data))]
            answers = {q['id']: {('noul' if q['kind']['type']=='noul' else 'choice'):
                       int(sample['reviewed_answers'][q['id']]) if q['kind']['type']=='noul'
                       else sample['reviewed_answers'][q['id']]} for q in sample['request']['questions']}
            return io.StringIO(json.dumps({'answers':answers,'model':'test'}))
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp); source=root/'review.json'; output=root/'result.json'
            source.write_text(json.dumps(self.document))
            with patch('sys.argv',['evaluate','--data',str(source),'--output',str(output),'--split','test']), \
                 patch('urllib.request.urlopen',side_effect=response), contextlib.redirect_stdout(io.StringIO()):
                main()
            report=json.loads(output.read_text())
            self.assertEqual([s['id'] for s in report['cases']],[s['id'] for s in selected])
            self.assertEqual(set(report['splits']),{'test'})

    def test_unseen_training_classes_are_reported_without_moving_holdouts(self):
        before=copy.deepcopy(self.document)
        coverage=training_coverage(self.document['samples'])
        self.assertEqual(coverage['memory_need']['missing_train_labels'],['knowledge','mixed'])
        self.assertEqual(coverage['is_anomaly']['missing_train_labels'],['true'])
        self.assertEqual(coverage['needs_action']['missing_train_labels'],['false'])
        self.assertNotIn('task_decision',coverage)
        self.assertEqual(self.document,before)


if __name__ == '__main__':
    unittest.main()
