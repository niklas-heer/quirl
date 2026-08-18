"""Contract tests for promotion-quality retrieval training."""

import json
from pathlib import Path
import tempfile
import unittest

from evaluate import load_fixture
from train import RetrievalExample as DiagnosticExample
from train_v2 import (
    FORCED_TEST_UTILITIES,
    RetrievalExample,
    auxiliary_document,
    flatten_documents,
    option_spellings,
    split_utilities,
    write_product_fixture,
)


class PromotionTrainingContractTests(unittest.TestCase):
    def test_whole_utility_splits_are_disjoint_deterministic_and_forced(self) -> None:
        eligible = {f"command-{index}" for index in range(80)} | set(
            FORCED_TEST_UTILITIES
        )
        first = split_utilities(eligible)
        second = split_utilities(eligible)
        self.assertEqual(first, second)
        training, validation, test = first
        self.assertTrue(FORCED_TEST_UTILITIES <= test)
        self.assertFalse(training & validation)
        self.assertFalse(training & test)
        self.assertFalse(validation & test)
        self.assertEqual(training | validation | test, eligible)

    def test_auxiliary_documents_retain_only_utility_and_safe_options(self) -> None:
        options = option_spellings(
            "find /private/path --name=secret -type f -exec rm {} ;", "find"
        )
        self.assertEqual(options, ("--name", "-exec", "-type"))
        document = auxiliary_document("find", [options])
        self.assertIn("Command: find", document)
        self.assertIn("--name", document)
        self.assertNotIn("/private/path", document)
        self.assertNotIn("secret", document)
        self.assertNotIn("rm", document)

    def test_unsafe_utility_name_is_rejected(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "unsafe"):
            auxiliary_document("$(touch /tmp/x)", [])

    def test_flattened_documents_preserve_command_membership(self) -> None:
        commands, texts, indexes = flatten_documents(
            {"b": ["b root"], "a": ["a root", "a option"]}
        )
        self.assertEqual(commands, ["a", "a", "b"])
        self.assertEqual(texts, ["a root", "a option", "b root"])
        self.assertEqual(indexes, {"a": [0, 1], "b": [2]})

    def test_generated_product_fixture_is_valid_and_bounded(self) -> None:
        test = [
            RetrievalExample(
                "remove one reviewed file from the current working directory",
                "rm",
                "test",
            ),
            RetrievalExample(
                "show a compact listing of files in this directory",
                "ls",
                "test",
            ),
        ]
        diagnostic = [
            DiagnosticExample(
                "Entferne diese geprüfte Datei dauerhaft aus dem aktuellen Verzeichnis",
                "rm",
                "unseen_command:de",
            )
        ]
        with tempfile.TemporaryDirectory() as directory:
            result = write_product_fixture(Path(directory), test, {"ls", "rm"}, diagnostic)
            fixture = load_fixture(Path(result["path"]))
            self.assertEqual(len(fixture.queries), 3)
            self.assertTrue(fixture.groups["utility_rm"].destructive)
            value = json.loads(Path(result["path"]).read_text(encoding="utf-8"))
            self.assertEqual(value["schema_version"], 1)


if __name__ == "__main__":
    unittest.main()
