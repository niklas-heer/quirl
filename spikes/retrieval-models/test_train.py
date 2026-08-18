"""Focused contract tests for the bounded retrieval-training harness."""

from pathlib import Path
import tempfile
import unittest

from train import (
    RetrievalExample,
    assert_no_fixture_leakage,
    catalog_phrases,
    extract_single_utility,
    file_sha256,
    retrieval_metrics,
)


class TrainingContractTests(unittest.TestCase):
    def test_extracts_only_one_unwrapped_utility(self) -> None:
        self.assertEqual(extract_single_utility("LC_ALL=C grep -n needle file"), "grep")
        self.assertEqual(extract_single_utility(r"/usr/bin/find . -name '*.rs'"), "find")
        for command in (
            "sudo rm -rf target",
            "printf x | grep x",
            "pwd; rm file",
            "echo $(uname)",
        ):
            self.assertIsNone(extract_single_utility(command))

    def test_metrics_use_one_based_ranks(self) -> None:
        metrics = retrieval_metrics([1, 5, 11])
        self.assertEqual(metrics["queries"], 3)
        self.assertEqual(metrics["median_rank"], 5)
        self.assertAlmostEqual(metrics["recall_at_1"], 1 / 3)
        self.assertAlmostEqual(metrics["recall_at_5"], 2 / 3)
        self.assertAlmostEqual(metrics["recall_at_10"], 2 / 3)
        self.assertAlmostEqual(metrics["mrr"], (1 + 1 / 5 + 1 / 11) / 3)

    def test_fixture_text_is_rejected_from_documents_and_training(self) -> None:
        fixture = [RetrievalExample("show the current directory", "pwd", "held_out")]
        with self.assertRaisesRegex(RuntimeError, "indexed document"):
            assert_no_fixture_leakage(
                fixture,
                {"pwd": "Use this to show the current directory safely."},
                {"pwd": ["where am I"]},
            )
        with self.assertRaisesRegex(RuntimeError, "training data"):
            assert_no_fixture_leakage(
                fixture,
                {"pwd": "Print the working directory."},
                {"pwd": ["show the current directory"]},
            )

    def test_catalog_phrases_are_deduplicated_and_bounded(self) -> None:
        body = "\n".join(
            (
                "summary: List files.",
                "intent: List files. Related intents: show directory; inspect files.",
            )
        )
        self.assertEqual(
            catalog_phrases("ls", body),
            ["ls", "List files.", "show directory", "inspect files"],
        )

    def test_file_identity_rejects_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source"
            source.write_text("bounded", encoding="utf-8")
            link = Path(directory) / "link"
            link.symlink_to(source)
            with self.assertRaisesRegex(RuntimeError, "non-symlink"):
                file_sha256(link, 1024)

    def test_file_identity_enforces_the_byte_bound(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source"
            source.write_bytes(b"12345")
            with self.assertRaisesRegex(RuntimeError, "exceeds 4 bytes"):
                file_sha256(source, 4)


if __name__ == "__main__":
    unittest.main()
