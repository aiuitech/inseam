"""Replay changes ranking without changing the source index configuration."""
import argparse
import dataclasses
import unittest
from pathlib import Path
from unittest.mock import patch

import enterprise_rag_bench as enterprise
import harness
import requery


class RequeryTests(unittest.TestCase):
    def test_judged_replay_uses_upstream_scorer_and_selected_count(self) -> None:
        options = enterprise.RunOptions(20, 8, 12, 8, 0, 4, False,
                                       evaluation_model="openai/gpt-5.4")
        inputs = [{"question_id": "first"}, {"question_id": "last"}]
        with patch.object(enterprise, "retrieval_scores", return_value={"recall": 0.5}), \
             patch.object(enterprise, "evaluate", return_value={"score": 75}) as evaluate, \
             patch.object(enterprise, "pip_freeze") as freeze:
            result = requery.score(enterprise, Path("run"), [], inputs, options)
        evaluate.assert_called_once_with(Path("run"), Path("run/logs"), 4, 2, "openai/gpt-5.4")
        freeze.assert_called_once_with(Path("run"))
        self.assertEqual(result, {"retrieval": {"recall": 0.5}, "enterprise_rag_bench": {"score": 75}})

    def test_bookends_preserve_release_order_and_include_both_ends(self) -> None:
        questions = [{"question_id": str(index)} for index in range(500)]
        selected = requery.select_bookends(questions, 10)
        self.assertEqual([row["question_id"] for row in selected],
                         [str(index) for index in (*range(10), *range(490, 500))])

    def test_bookends_reject_overlap_and_invalid_counts(self) -> None:
        questions = [{"question_id": str(index)} for index in range(5)]
        for count in (-1, 0, 3, 501):
            with self.subTest(count=count):
                with self.assertRaises(harness.BenchmarkError):
                    requery.select_bookends(questions, count)

    def test_judged_bookends_enable_gpt54_without_changing_index_shape(self) -> None:
        options = enterprise.RunOptions(500, 8, 12, 8, 0, 4, True, skip_agent=True)
        arguments = argparse.Namespace(**dict.fromkeys(requery.QUERY_OPTIONS),
            bookend_count=10, answer_model="openai/gpt-5.4", evaluation_model="openai/gpt-5.4")
        selected = requery.selected_options(enterprise, {"options": dataclasses.asdict(options)}, arguments)
        self.assertFalse(selected.skip_agent)
        self.assertFalse(selected.skip_evaluation)
        self.assertEqual(selected.question_limit, 20)
        self.assertEqual(selected.answer_model, "openai/gpt-5.4")
        self.assertEqual(selected.evaluation_model, "openai/gpt-5.4")
        self.assertEqual(selected.summary_target_chars, options.summary_target_chars)
        with self.assertRaises(harness.BenchmarkError):
            requery.selected_options(enterprise, {"options": dataclasses.asdict(
                dataclasses.replace(options, corpus_slice=25000))}, arguments)

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

    def test_finder_overrides_replace_the_origins_list_for_a_replay(self) -> None:
        origin = {"options": dataclasses.asdict(enterprise.RunOptions(500, 8, 5, 8, 0, 4, False))}
        origin["options"]["finder_overrides"] = ["hub_degree_max=1"]
        arguments = argparse.Namespace(
            finder_seeds=None, finder_seed_k=None, finder_rrf_k=None, finder_damping=None,
            finder_lexical_weight=None, finder_max_vector_distance=None, bookend_count=0,
            finder_override=["seed_lists.cluster.weight=1", "seed_lists.exact.weight=3"],
        )
        options = requery.selected_options(enterprise, origin, arguments)
        self.assertEqual(options.finder_overrides, ["seed_lists.cluster.weight=1", "seed_lists.exact.weight=3"])
        self.assertTrue(options.skip_agent)
        kept = argparse.Namespace(**{**vars(arguments), "finder_override": []})
        self.assertEqual(requery.selected_options(enterprise, origin, kept).finder_overrides, ["hub_degree_max=1"])

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
