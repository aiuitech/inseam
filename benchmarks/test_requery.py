"""Replay changes ranking without changing the source index configuration."""
import argparse
import dataclasses
import unittest

import enterprise_rag_bench as enterprise
import harness
import requery


class RequeryTests(unittest.TestCase):
    def test_preserves_index_configuration_when_changing_finder(self) -> None:
        text = ('[[entry]]\nid = "directory"\ndisabled = false\n'
                '[[entry]]\nid = "finder"\n[entry.config]\nrrf_k = 60\n'
                '[[entry]]\nid = "summarizer"\n[entry.config]\ntarget_chars = 24000\n')
        options = dataclasses.asdict(enterprise.RunOptions(500, 8, 5, 8, 0, 4, False))
        options["finder_lexical_weight"] = 0.1
        patched = requery.composition_with_query_options(text, options)
        before = text.split('[[entry]]')
        after = patched.split('[[entry]]')
        self.assertEqual(before[1], after[1])
        self.assertEqual(before[3], after[3])
        self.assertIn('lexical_weight = 0.1\n', after[2])
        self.assertEqual(after[2].count('rrf_k = '), 1)

    def test_rejects_missing_or_ambiguous_finder(self) -> None:
        options = dataclasses.asdict(enterprise.RunOptions(500, 8, 5, 8, 0, 4, False))
        finder = '[[entry]]\nid = "finder"\n[entry.config]\n'
        for text in ('', finder + finder, '[[entry]]\nid = "finder"\n'):
            with self.subTest(text=text):
                with self.assertRaises(harness.BenchmarkError):
                    requery.composition_with_query_options(text, options)

    def test_enterprise_replay_always_disables_answers_and_judging(self) -> None:
        source = {"options": dataclasses.asdict(enterprise.RunOptions(500, 8, 5, 8, 0, 4, False))}
        arguments = argparse.Namespace(**dict.fromkeys(requery.QUERY_OPTIONS))
        arguments.finder_lexical_weight = 0.1
        selected = requery.selected_options(enterprise, source, arguments)
        self.assertTrue(selected.skip_agent)
        self.assertTrue(selected.skip_evaluation)
        self.assertEqual(selected.finder_lexical_weight, 0.1)
        self.assertEqual(selected.summary_target_chars, source["options"]["summary_target_chars"])
