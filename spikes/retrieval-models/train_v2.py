#!/usr/bin/env python3
"""Train Quirl's promotion candidate with whole-utility holdouts and anchors."""

from __future__ import annotations

import argparse
from collections import defaultdict
from dataclasses import asdict, dataclass
import hashlib
import json
import math
import os
from pathlib import Path
import random
import re
import shlex
import sqlite3
import statistics
import sys
import time
from typing import Any

from model2vec import StaticModel
from model2vec.model import quantize_model
import numpy as np
import torch
from torch import nn
from torch.nn import functional as F

import train as v1


TRAINING_SCHEMA_VERSION = 2
SEED = 20260819
TOKENS_MAX = 256
DATABASE_BYTES_MAX = 128 * 1024 * 1024
DOCUMENTS_MAX = 4_096
DOCUMENTS_PER_COMMAND_MAX = 256
DOCUMENT_BYTES_MAX = 16 * 1024
TEXT_RETAINED_BYTES_MAX = 48 * 1024 * 1024
QUERY_BYTES_MAX = 4 * 1024
UTILITIES_MAX = 256
EXAMPLES_PER_UTILITY_MAX = 96
EVALUATION_EXAMPLES_PER_UTILITY_MAX = 64
HARD_NEGATIVES_MAX = 6
TRIALS_MAX = 6
EPOCHS_MAX = 320
BATCH_COMMANDS_MAX = 32
VALIDATION_INTERVAL = 10
PATIENCE_INTERVALS = 7
DELTA_ABS_MAX = 2.0
GENERATED_FIXTURE_QUERIES_MAX = 256
OPTION_PATTERN = re.compile(r"^--?[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
UTILITY_PATTERN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9+._-]{0,127}$")
FORCED_TEST_UTILITIES = frozenset(("dig", "rm", "rmdir"))


@dataclass(frozen=True)
class RetrievalExample:
    """One query whose expected result is a complete utility group."""

    query: str
    command: str
    slice: str


@dataclass(frozen=True)
class TrainingPair:
    """One query paired with a selected production-shaped positive document."""

    query: str
    positive: str


@dataclass(frozen=True)
class Trial:
    """One bounded anchored token-weight optimization configuration."""

    name: str
    learning_rate: float
    anchor_strength: float
    delta_strength: float


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
    parser.add_argument("--epochs", type=int, default=280)
    return parser.parse_args()


def read_product_documents(
    database: Path,
) -> tuple[dict[str, list[str]], dict[str, str], dict[str, str]]:
    v1.file_sha256(database, DATABASE_BYTES_MAX)
    uri = f"file:{database.resolve().as_posix()}?mode=ro&immutable=1"
    connection = sqlite3.connect(uri, uri=True)
    try:
        rows = connection.execute(
            "SELECT command_id, document_kind, title, body FROM semantic_documents "
            "ORDER BY command_id, document_id"
        ).fetchall()
    finally:
        connection.close()
    if not rows or len(rows) > DOCUMENTS_MAX:
        raise RuntimeError(f"semantic document count is outside 1..{DOCUMENTS_MAX}")
    documents: dict[str, list[str]] = defaultdict(list)
    root_titles: dict[str, str] = {}
    root_bodies: dict[str, str] = {}
    retained = 0
    for command, kind, title, body in rows:
        if not all(isinstance(value, str) for value in (command, kind, title, body)):
            raise RuntimeError("semantic document contains a non-text field")
        text = f"{title}\n{body}"
        text_bytes = len(text.encode("utf-8"))
        if not command or text_bytes > DOCUMENT_BYTES_MAX:
            raise RuntimeError(f"semantic document violates its byte bound: {command}")
        retained += text_bytes
        if retained > TEXT_RETAINED_BYTES_MAX:
            raise RuntimeError("semantic documents exceed their retained-text bound")
        if len(documents[command]) >= DOCUMENTS_PER_COMMAND_MAX:
            raise RuntimeError(f"command exceeds its document-count bound: {command}")
        documents[command].append(text)
        if kind == "command":
            if command in root_bodies:
                raise RuntimeError(f"duplicate root command document: {command}")
            root_titles[command] = title
            root_bodies[command] = body
    if set(documents) != set(root_bodies):
        raise RuntimeError("every document command must have exactly one root document")
    return dict(documents), root_titles, root_bodies


def option_spellings(command_line: str, utility: str) -> tuple[str, ...]:
    try:
        tokens = shlex.split(command_line, posix=True)
    except ValueError:
        return ()
    utility_index = next(
        (
            index
            for index, token in enumerate(tokens)
            if Path(token.lstrip("\\")).name == utility
        ),
        None,
    )
    if utility_index is None:
        return ()
    options = set()
    for token in tokens[utility_index + 1 :]:
        option = token.partition("=")[0]
        if OPTION_PATTERN.fullmatch(option):
            options.add(option)
    return tuple(sorted(options))[:32]


def auxiliary_document(utility: str, option_sets: list[tuple[str, ...]]) -> str:
    if not UTILITY_PATTERN.fullmatch(utility):
        raise RuntimeError(f"utility name is unsafe for an auxiliary document: {utility!r}")
    options = sorted({option for values in option_sets for option in values})[:64]
    text = f"Command: {utility}. Use the {utility} command."
    if options:
        text += " Recognized options: " + ", ".join(options) + "."
    if len(text.encode("utf-8")) > DOCUMENT_BYTES_MAX:
        raise RuntimeError(f"auxiliary document exceeds its byte bound: {utility}")
    return text


def split_utilities(eligible: set[str]) -> tuple[set[str], set[str], set[str]]:
    forced = FORCED_TEST_UTILITIES & eligible
    ordered = sorted(
        eligible - forced,
        key=lambda command: (v1.stable_hash(str(SEED), "split", command), command),
    )
    test_count = max(len(forced), math.ceil(len(eligible) * 0.18))
    test = forced | set(ordered[: test_count - len(forced)])
    remaining = [command for command in ordered if command not in test]
    validation_count = max(8, math.ceil(len(eligible) * 0.18))
    validation = set(remaining[:validation_count])
    training = eligible - validation - test
    if not training or not validation or not test:
        raise RuntimeError("whole-utility split produced an empty partition")
    if training & validation or training & test or validation & test:
        raise RuntimeError("whole-utility partitions overlap")
    return training, validation, test


def load_corpus(
    cache: Path,
    product_documents: dict[str, list[str]],
    root_titles: dict[str, str],
    root_bodies: dict[str, str],
) -> tuple[
    dict[str, list[str]],
    dict[str, list[str]],
    list[RetrievalExample],
    list[RetrievalExample],
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
            and UTILITY_PATTERN.fullmatch(utility)
            and query.strip()
            and len(query.encode("utf-8")) <= QUERY_BYTES_MAX
        ):
            grouped[utility].append((query, option_spellings(command_line, utility)))
    if not grouped or len(grouped) > UTILITIES_MAX:
        raise RuntimeError(f"utility count is outside 1..{UTILITIES_MAX}")
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
    training_utilities, validation_utilities, test_utilities = split_utilities(eligible)
    documents = {command: list(values) for command, values in product_documents.items()}
    retained = sum(
        len(text.encode("utf-8")) for values in documents.values() for text in values
    )
    for command, records in sorted(deduplicated.items()):
        if command not in documents:
            document = auxiliary_document(command, [options for _query, options in records])
            retained += len(document.encode("utf-8"))
            if retained > TEXT_RETAINED_BYTES_MAX:
                raise RuntimeError("training documents exceed their retained-text bound")
            documents[command] = [document]
    training_queries: dict[str, list[str]] = {}
    for command in sorted(training_utilities):
        training_queries[command] = [
            query
            for query, _options in deduplicated[command][
                :EXAMPLES_PER_UTILITY_MAX
            ]
        ]
    for command in sorted(product_documents):
        if command in validation_utilities or command in test_utilities:
            continue
        phrases = v1.catalog_phrases(root_titles[command], root_bodies[command])
        if phrases:
            combined = training_queries.get(command, []) + phrases
            training_queries[command] = list(dict.fromkeys(combined))[
                :EXAMPLES_PER_UTILITY_MAX
            ]
    validation = [
        RetrievalExample(query, command, "nl2bash_validation_utility")
        for command in sorted(validation_utilities)
        for query, _options in deduplicated[command][
            :EVALUATION_EXAMPLES_PER_UTILITY_MAX
        ]
    ]
    test = [
        RetrievalExample(query, command, "nl2bash_test_utility")
        for command in sorted(test_utilities)
        for query, _options in deduplicated[command][
            :EVALUATION_EXAMPLES_PER_UTILITY_MAX
        ]
    ]
    train_text = {
        v1.normalized(query) for values in training_queries.values() for query in values
    }
    validation_text = {v1.normalized(example.query) for example in validation}
    test_text = {v1.normalized(example.query) for example in test}
    if train_text & validation_text or train_text & test_text or validation_text & test_text:
        raise RuntimeError("normalized query text overlaps whole-utility partitions")
    if not training_queries or not validation or not test:
        raise RuntimeError("training corpus produced an empty query partition")
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
        "training_groups_with_catalog_auxiliaries": len(training_queries),
        "training_queries": sum(len(values) for values in training_queries.values()),
        "validation_queries": len(validation),
        "test_queries": len(test),
        "training_documents": sum(len(values) for values in documents.values()),
    }
    return documents, training_queries, validation, test, metadata


def assert_fixture_is_diagnostic_only(
    fixture: list[v1.RetrievalExample], training_queries: dict[str, list[str]]
) -> None:
    trained = {
        v1.normalized(query) for queries in training_queries.values() for query in queries
    }
    for example in fixture:
        if v1.normalized(example.query) in trained:
            raise RuntimeError(f"diagnostic fixture query leaked into training: {example.query}")


class AnchoredStaticModel(nn.Module):
    """Frozen token vectors with bounded dense deltas over source token weights."""

    def __init__(self, source: StaticModel, trainable: bool) -> None:
        super().__init__()
        vectors = torch.from_numpy(np.asarray(source.embedding, dtype=np.float32))
        self.embeddings = nn.Embedding.from_pretrained(vectors, freeze=True)
        mapping = (
            torch.arange(len(source.tokens), dtype=torch.int64)
            if source.token_mapping is None
            else torch.from_numpy(np.asarray(source.token_mapping, dtype=np.int64))
        )
        self.register_buffer("mapping", mapping)
        weights = (
            np.ones(len(source.tokens), dtype=np.float32)
            if source.weights is None
            else np.asarray(source.weights, dtype=np.float32)
        )
        base_log_weights = torch.from_numpy(np.log(np.clip(weights, 1e-4, 1e4)))
        self.register_buffer("base_log_weights", base_log_weights)
        self.delta = nn.Parameter(
            torch.zeros(len(source.tokens), dtype=torch.float32), requires_grad=trainable
        )
        self.source = source

    def token_ids(self, texts: list[str]) -> list[list[int]]:
        return self.source.tokenize(texts, max_length=TOKENS_MAX)

    def encode_ids(self, encoded: list[list[int]]) -> torch.Tensor:
        if not encoded:
            return torch.empty((0, self.embeddings.embedding_dim), dtype=torch.float32)
        width = max(1, max(len(token_ids) for token_ids in encoded))
        ids = torch.zeros((len(encoded), width), dtype=torch.int64)
        mask = torch.zeros((len(encoded), width), dtype=torch.float32)
        for row, token_ids in enumerate(encoded):
            retained = token_ids[:TOKENS_MAX]
            if retained:
                ids[row, : len(retained)] = torch.tensor(retained, dtype=torch.int64)
                mask[row, : len(retained)] = 1.0
        vectors = self.embeddings(self.mapping[ids])
        log_weights = self.base_log_weights[ids] + self.delta[ids].clamp(
            -DELTA_ABS_MAX, DELTA_ABS_MAX
        )
        weights = torch.exp(log_weights.clamp(-4.0, 4.0))
        pooled = (vectors * weights[:, :, None] * mask[:, :, None]).sum(dim=1)
        pooled = pooled / (weights * mask).sum(dim=1).clamp_min(1e-6)[:, None]
        return F.normalize(pooled, dim=1)

    def encode_texts(self, texts: list[str], batch_size: int = 256) -> torch.Tensor:
        if not texts:
            return torch.empty((0, self.embeddings.embedding_dim), dtype=torch.float32)
        return torch.cat(
            [
                self.encode_ids(self.token_ids(texts[start : start + batch_size]))
                for start in range(0, len(texts), batch_size)
            ]
        )

    def export(self, metadata: dict[str, Any]) -> StaticModel:
        config = dict(self.source.config)
        config["quirl_command_tuning"] = metadata
        log_weights = self.base_log_weights + self.delta.detach().clamp(
            -DELTA_ABS_MAX, DELTA_ABS_MAX
        )
        weights = torch.exp(log_weights.clamp(-4.0, 4.0)).numpy()
        return StaticModel(
            vectors=np.asarray(self.source.embedding, dtype=np.float32),
            tokenizer=self.source.tokenizer,
            config=config,
            normalize=True,
            base_model_name=self.source.base_model_name,
            language=self.source.language,
            weights=weights,
            token_mapping=self.source.token_mapping,
        )


def retrieval_metrics(ranks: list[int]) -> dict[str, float | int]:
    if not ranks:
        raise RuntimeError("cannot score an empty retrieval slice")
    return {
        "queries": len(ranks),
        "recall_at_1": sum(rank <= 1 for rank in ranks) / len(ranks),
        "recall_at_5": sum(rank <= 5 for rank in ranks) / len(ranks),
        "recall_at_10": sum(rank <= 10 for rank in ranks) / len(ranks),
        "mrr": sum(1.0 / rank for rank in ranks) / len(ranks),
        "median_rank": statistics.median(ranks),
    }


def flatten_documents(
    documents: dict[str, list[str]],
) -> tuple[list[str], list[str], dict[str, list[int]]]:
    commands = []
    texts = []
    indexes: dict[str, list[int]] = defaultdict(list)
    for command, values in sorted(documents.items()):
        for text in values:
            indexes[command].append(len(texts))
            commands.append(command)
            texts.append(text)
    return commands, texts, dict(indexes)


@torch.no_grad()
def evaluate(
    model: AnchoredStaticModel,
    documents: dict[str, list[str]],
    examples: list[RetrievalExample] | list[v1.RetrievalExample],
) -> dict[str, Any]:
    command_names = sorted(documents)
    _document_commands, texts, indexes = flatten_documents(documents)
    document_vectors = model.encode_texts(texts)
    slices: dict[str, list[int]] = defaultdict(list)
    all_ranks = []
    for start in range(0, len(examples), 256):
        batch = examples[start : start + 256]
        query_vectors = model.encode_texts([example.query for example in batch])
        scores = query_vectors @ document_vectors.T
        for row, example in enumerate(batch):
            if example.command not in indexes:
                raise RuntimeError(f"evaluation target has no documents: {example.command}")
            command_scores = {
                command: float(scores[row, document_indexes].max())
                for command, document_indexes in indexes.items()
            }
            expected = command_scores[example.command]
            rank = 1 + sum(
                score > expected
                or (score == expected and command < example.command)
                for command, score in command_scores.items()
                if command != example.command
            )
            all_ranks.append(rank)
            slices[example.slice].append(rank)
    return {
        "overall": retrieval_metrics(all_ranks),
        "slices": {name: retrieval_metrics(ranks) for name, ranks in sorted(slices.items())},
        "commands": len(command_names),
        "documents": len(texts),
    }


@torch.no_grad()
def select_positive_documents(
    source: StaticModel,
    documents: dict[str, list[str]],
    queries: dict[str, list[str]],
) -> dict[str, list[TrainingPair]]:
    selected = {}
    for command, command_queries in sorted(queries.items()):
        values = documents[command]
        document_vectors = source.encode(
            values, max_length=TOKENS_MAX, batch_size=128, use_multiprocessing=False
        )
        query_vectors = source.encode(
            command_queries,
            max_length=TOKENS_MAX,
            batch_size=128,
            use_multiprocessing=False,
        )
        scores = query_vectors @ document_vectors.T
        selected[command] = [
            TrainingPair(query, values[int(np.argmax(scores[index]))])
            for index, query in enumerate(command_queries)
        ]
    return selected


@torch.no_grad()
def mine_hard_negatives(
    source: StaticModel,
    documents: dict[str, list[str]],
    training: dict[str, list[TrainingPair]],
) -> dict[str, list[str]]:
    commands = sorted(documents)
    centroids = []
    for command in commands:
        vectors = source.encode(
            documents[command],
            max_length=TOKENS_MAX,
            batch_size=128,
            use_multiprocessing=False,
        )
        centroid = np.mean(vectors, axis=0)
        centroid /= max(np.linalg.norm(centroid), 1e-12)
        centroids.append(centroid)
    command_vectors = np.stack(centroids)
    negatives = {}
    for command, pairs in sorted(training.items()):
        query_vectors = source.encode(
            [pair.query for pair in pairs[:16]],
            max_length=TOKENS_MAX,
            batch_size=128,
            use_multiprocessing=False,
        )
        scores = command_vectors @ np.mean(query_vectors, axis=0)
        order = sorted(range(len(commands)), key=lambda index: (-scores[index], commands[index]))
        negatives[command] = [
            candidate for index in order if (candidate := commands[index]) != command
        ][:HARD_NEGATIVES_MAX]
    return negatives


def train_trial(
    source: StaticModel,
    trial: Trial,
    training: dict[str, list[TrainingPair]],
    documents: dict[str, list[str]],
    validation: list[RetrievalExample],
    hard_negatives: dict[str, list[str]],
    epochs: int,
) -> tuple[AnchoredStaticModel, dict[str, Any]]:
    torch.manual_seed(SEED)
    randomizer = random.Random(SEED)
    model = AnchoredStaticModel(source, trainable=True)
    baseline = AnchoredStaticModel(source, trainable=False)
    optimizer = torch.optim.AdamW([model.delta], lr=trial.learning_rate, weight_decay=0.0)
    commands = sorted(training)
    best_key = (-1.0, -1.0, -1.0)
    best_delta: torch.Tensor | None = None
    best_epoch = 0
    stale_intervals = 0
    losses = []
    validation_history = []
    started = time.perf_counter()
    for epoch in range(1, epochs + 1):
        shuffled = list(commands)
        randomizer.shuffle(shuffled)
        epoch_losses = []
        for offset in range(0, len(shuffled), BATCH_COMMANDS_MAX):
            batch_commands = shuffled[offset : offset + BATCH_COMMANDS_MAX]
            pairs = [
                training[command][(epoch - 1) % len(training[command])]
                for command in batch_commands
            ]
            negative_commands = []
            batch_set = set(batch_commands)
            for command in batch_commands:
                for negative in hard_negatives[command]:
                    if negative not in batch_set and negative not in negative_commands:
                        negative_commands.append(negative)
            negative_texts = [documents[command][0] for command in negative_commands]
            query_ids = model.token_ids([pair.query for pair in pairs])
            positive_ids = model.token_ids([pair.positive for pair in pairs])
            negative_ids = model.token_ids(negative_texts)
            query_vectors = model.encode_ids(query_ids)
            positive_vectors = model.encode_ids(positive_ids)
            candidates = (
                torch.cat([positive_vectors, model.encode_ids(negative_ids)], dim=0)
                if negative_ids
                else positive_vectors
            )
            labels = torch.arange(len(batch_commands), dtype=torch.int64)
            query_loss = F.cross_entropy(query_vectors @ candidates.T / 0.05, labels)
            symmetric_loss = F.cross_entropy(positive_vectors @ query_vectors.T / 0.05, labels)
            with torch.no_grad():
                baseline_queries = baseline.encode_ids(query_ids)
                baseline_candidates = torch.cat(
                    [baseline.encode_ids(positive_ids), baseline.encode_ids(negative_ids)], dim=0
                )
            anchor_loss = (
                (1.0 - (query_vectors * baseline_queries).sum(dim=1)).mean()
                + (1.0 - (candidates * baseline_candidates).sum(dim=1)).mean()
            ) / 2.0
            delta_loss = model.delta.square().mean()
            loss = (
                (query_loss + symmetric_loss) / 2.0
                + trial.anchor_strength * anchor_loss
                + trial.delta_strength * delta_loss
            )
            optimizer.zero_grad(set_to_none=True)
            loss.backward()
            if model.delta.grad is not None:
                model.delta.grad.clamp_(-2.0, 2.0)
            optimizer.step()
            with torch.no_grad():
                model.delta.clamp_(-DELTA_ABS_MAX, DELTA_ABS_MAX)
            observed_loss = float(loss.detach())
            if not math.isfinite(observed_loss):
                raise RuntimeError(f"non-finite loss in trial {trial.name}")
            epoch_losses.append(observed_loss)
        losses.append(statistics.mean(epoch_losses))
        if epoch % VALIDATION_INTERVAL == 0 or epoch == epochs:
            score = evaluate(model, documents, validation)
            overall = score["overall"]
            key = (overall["recall_at_10"], overall["mrr"], overall["recall_at_1"])
            validation_history.append({"epoch": epoch, **overall})
            if key > best_key:
                best_key = key
                best_epoch = epoch
                best_delta = model.delta.detach().clone()
                stale_intervals = 0
            else:
                stale_intervals += 1
                if stale_intervals >= PATIENCE_INTERVALS:
                    break
    if best_delta is None:
        raise RuntimeError(f"trial did not produce a checkpoint: {trial.name}")
    with torch.no_grad():
        model.delta.copy_(best_delta)
    return model, {
        "trial": asdict(trial),
        "best_epoch": best_epoch,
        "epochs_run": len(losses),
        "initial_loss": losses[0],
        "final_loss": losses[-1],
        "training_seconds": time.perf_counter() - started,
        "delta_abs_max": float(model.delta.detach().abs().max()),
        "delta_rms": float(model.delta.detach().square().mean().sqrt()),
        "validation_history": validation_history,
        "best_validation": evaluate(model, documents, validation),
    }


def pipeline_identity() -> dict[str, Any]:
    directory = Path(__file__).resolve().parent
    files = {}
    for name in ("train_v2.py", "train.py", "pyproject.toml", "uv.lock"):
        size, digest = v1.file_sha256(directory / name, 1024 * 1024)
        files[name] = {"bytes": size, "sha256": digest}
    return {"schema_version": TRAINING_SCHEMA_VERSION, "files": files}


def save_candidate(
    model: AnchoredStaticModel,
    output: Path,
    metadata: dict[str, Any],
    revision: str,
) -> dict[str, Any]:
    exported = model.export(metadata)
    float_path = output / "selected-f32"
    exported.save_pretrained(float_path, model_name=float_path.name)
    float_identity = v1.write_manifest(
        float_path, "niklas-heer/quirl-command-v2", revision
    )
    int8_path = output / "selected-int8"
    quantized = quantize_model(exported, quantize_to="int8")
    quantized.config["quirl_command_quantization"] = "global_int8_cosine"
    quantized.save_pretrained(int8_path, model_name=int8_path.name)
    int8_identity = v1.write_manifest(
        int8_path, "niklas-heer/quirl-command-v2-int8", revision
    )
    for path in (float_path, int8_path):
        probe = StaticModel.from_pretrained(path).encode(
            ["list files", "show current directory"], use_multiprocessing=False
        )
        if probe.shape != (2, 256) or not np.isfinite(probe).all():
            raise RuntimeError(f"exported candidate failed its reload probe: {path}")
    return {
        "f32_path": str(float_path),
        "f32_identity": float_identity,
        "int8_path": str(int8_path),
        "int8_identity": int8_identity,
    }


def write_product_fixture(
    output: Path,
    test: list[RetrievalExample],
    product_commands: set[str],
    diagnostic_fixture: list[v1.RetrievalExample],
) -> dict[str, Any]:
    def group_id(command: str) -> str:
        return "utility_" + re.sub(r"[^a-z0-9_]+", "_", command.casefold()).strip("_")

    destructive = {
        example.command
        for example in diagnostic_fixture
        if example.command in {"mkdir", "rm", "rmdir"}
    }
    grouped: dict[str, list[str]] = defaultdict(list)
    for example in test:
        if example.command in product_commands and len(example.query.split()) >= 6:
            grouped[example.command].append(example.query)
    groups = set(grouped)
    queries = []
    for command in sorted(groups):
        for index, query in enumerate(grouped[command][:8]):
            queries.append(
                {
                    "id": f"test_{group_id(command)}_{index}",
                    "command_group": group_id(command),
                    "language": "en",
                    "text": query,
                }
            )
    diagnostic_index = 0
    for example in diagnostic_fixture:
        language = example.slice.rsplit(":", 1)[-1]
        if (
            language == "en"
            or example.command not in product_commands
            or len(example.query.split()) < 6
        ):
            continue
        groups.add(example.command)
        queries.append(
            {
                "id": f"diagnostic_{diagnostic_index}_{language}",
                "command_group": group_id(example.command),
                "language": language,
                "text": example.query,
            }
        )
        diagnostic_index += 1
    if not queries or len(queries) > GENERATED_FIXTURE_QUERIES_MAX:
        raise RuntimeError("generated product fixture violates its query-count bound")
    value = {
        "schema_version": 1,
        "name": "quirl_retrieval_v2_generated_holdout",
        "description": (
            "Generated from pinned whole-utility NL2Bash test groups after model selection; "
            "ignored research output, never indexed or shipped."
        ),
        "splits": [
            {
                "name": "unseen_command",
                "command_groups": [
                    group_id(command) for command in sorted(groups)
                ],
            }
        ],
        "command_groups": [
            {
                "id": group_id(command),
                "expected_targets": [command],
                "destructive": command in destructive,
            }
            for command in sorted(groups)
        ],
        "queries": queries,
    }
    path = output / "product-holdout-fixture.json"
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return {"path": str(path), "groups": len(groups), "queries": len(queries)}


def main() -> int:
    arguments = parse_arguments()
    if not 40 <= arguments.epochs <= EPOCHS_MAX:
        raise RuntimeError(f"epochs must be between 40 and {EPOCHS_MAX}")
    torch.set_num_threads(min(8, os.cpu_count() or 1))
    torch.use_deterministic_algorithms(True)
    output = v1.prepare_output(arguments.output)
    database_identity = v1.file_sha256(arguments.database, DATABASE_BYTES_MAX)
    fixture_identity = v1.file_sha256(arguments.fixture, 1024 * 1024)
    source_identity = v1.model_source_identity(arguments.model)
    training_pipeline = pipeline_identity()
    product_documents, root_titles, root_bodies = read_product_documents(arguments.database)
    diagnostic_fixture, _unseen = v1.read_fixture(arguments.fixture, root_bodies)
    documents, training_queries, validation, test, dataset = load_corpus(
        arguments.dataset_cache, product_documents, root_titles, root_bodies
    )
    assert_fixture_is_diagnostic_only(diagnostic_fixture, training_queries)
    source = StaticModel.from_pretrained(arguments.model)
    if len(source.tokens) > v1.VOCABULARY_MAX or source.embedding.shape != (29528, 256):
        raise RuntimeError("source model has an unexpected vocabulary or dimension identity")
    training = select_positive_documents(source, documents, training_queries)
    hard_negatives = mine_hard_negatives(source, documents, training)
    baseline = AnchoredStaticModel(source, trainable=False)
    report: dict[str, Any] = {
        "schema_version": TRAINING_SCHEMA_VERSION,
        "seed": SEED,
        "selection_rule": "whole-utility validation Recall@10, then MRR, then Recall@1",
        "test_access_rule": "only the selected validation winner is test-scored",
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
            "queries": len(diagnostic_fixture),
            "role": "post-selection diagnostic only; previously observed in v1",
            "leakage_check": "passed",
        },
        "source_model": source_identity,
        "training_pipeline": training_pipeline,
        "dataset": dataset,
        "baseline": {
            "validation": evaluate(baseline, documents, validation),
        },
        "trials": [],
    }
    trials = [
        Trial("anchor-lr-0.008-a0.05-d0.01", 0.008, 0.05, 0.01),
        Trial("anchor-lr-0.015-a0.05-d0.01", 0.015, 0.05, 0.01),
        Trial("anchor-lr-0.03-a0.10-d0.02", 0.03, 0.10, 0.02),
        Trial("anchor-lr-0.05-a0.15-d0.03", 0.05, 0.15, 0.03),
        Trial("anchor-lr-0.03-a0.25-d0.05", 0.03, 0.25, 0.05),
        Trial("anchor-lr-0.06-a0.30-d0.08", 0.06, 0.30, 0.08),
    ]
    if len(trials) > TRIALS_MAX:
        raise RuntimeError("trial count exceeds its configured bound")
    selected_key = (-1.0, -1.0, -1.0)
    selected_model: AnchoredStaticModel | None = None
    selected_report: dict[str, Any] | None = None
    for trial in trials:
        print(f"training {trial.name}", file=sys.stderr, flush=True)
        model, trial_report = train_trial(
            source,
            trial,
            training,
            documents,
            validation,
            hard_negatives,
            arguments.epochs,
        )
        report["trials"].append(trial_report)
        score = trial_report["best_validation"]["overall"]
        key = (score["recall_at_10"], score["mrr"], score["recall_at_1"])
        if key > selected_key:
            selected_key = key
            selected_model = model
            selected_report = trial_report
    if selected_model is None or selected_report is None:
        raise RuntimeError("training sweep did not select a candidate")
    baseline_test = evaluate(baseline, documents, test)
    selected_test = evaluate(selected_model, documents, test)
    baseline_diagnostic = evaluate(baseline, product_documents, diagnostic_fixture)
    selected_diagnostic = evaluate(selected_model, product_documents, diagnostic_fixture)
    identity_material = json.dumps(
        {
            "schema_version": TRAINING_SCHEMA_VERSION,
            "seed": SEED,
            "database_sha256": database_identity[1],
            "source": source_identity["files"],
            "training_pipeline": training_pipeline,
            "dataset": dataset,
            "selected_trial": selected_report["trial"],
            "best_epoch": selected_report["best_epoch"],
        },
        sort_keys=True,
        separators=(",", ":"),
    )
    revision = "quirl-command-v2-" + hashlib.sha256(
        identity_material.encode("utf-8")
    ).hexdigest()[:16]
    report["baseline"].update(
        {"test": baseline_test, "diagnostic_fixture": baseline_diagnostic}
    )
    report["selected"] = {
        "trial": selected_report["trial"],
        "best_epoch": selected_report["best_epoch"],
        "revision": revision,
        "validation": selected_report["best_validation"],
        "test": selected_test,
        "diagnostic_fixture": selected_diagnostic,
        "delta_abs_max": selected_report["delta_abs_max"],
        "delta_rms": selected_report["delta_rms"],
    }
    report["artifacts"] = save_candidate(
        selected_model,
        output,
        {
            "schema_version": TRAINING_SCHEMA_VERSION,
            "seed": SEED,
            "dataset_revision": v1.NL2BASH_REVISION,
            "database_sha256": database_identity[1],
            "source_repository": v1.SOURCE_REPOSITORY,
            "source_revision": v1.SOURCE_REVISION,
            "source_file_sha256": v1.SOURCE_FILE_SHA256,
            "training_pipeline": training_pipeline,
            "split_identity": {
                "training_utilities": dataset["training_utilities"],
                "validation_utilities": dataset["validation_utilities"],
                "test_utilities": dataset["test_utilities"],
            },
            "trial": selected_report["trial"],
            "best_epoch": selected_report["best_epoch"],
        },
        revision,
    )
    report["generated_product_fixture"] = write_product_fixture(
        output, test, set(product_documents), diagnostic_fixture
    )
    report_path = output / "training-report.json"
    report_path.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(
        json.dumps(
            {
                "report": str(report_path),
                "selected": report["selected"],
                "artifacts": report["artifacts"],
                "generated_product_fixture": report["generated_product_fixture"],
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
        print(f"retrieval training v2: {error}", file=sys.stderr)
        raise SystemExit(1) from None
