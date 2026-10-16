import unittest

from e2e_assistant_tools import clarified_before_draft


def call(name):
    return {"role": "assistant", "tool_calls": [{"function": {"name": name}}]}


class ClarificationEvidenceTests(unittest.TestCase):
    def test_plain_question_then_answer_then_draft_is_valid(self):
        self.assertTrue(clarified_before_draft([
            {"role": "user", "content": "给张经理起草"},
            {"role": "assistant", "content": "研发还是采购那位？"},
            {"role": "user", "content": "研发那位"},
            call("mac_mail_draft"),
        ]))

    def test_ask_tool_requires_a_subsequent_user_answer(self):
        history = [{"role": "user"}, call("ask_user")]
        self.assertFalse(clarified_before_draft(history + [call("mac_mail_draft")]))
        self.assertTrue(clarified_before_draft(
            history + [{"role": "user"}, call("mac_mail_draft")]))

    def test_drafting_before_clarification_never_passes(self):
        self.assertFalse(clarified_before_draft([
            {"role": "user"}, call("mac_mail_draft"), call("ask_user"),
            {"role": "user"}, call("mac_mail_draft"),
        ]))

    def test_two_user_messages_without_question_do_not_prove_clarification(self):
        self.assertFalse(clarified_before_draft([
            {"role": "user"}, {"role": "assistant", "content": "好的"},
            {"role": "user"}, call("mac_mail_draft"),
        ]))


if __name__ == "__main__":
    unittest.main()
