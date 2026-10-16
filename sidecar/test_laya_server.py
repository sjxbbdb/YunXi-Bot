import copy
import http.client
import json
import sys
import tempfile
import threading
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

import laya_server as server


CHOICE = {"id": "q", "instructions": "现在提醒？", "kind": {
    "type": "choice", "criteria": {"speak": "现在提醒", "hold": "稍后看"}}}


class ContractTests(unittest.TestCase):
    def test_native_probabilities_survive_conversion(self):
        raw = {"q": {"choice": "hold", "probabilities": {"speak": .2, "hold": .8}}}
        answer = server.normalize_answers(raw, [CHOICE])["q"]
        self.assertEqual(answer["distribution"], raw["q"]["probabilities"])

    def test_score_indices_map_to_declared_levels(self):
        q = {"id": "q", "kind": {"type": "score", "levels": ["低", "高"]}}
        raw = {"q": {"score": .8, "probabilities": {"0": .2, "1": .8}}}
        answer = server.normalize_answers(raw, [q])["q"]
        self.assertEqual(answer, {"choice": "高", "expected_score": .8,
                                  "distribution": {"低": .2, "高": .8}})

    def test_missing_invalid_and_abstained_answers_fail_closed(self):
        for answer in [None, {"choice": "invented"},
                       {"choice": "hold", "probabilities": {"speak": float("nan"), "hold": 1}},
                       {"choice": "hold", "probabilities": {"hold": .8}},
                       {"choice": "hold", "abstention": True},
                       {"choice": "hold", "abstention": "unevaluated"}]:
            with self.subTest(answer=answer), self.assertRaises(ValueError):
                server.normalize_answers({"q": answer}, [CHOICE])

    def test_noul_rejects_nonfinite_probability(self):
        q = {"id": "q", "kind": {"type": "noul"}}
        for value in [float("nan"), -1, 2, True]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                server.normalize_answers({"q": {"noul": value}}, [q])

    def test_duplicate_question_ids_are_rejected(self):
        with self.assertRaises(ValueError):
            server.to_laya_questions([CHOICE, copy.deepcopy(CHOICE)])

    def test_truncation_fallback_and_abstention_are_not_success(self):
        for raw in [{"usage": {"truncated": True}}, {"usage": {"options": {"q": {}}}},
                    {"abstained": True}]:
            agent = SimpleNamespace(system_one=Mock(return_value=raw), cpu_fallback_count=0)
            with patch.object(server, "_router", agent), self.assertRaises(ValueError):
                server.predict({}, [CHOICE])
        agent = SimpleNamespace(cpu_fallback_count=0)
        def fallback(*args, **kwargs):
            agent.cpu_fallback_count += 1
            return {}
        agent.system_one = fallback
        with patch.object(server, "_router", agent), self.assertRaises(ValueError):
            server.predict({}, [CHOICE])

    def test_missing_local_model_never_initializes_router(self):
        with patch.object(server, "_router", None), patch.object(server, "_router_error", None):
            server.load_router("/nonexistent/yunxi-laya-checkpoint", "cpu")
            self.assertIsNone(server._router)
            self.assertIn("FileNotFoundError", server._router_error)

    def test_load_failure_and_device_fallback_never_report_ready(self):
        for actual, failure in [("cpu", None), ("mps", RuntimeError("private input"))]:
            with self.subTest(actual=actual, failure=failure), tempfile.TemporaryDirectory() as path:
                agent = SimpleNamespace(device=actual, cpu_fallback_count=0,
                                        system_one=Mock(side_effect=failure))
                with patch.dict(sys.modules, {"torch": SimpleNamespace(set_num_threads=Mock()),
                                             "laya": SimpleNamespace(load=Mock(return_value=agent))}), \
                     patch.object(server, "_router", None), patch.object(server, "_router_error", None):
                    server.load_router(path, "mps")
                    self.assertIsNone(server._router)
                    self.assertNotIn("private input", server._router_error)

    def test_ready_requires_successful_local_load_and_warmup(self):
        agent = SimpleNamespace(device="mps", cpu_fallback_count=0, system_one=Mock())
        with tempfile.TemporaryDirectory() as path, \
             patch.dict(sys.modules, {"torch": SimpleNamespace(set_num_threads=Mock()),
                                     "laya": SimpleNamespace(load=Mock(return_value=agent))}), \
             patch.object(server, "_router", None), patch.object(server, "_router_error", None), \
             patch.object(server, "_loaded_model", "unloaded"), patch.object(server, "_device", None):
            server.load_router(path, "mps")
            self.assertIs(server._router, agent)
            agent.system_one.assert_called_once()


class HttpTests(unittest.TestCase):
    def setUp(self):
        self.http = server.HTTPServer(("127.0.0.1", 0), server.Handler)
        self.thread = threading.Thread(target=self.http.serve_forever, kwargs={"poll_interval": .01})
        self.thread.start()
        self.addCleanup(self.stop)

    def stop(self):
        self.http.shutdown()
        self.thread.join()
        self.http.server_close()

    def request(self, method, path, body=None):
        conn = http.client.HTTPConnection(*self.http.server_address, timeout=2)
        try:
            conn.request(method, path, body)
            resp = conn.getresponse()
            return resp.status, json.loads(resp.read())
        finally:
            conn.close()

    def test_unloaded_model_reports_degraded_and_refuses_decisions(self):
        with patch.object(server, "_router", None):
            self.assertEqual(self.request("GET", "/health")[1]["status"], "degraded")
            code, body = self.request("POST", "/decide", json.dumps({"state": {}, "questions": [CHOICE]}))
            self.assertEqual(code, 503)
            self.assertTrue(body["degraded"])

    def test_valid_native_reply_preserves_contract_and_uncalibrated_status(self):
        agent = SimpleNamespace(cpu_fallback_count=0, system_one=Mock(return_value={
            "answers": {"q": {"choice": "hold", "probabilities": {"speak": .2, "hold": .8}}}}))
        with patch.object(server, "_router", agent):
            code, body = self.request("POST", "/decide", json.dumps({"state": {}, "questions": [CHOICE]}))
            self.assertEqual(code, 200)
            self.assertEqual(body["answers"]["q"]["choice"], "hold")
            self.assertFalse(body["calibrated"])

    def test_private_input_is_not_echoed_in_failure(self):
        agent = SimpleNamespace(cpu_fallback_count=0, system_one=Mock(side_effect=ValueError("private-secret")))
        with patch.object(server, "_router", agent):
            code, body = self.request("POST", "/decide", json.dumps({"state": {}, "questions": [CHOICE]}))
            self.assertEqual(code, 503)
            self.assertNotIn("private-secret", json.dumps(body))

    def test_bad_json_and_nonfinite_input_are_rejected(self):
        for body in ['{', '{"state": NaN, "questions": []}', '[]']:
            with self.subTest(body=body):
                self.assertEqual(self.request("POST", "/decide", body)[0], 400)
