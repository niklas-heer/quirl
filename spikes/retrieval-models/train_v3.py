#!/usr/bin/env python3
"""Produce product-validation candidates for Quirl's final model promotion."""

from __future__ import annotations

import argparse
from collections import defaultdict
from dataclasses import asdict
import hashlib
import json
import math
import os
from pathlib import Path
import sqlite3
import sys
from typing import Any

from model2vec import StaticModel
from model2vec.model import quantize_model
import numpy as np
import torch

import train as v1
import train_v2 as v2


TRAINING_SCHEMA_VERSION = 3
SEED = 20260820
PRODUCT_VALIDATION_FORCED = frozenset(("ls", "mkdir", "tail"))
PRODUCT_TEST_FORCED = v2.FORCED_TEST_UTILITIES
PRODUCT_VALIDATION_COUNT = 6
PRODUCT_TEST_COUNT = 8
AUXILIARY_SPLIT_FRACTION = 0.18
CANDIDATES_MAX = 7


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--database", type=Path, required=True)
    parser.add_argument("--model", type=Path, required=True)
    parser.add_argument(
        "--fixture", type=Path, default=Path(__file__).with_name("fixture-v1.json")
    )
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--dataset-cache",
        type=Path,
        default=Path.home() / ".cache/quirl/training/nl2bash",
    )
    parser.add_argument("--epochs", type=int, default=180)
    return parser.parse_args()


def stratified_split(
    eligible: set[str], product_commands: set[str]
) -> tuple[set[str], set[str], set[str]]:
    eligible_product = eligible & product_commands
    if not PRODUCT_TEST_FORCED <= eligible_product:
        raise RuntimeError("forced product test utilities are not all eligible")
    if not PRODUCT_VALIDATION_FORCED <= eligible_product:
        raise RuntimeError("forced product validation utilities are not all eligible")
    if PRODUCT_TEST_FORCED & PRODUCT_VALIDATION_FORCED:
        raise RuntimeError("forced product validation and test utilities overlap")
    ordered_product = sorted(
        eligible_product - PRODUCT_TEST_FORCED - PRODUCT_VALIDATION_FORCED,
        key=lambda command: (v1.stable_hash(str(SEED), "product", command), command),
    )
    product_test = set(PRODUCT_TEST_FORCED)
    product_test.update(ordered_product[: PRODUCT_TEST_COUNT - len(product_test)])
    remaining_product = [command for command in ordered_product if command not in product_test]
    product_validation = set(PRODUCT_VALIDATION_FORCED)
    product_validation.update(
        remaining_product[: PRODUCT_VALIDATION_COUNT - len(product_validation)]
    )
    auxiliary = eligible - eligible_product
    ordered_auxiliary = sorted(
        auxiliary,
        key=lambda command: (v1.stable_hash(str(SEED), "auxiliary", command), command),
    )
    auxiliary_test_count = math.ceil(len(auxiliary) * AUXILIARY_SPLIT_FRACTION)
    auxiliary_validation_count = math.ceil(len(auxiliary) * AUXILIARY_SPLIT_FRACTION)
    auxiliary_test = set(ordered_auxiliary[:auxiliary_test_count])
    auxiliary_validation = set(
        ordered_auxiliary[
            auxiliary_test_count : auxiliary_test_count + auxiliary_validation_count
        ]
    )
    test = product_test | auxiliary_test
    validation = product_validation | auxiliary_validation
    training = eligible - validation - test
    if training & validation or training & test or validation & test:
        raise RuntimeError("stratified whole-utility partitions overlap")
    if not training or len(product_test) != PRODUCT_TEST_COUNT:
        raise RuntimeError("stratified split violates its product group bounds")
    if len(product_validation) != PRODUCT_VALIDATION_COUNT:
        raise RuntimeError("stratified split has the wrong product validation count")
    return training, validation, test


def load_corpus(
    cache: Path,
    product_documents: dict[str, list[str]],
    root_titles: dict[str, str],
    root_bodies: dict[str, str],
) -> tuple[
    dict[str, list[str]],
    dict[str, list[str]],
    list[v2.RetrievalExample],
    list[v2.RetrievalExample],
    dict[str, Any],
]:
    paths = {
        name: v1.download_verified(cache, name, digest)
        for name, digest in v1.NL2BASH_FILES.items()
    }
    queries = paths["all.nl"].read_text(encoding="utf-8").splitlines()
    command_lines = paths["all.cm"].read_text(encoding="utf-8").splitlines()
    if len(queries) != len(command_lines) or len(queries) > v1.NL2BASH_LINES_MAX:
        raise RuntimeError("NL2Bash files violate their aligned line-count bound")
    grouped: dict[str, list[tuple[str, tuple[str, ...]]]] = defaultdict(list)
    for query, command_line in zip(queries, command_lines, strict=True):
        utility = v1.extract_single_utility(command_line)
        if (
            utility is not None
            and v2.UTILITY_PATTERN.fullmatch(utility)
            and query.strip()
            and len(query.encode("utf-8")) <= v2.QUERY_BYTES_MAX
        ):
            grouped[utility].append((query, v2.option_spellings(command_line, utility)))
    if not grouped or len(grouped) > v2.UTILITIES_MAX:
        raise RuntimeError(f"utility count is outside 1..{v2.UTILITIES_MAX}")
    deduplicated: dict[str, list[tuple[str, tuple[str, ...]]]] = {}
    for command, records in grouped.items():
        by_query = {v1.normalized(query): (query, options) for query, options in records}
        deduplicated[command] = sorted(
            by_query.values(),
            key=lambda record: (v1.stable_hash(str(SEED), command, record[0]), record[0]),
        )
    eligible = {
        command for command, records in deduplicated.items() if len(records) >= 6
    }
    training_utilities, validation_utilities, test_utilities = stratified_split(
        eligible, set(product_documents)
    )
    documents = {command: list(values) for command, values in product_documents.items()}
    retained = sum(
        len(text.encode("utf-8")) for values in documents.values() for text in values
    )
    for command, records in sorted(deduplicated.items()):
        if command not in documents:
            document = v2.auxiliary_document(
                command, [options for _query, options in records]
            )
            retained += len(document.encode("utf-8"))
            if retained > v2.TEXT_RETAINED_BYTES_MAX:
                raise RuntimeError("training documents exceed their retained-text bound")
            documents[command] = [document]
    training_queries: dict[str, list[str]] = {}
    for command in sorted(training_utilities):
        training_queries[command] = [
            query
            for query, _options in deduplicated[command][
                : v2.EXAMPLES_PER_UTILITY_MAX
            ]
        ]
    for command in sorted(product_documents):
        if command in validation_utilities or command in test_utilities:
            continue
        phrases = v1.catalog_phrases(root_titles[command], root_bodies[command])
        if phrases:
            combined = training_queries.get(command, []) + phrases
            training_queries[command] = list(dict.fromkeys(combined))[
                : v2.EXAMPLES_PER_UTILITY_MAX
            ]
    validation = [
        v2.RetrievalExample(query, command, "nl2bash_validation_utility")
        for command in sorted(validation_utilities)
        for query, _options in deduplicated[command][
            : v2.EVALUATION_EXAMPLES_PER_UTILITY_MAX
        ]
    ]
    test = [
        v2.RetrievalExample(query, command, "nl2bash_test_utility")
        for command in sorted(test_utilities)
        for query, _options in deduplicated[command][
            : v2.EVALUATION_EXAMPLES_PER_UTILITY_MAX
        ]
    ]
    train_text = {
        v1.normalized(query) for values in training_queries.values() for query in values
    }
    validation_text = {v1.normalized(example.query) for example in validation}
    test_text = {v1.normalized(example.query) for example in test}
    if train_text & validation_text or train_text & test_text or validation_text & test_text:
        raise RuntimeError("normalized query text overlaps stratified partitions")
    metadata = {
        "revision": v1.NL2BASH_REVISION,
        "file_sha256": v1.NL2BASH_FILES,
        "source_pairs": len(queries),
        "accepted_single_utility_pairs": sum(len(records) for records in deduplicated.values()),
        "utilities": len(deduplicated),
        "eligible_utilities": len(eligible),
        "training_utilities": sorted(training_utilities),
        "validation_utilities": sorted(validation_utilities),
        "test_utilities": sorted(test_utilities),
        "product_validation_utilities": sorted(validation_utilities & set(product_documents)),
        "product_test_utilities": sorted(test_utilities & set(product_documents)),
        "training_groups_with_catalog_auxiliaries": len(training_queries),
        "training_queries": sum(len(values) for values in training_queries.values()),
        "validation_queries": len(validation),
        "test_queries": len(test),
        "training_documents": sum(len(values) for values in documents.values()),
    }
    return documents, training_queries, validation, test, metadata


def diagnostic_partitions(
    examples: list[v1.RetrievalExample],
) -> tuple[list[v1.RetrievalExample], list[v1.RetrievalExample]]:
    validation_commands = {"ls", "mkdir", "tail"}
    test_commands = {"dig", "rm", "rmdir"}
    validation = [example for example in examples if example.command in validation_commands]
    test = [example for example in examples if example.command in test_commands]
    if not validation or not test:
        raise RuntimeError("diagnostic fixture cannot satisfy both product fixture contracts")
    return validation, test


def pipeline_identity() -> dict[str, Any]:
    directory = Path(__file__).resolve().parent
    files = {}
    for name in ("train_v3.py", "train_v2.py", "train.py", "pyproject.toml", "uv.lock"):
        size, digest = v1.file_sha256(directory / name, 1024 * 1024)
        files[name] = {"bytes": size, "sha256": digest}
    return {"schema_version": TRAINING_SCHEMA_VERSION, "files": files}


def save_int8_candidate(
    model: v2.AnchoredStaticModel,
    directory: Path,
    metadata: dict[str, Any],
    revision: str,
) -> dict[str, Any]:
    exported = model.export(metadata)
    quantized = quantize_model(exported, quantize_to="int8")
    quantized.config["quirl_command_quantization"] = "global_int8_cosine"
    quantized.save_pretrained(directory, model_name=directory.name)
    identity = v1.write_manifest(
        directory, "niklas-heer/quirl-command-v3-int8", revision
    )
    probe = StaticModel.from_pretrained(directory).encode(
        ["list files", "show current directory"], use_multiprocessing=False
    )
    if probe.shape != (2, 256) or not np.isfinite(probe).all():
        raise RuntimeError(f"candidate failed its reload probe: {directory}")
    return {"path": str(directory), "identity": identity}


def candidate_revision(
    name: str,
    trial: dict[str, Any],
    best_epoch: int,
    database_sha256: str,
    dataset: dict[str, Any],
    pipeline: dict[str, Any],
) -> str:
    material = json.dumps(
        {
            "schema_version": TRAINING_SCHEMA_VERSION,
            "seed": SEED,
            "name": name,
            "trial": trial,
            "best_epoch": best_epoch,
            "database_sha256": database_sha256,
            "dataset": dataset,
            "training_pipeline": pipeline,
        },
        sort_keys=True,
        separators=(",", ":"),
    )
    return "quirl-command-v3-" + hashlib.sha256(material.encode("utf-8")).hexdigest()[:16]


def main() -> int:
    arguments = parse_arguments()
    if not 40 <= arguments.epochs <= v2.EPOCHS_MAX:
        raise RuntimeError(f"epochs must be between 40 and {v2.EPOCHS_MAX}")
    torch.set_num_threads(min(8, os.cpu_count() or 1))
    torch.use_deterministic_algorithms(True)
    output = v1.prepare_output(arguments.output)
    candidates_directory = output / "candidates"
    candidates_directory.mkdir(mode=0o700)
    database_identity = v1.file_sha256(arguments.database, v2.DATABASE_BYTES_MAX)
    fixture_identity = v1.file_sha256(arguments.fixture, 1024 * 1024)
    source_identity = v1.model_source_identity(arguments.model)
    training_pipeline = pipeline_identity()
    product_documents, root_titles, root_bodies = v2.read_product_documents(
        arguments.database
    )
    diagnostic_fixture, _unseen = v1.read_fixture(arguments.fixture, root_bodies)
    documents, training_queries, validation, test, dataset = load_corpus(
        arguments.dataset_cache, product_documents, root_titles, root_bodies
    )
    v2.assert_fixture_is_diagnostic_only(diagnostic_fixture, training_queries)
    diagnostic_validation, diagnostic_test = diagnostic_partitions(diagnostic_fixture)
    validation_fixture_directory = output / "validation-fixture"
    validation_fixture_directory.mkdir(mode=0o700)
    test_fixture_directory = output / "test-fixture"
    test_fixture_directory.mkdir(mode=0o700)
    validation_fixture = v2.write_product_fixture(
        validation_fixture_directory,
        validation,
        set(product_documents),
        diagnostic_validation,
    )
    test_fixture = v2.write_product_fixture(
        test_fixture_directory, test, set(product_documents), diagnostic_test
    )
    source = StaticModel.from_pretrained(arguments.model)
    if len(source.tokens) > v1.VOCABULARY_MAX or source.embedding.shape != (29528, 256):
        raise RuntimeError("source model has an unexpected vocabulary or dimension identity")
    training = v2.select_positive_documents(source, documents, training_queries)
    hard_negatives = v2.mine_hard_negatives(source, documents, training)
    baseline = v2.AnchoredStaticModel(source, trainable=False)
    report: dict[str, Any] = {
        "schema_version": TRAINING_SCHEMA_VERSION,
        "seed": SEED,
        "selection_rule": (
            "external production validation fixture; test fixture remains unread until selection"
        ),
        "training_device": "cpu",
        "database": {
            "path": str(arguments.database.resolve()),
            "bytes": database_identity[0],
            "sha256": database_identity[1],
            "product_commands": len(product_documents),
            "product_documents": sum(len(values) for values in product_documents.values()),
        },
        "diagnostic_fixture": {
            "path": str(arguments.fixture.resolve()),
            "bytes": fixture_identity[0],
            "sha256": fixture_identity[1],
            "role": "partitioned multilingual/destructive fixture support only",
        },
        "source_model": source_identity,
        "training_pipeline": training_pipeline,
        "dataset": dataset,
        "semantic_baseline_validation": v2.evaluate(baseline, documents, validation),
        "validation_fixture": validation_fixture,
        "test_fixture": test_fixture,
        "test_metrics": "intentionally absent before external candidate selection",
        "candidates": [],
    }
    stock_revision = candidate_revision(
        "stock-int8", {}, 0, database_identity[1], dataset, training_pipeline
    )
    stock_metadata = {
        "schema_version": TRAINING_SCHEMA_VERSION,
        "seed": SEED,
        "candidate": "stock-int8",
        "control": True,
        "source_repository": v1.SOURCE_REPOSITORY,
        "source_revision": v1.SOURCE_REVISION,
        "source_file_sha256": v1.SOURCE_FILE_SHA256,
        "training_pipeline": training_pipeline,
        "split_identity": {
            "validation_utilities": dataset["validation_utilities"],
            "test_utilities": dataset["test_utilities"],
        },
    }
    stock_artifact = save_int8_candidate(
        baseline,
        candidates_directory / "stock-int8",
        stock_metadata,
        stock_revision,
    )
    report["candidates"].append(
        {
            "name": "stock-int8",
            "revision": stock_revision,
            "semantic_validation": report["semantic_baseline_validation"],
            "artifact": stock_artifact,
        }
    )
    trials = [
        v2.Trial("mild-lr-0.001-a0.10-d0.10", 0.001, 0.10, 0.10),
        v2.Trial("mild-lr-0.002-a0.10-d0.08", 0.002, 0.10, 0.08),
        v2.Trial("mild-lr-0.004-a0.10-d0.05", 0.004, 0.10, 0.05),
        v2.Trial("mild-lr-0.006-a0.15-d0.05", 0.006, 0.15, 0.05),
        v2.Trial("mild-lr-0.008-a0.20-d0.08", 0.008, 0.20, 0.08),
        v2.Trial("mild-lr-0.012-a0.25-d0.10", 0.012, 0.25, 0.10),
    ]
    if len(trials) + 1 > CANDIDATES_MAX:
        raise RuntimeError("candidate count exceeds its configured bound")
    original_seed = v2.SEED
    v2.SEED = SEED
    try:
        for trial in trials:
            print(f"training {trial.name}", file=sys.stderr, flush=True)
            model, trial_report = v2.train_trial(
                source,
                trial,
                training,
                documents,
                validation,
                hard_negatives,
                arguments.epochs,
            )
            revision = candidate_revision(
                trial.name,
                asdict(trial),
                trial_report["best_epoch"],
                database_identity[1],
                dataset,
                training_pipeline,
            )
            metadata = {
                "schema_version": TRAINING_SCHEMA_VERSION,
                "seed": SEED,
                "candidate": trial.name,
                "source_repository": v1.SOURCE_REPOSITORY,
                "source_revision": v1.SOURCE_REVISION,
                "source_file_sha256": v1.SOURCE_FILE_SHA256,
                "training_pipeline": training_pipeline,
                "split_identity": {
                    "training_utilities": dataset["training_utilities"],
                    "validation_utilities": dataset["validation_utilities"],
                    "test_utilities": dataset["test_utilities"],
                },
                "trial": asdict(trial),
                "best_epoch": trial_report["best_epoch"],
            }
            artifact = save_int8_candidate(
                model, candidates_directory / trial.name, metadata, revision
            )
            report["candidates"].append(
                {
                    "name": trial.name,
                    "revision": revision,
                    "trial_report": trial_report,
                    "artifact": artifact,
                }
            )
    finally:
        v2.SEED = original_seed
    report_path = output / "training-report.json"
    report_path.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(
        json.dumps(
            {
                "report": str(report_path),
                "validation_fixture": validation_fixture,
                "test_fixture": test_fixture,
                "candidates": [
                    {
                        "name": candidate["name"],
                        "revision": candidate["revision"],
                        "path": candidate["artifact"]["path"],
                    }
                    for candidate in report["candidates"]
                ],
            },
            indent=2,
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, sqlite3.Error, ValueError) as error:
        print(f"retrieval training v3: {error}", file=sys.stderr)
        raise SystemExit(1) from None
