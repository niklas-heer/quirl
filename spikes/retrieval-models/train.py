#!/usr/bin/env python3
"""Tune potion-base-8M token weights for bounded Quirl command retrieval."""

from __future__ import annotations

import argparse
from collections import defaultdict
from dataclasses import asdict, dataclass
import hashlib
import json
import math
import os
from pathlib import Path, PurePosixPath
import random
import re
import shlex
import sqlite3
import stat
import statistics
import sys
import time
from typing import Any
from urllib.request import Request, urlopen

from model2vec import StaticModel
from model2vec.model import quantize_model
import numpy as np
import torch
from torch import nn
from torch.nn import functional as F


TRAINING_SCHEMA_VERSION = 1
SEED = 42
TOKENS_MAX = 256
DATABASE_BYTES_MAX = 128 * 1024 * 1024
MODEL_FILES_BYTES_MAX = 69 * 1024 * 1024
DOCUMENTS_MAX = 4_096
DOCUMENT_BYTES_MAX = 16 * 1024
DOCUMENTS_RETAINED_BYTES_MAX = 32 * 1024 * 1024
QUERY_BYTES_MAX = 4 * 1024
VOCABULARY_MAX = 65_536
DIMENSIONS_MAX = 2_048
DOWNLOAD_BYTES_MAX = 2 * 1024 * 1024
NL2BASH_LINES_MAX = 20_000
EXAMPLES_PER_COMMAND_MAX = 64
COMMANDS_MAX = 256
HARD_NEGATIVES_MAX = 4
TRIALS_MAX = 3
EPOCHS_MAX = 200
BATCH_COMMANDS_MAX = 32
VALIDATION_INTERVAL = 10
PATIENCE_INTERVALS = 5
NL2BASH_REVISION = "d6b9f5bdff45621d190134e31ab63b7bf7002190"
SOURCE_REPOSITORY = "minishlab/potion-base-8M"
SOURCE_REVISION = "bf8b056651a2c21b8d2565580b8569da283cab23"
SOURCE_FILE_SHA256 = {
    "config.json": "2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553",
    "model.safetensors": "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2",
    "tokenizer.json": "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50",
}
NL2BASH_FILES = {
    "all.nl": "1db0c529c350b463919624550b8f5882a97c42ad5051c7d49fbc496bc4e8b770",
    "all.cm": "3a72eaced7fa14a0938354cefc42b2dcafb2d47297102f1279086e18c3abe57e",
    "LICENSE": "4ac5c8b7fb1d1fccfa52916749674d67b2024c76616fed89db7f67a976056750",
}


@dataclass(frozen=True)
class RetrievalExample:
    """One bounded query with its expected command and evaluation slice."""

    query: str
    command: str
    slice: str


@dataclass(frozen=True)
class Trial:
    """One bounded token-weight optimization configuration."""

    name: str
    learning_rate: float


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
    parser.add_argument("--epochs", type=int, default=160)
    return parser.parse_args()


def file_sha256(path: Path, bytes_max: int) -> tuple[int, str]:
    metadata = path.lstat()
    if path.is_symlink() or not stat.S_ISREG(metadata.st_mode):
        raise RuntimeError(f"input must be a regular non-symlink file: {path}")
    if metadata.st_size > bytes_max:
        raise RuntimeError(
            f"input exceeds {bytes_max} bytes: {path} ({metadata.st_size} bytes)"
        )
    digest = hashlib.sha256()
    observed = 0
    with path.open("rb") as source:
        while chunk := source.read(64 * 1024):
            observed += len(chunk)
            if observed > bytes_max:
                raise RuntimeError(f"input grew beyond {bytes_max} bytes: {path}")
            digest.update(chunk)
    if observed != metadata.st_size:
        raise RuntimeError(f"input changed while hashing: {path}")
    return observed, digest.hexdigest()


def stable_hash(*values: str) -> int:
    digest = hashlib.sha256("\0".join(values).encode("utf-8")).digest()
    return int.from_bytes(digest[:8], "big")


def normalized(text: str) -> str:
    return " ".join(text.casefold().split())


def prepare_output(path: Path) -> Path:
    if path.exists() or path.is_symlink():
        raise RuntimeError(f"training output already exists: {path}")
    path.mkdir(parents=True, mode=0o700)
    return path.resolve()


def read_command_documents(database: Path) -> tuple[dict[str, str], dict[str, str]]:
    file_sha256(database, DATABASE_BYTES_MAX)
    uri = f"file:{database.resolve().as_posix()}?mode=ro&immutable=1"
    connection = sqlite3.connect(uri, uri=True)
    try:
        rows = connection.execute(
            "SELECT target_id, title, body FROM semantic_documents "
            "WHERE document_kind = 'command' ORDER BY target_id"
        ).fetchall()
    finally:
        connection.close()
    if not rows or len(rows) > DOCUMENTS_MAX:
        raise RuntimeError(f"command document count is outside 1..{DOCUMENTS_MAX}")
    documents: dict[str, str] = {}
    titles: dict[str, str] = {}
    retained = 0
    for command, title, body in rows:
        if command in documents:
            raise RuntimeError(f"duplicate command document target: {command}")
        if not all(isinstance(value, str) for value in (command, title, body)):
            raise RuntimeError("command document contains a non-text field")
        body_bytes = len(body.encode("utf-8"))
        if not command or body_bytes > DOCUMENT_BYTES_MAX:
            raise RuntimeError(f"command document violates its byte bound: {command}")
        retained += len(command.encode("utf-8")) + len(title.encode("utf-8")) + body_bytes
        if retained > DOCUMENTS_RETAINED_BYTES_MAX:
            raise RuntimeError("command documents exceed their retained-text bound")
        documents[command] = body
        titles[command] = title
    return documents, titles


def read_fixture(path: Path, documents: dict[str, str]) -> tuple[list[RetrievalExample], set[str]]:
    _size, _digest = file_sha256(path, 1024 * 1024)
    value = json.loads(path.read_text(encoding="utf-8"))
    if value.get("schema_version") != 1:
        raise RuntimeError("unsupported retrieval fixture schema")
    splits: dict[str, str] = {}
    for split in value.get("splits", []):
        name = split.get("name")
        for group in split.get("command_groups", []):
            if not isinstance(name, str) or not isinstance(group, str) or group in splits:
                raise RuntimeError("retrieval fixture has an invalid group split")
            splits[group] = name
    groups: dict[str, tuple[str, ...]] = {}
    for group in value.get("command_groups", []):
        group_id = group.get("id")
        targets = group.get("expected_targets")
        if not isinstance(group_id, str) or not isinstance(targets, list) or not targets:
            raise RuntimeError("retrieval fixture has an invalid command group")
        target_tuple = tuple(target for target in targets if isinstance(target, str))
        if len(target_tuple) != len(targets) or group_id in groups:
            raise RuntimeError("retrieval fixture has duplicate or invalid targets")
        groups[group_id] = target_tuple
    examples = []
    for query in value.get("queries", []):
        group_id = query.get("command_group")
        text = query.get("text")
        language = query.get("language")
        if group_id not in groups or group_id not in splits:
            raise RuntimeError("retrieval fixture query references an unknown group")
        if not isinstance(text, str) or not isinstance(language, str):
            raise RuntimeError("retrieval fixture query has invalid text or language")
        if not text or len(text.encode("utf-8")) > QUERY_BYTES_MAX:
            raise RuntimeError("retrieval fixture query violates its byte bound")
        target = groups[group_id][0]
        if target not in documents:
            raise RuntimeError(f"fixture target is absent from the catalog: {target}")
        examples.append(RetrievalExample(text, target, f"{splits[group_id]}:{language}"))
    unseen = {
        target
        for group_id, targets in groups.items()
        if splits.get(group_id) == "unseen_command"
        for target in targets
    }
    if not examples or not unseen:
        raise RuntimeError("retrieval fixture lacks evaluation examples or unseen commands")
    return examples, unseen


def download_verified(directory: Path, name: str, expected_sha256: str) -> Path:
    directory.mkdir(parents=True, mode=0o700, exist_ok=True)
    path = directory / name
    if path.is_file() and not path.is_symlink():
        if file_sha256(path, DOWNLOAD_BYTES_MAX)[1] == expected_sha256:
            return path
        raise RuntimeError(f"cached dataset file has the wrong identity: {path}")
    url = (
        "https://raw.githubusercontent.com/TellinaTool/nl2bash/"
        f"{NL2BASH_REVISION}/data/bash/{name}"
    )
    request = Request(url, headers={"User-Agent": "quirl-retrieval-training/1"})
    with urlopen(request, timeout=30) as response:
        declared = response.headers.get("Content-Length")
        if declared is not None and int(declared) > DOWNLOAD_BYTES_MAX:
            raise RuntimeError(f"dataset file exceeds {DOWNLOAD_BYTES_MAX} bytes: {name}")
        payload = response.read(DOWNLOAD_BYTES_MAX + 1)
    if len(payload) > DOWNLOAD_BYTES_MAX:
        raise RuntimeError(f"dataset file exceeds {DOWNLOAD_BYTES_MAX} bytes: {name}")
    if hashlib.sha256(payload).hexdigest() != expected_sha256:
        raise RuntimeError(f"dataset file failed its pinned SHA-256 check: {name}")
    temporary = directory / f".{name}.tmp-{os.getpid()}"
    temporary.write_bytes(payload)
    os.replace(temporary, path)
    return path


def extract_single_utility(command_line: str) -> str | None:
    if len(command_line.encode("utf-8")) > QUERY_BYTES_MAX:
        return None
    if any(operator in command_line for operator in ("|", ";", "&", "`", "$(")):
        return None
    try:
        tokens = shlex.split(command_line, posix=True)
    except ValueError:
        return None
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if "=" not in token or token.startswith(("-", "/")):
            break
        name, _separator, _value = token.partition("=")
        if not name.replace("_", "a").isalnum():
            break
        index += 1
    if index >= len(tokens) or tokens[index] in {"sudo", "env", "time", "command", "nohup"}:
        return None
    utility = PurePosixPath(tokens[index].lstrip("\\")).name
    if not utility or utility.startswith("-") or len(utility.encode("utf-8")) > 128:
        return None
    return utility


def catalog_phrases(title: str, body: str) -> list[str]:
    phrases = [title]
    for line in body.splitlines():
        if line.startswith("summary: "):
            phrases.append(line.removeprefix("summary: "))
        elif line.startswith("intent: "):
            intent = line.removeprefix("intent: ")
            primary, separator, related = intent.partition(" Related intents: ")
            phrases.extend(re.split(r"(?<=[.!?])\s+", primary))
            if separator:
                phrases.extend(part.strip(" .") for part in related.split(";") if part.strip())
    deduplicated = []
    seen = set()
    for phrase in phrases:
        phrase = phrase.strip()
        key = normalized(phrase)
        if key and key not in seen and len(phrase.encode("utf-8")) <= QUERY_BYTES_MAX:
            seen.add(key)
            deduplicated.append(phrase)
    return deduplicated[:8]


def load_training_data(
    cache: Path,
    documents: dict[str, str],
    titles: dict[str, str],
    unseen_commands: set[str],
) -> tuple[dict[str, list[str]], list[RetrievalExample], list[RetrievalExample], dict[str, Any]]:
    paths = {
        name: download_verified(cache, name, digest)
        for name, digest in NL2BASH_FILES.items()
    }
    queries = paths["all.nl"].read_text(encoding="utf-8").splitlines()
    command_lines = paths["all.cm"].read_text(encoding="utf-8").splitlines()
    if len(queries) != len(command_lines) or len(queries) > NL2BASH_LINES_MAX:
        raise RuntimeError("NL2Bash files violate their aligned line-count bound")
    grouped: dict[str, list[tuple[str, str]]] = defaultdict(list)
    for query, command_line in zip(queries, command_lines, strict=True):
        utility = extract_single_utility(command_line)
        if (
            utility is not None
            and utility in documents
            and utility not in unseen_commands
            and query
            and len(query.encode("utf-8")) <= QUERY_BYTES_MAX
        ):
            grouped[utility].append((query, command_line))
    training: dict[str, list[str]] = {}
    validation = []
    test = []
    nl2bash_train_pairs = 0
    for command, records in sorted(grouped.items()):
        deduplicated = {normalized(query): (query, shell) for query, shell in records}
        ordered = sorted(
            deduplicated.values(),
            key=lambda record: stable_hash(str(SEED), command, record[0], record[1]),
        )[:EXAMPLES_PER_COMMAND_MAX]
        if len(ordered) < 6:
            continue
        validation_count = max(1, math.floor(len(ordered) * 0.15))
        test_count = max(1, math.floor(len(ordered) * 0.15))
        training_count = len(ordered) - validation_count - test_count
        training[command] = [query for query, _shell in ordered[:training_count]]
        nl2bash_train_pairs += training_count
        validation.extend(
            RetrievalExample(query, command, "nl2bash_validation")
            for query, _shell in ordered[training_count : training_count + validation_count]
        )
        test.extend(
            RetrievalExample(query, command, "nl2bash_test")
            for query, _shell in ordered[training_count + validation_count :]
        )
    for command, body in sorted(documents.items()):
        if command in unseen_commands:
            continue
        phrases = catalog_phrases(titles[command], body)
        if phrases:
            combined = training.setdefault(command, []) + phrases
            training[command] = list(dict.fromkeys(combined))[:EXAMPLES_PER_COMMAND_MAX]
    if not 16 <= len(training) <= COMMANDS_MAX:
        raise RuntimeError("training command count is outside its configured bound")
    if not validation or not test:
        raise RuntimeError("training dataset produced an empty validation or test split")
    return training, validation, test, {
        "revision": NL2BASH_REVISION,
        "file_sha256": {name: digest for name, digest in NL2BASH_FILES.items()},
        "source_pairs": len(queries),
        "training_commands": len(training),
        "training_pairs": sum(len(values) for values in training.values()),
        "nl2bash_training_pairs": nl2bash_train_pairs,
        "validation_pairs": len(validation),
        "test_pairs": len(test),
        "unseen_commands": sorted(unseen_commands),
    }


def assert_no_fixture_leakage(
    examples: list[RetrievalExample],
    documents: dict[str, str],
    training: dict[str, list[str]],
) -> None:
    indexed = [normalized(body) for body in documents.values()]
    trained = [normalized(query) for queries in training.values() for query in queries]
    for example in examples:
        query = normalized(example.query)
        if any(query in text for text in indexed):
            raise RuntimeError(f"fixture query leaked into an indexed document: {example.query}")
        if any(query in text for text in trained):
            raise RuntimeError(f"fixture query leaked into training data: {example.query}")


class TunableStaticModel(nn.Module):
    """A sparse token-weight adaptation that preserves source vectors."""

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
        initial_weights = (
            np.ones(len(source.tokens), dtype=np.float32)
            if source.weights is None
            else np.asarray(source.weights, dtype=np.float32)
        )
        log_weights = torch.from_numpy(np.log(np.clip(initial_weights, 1e-4, 1e4)))
        self.log_weights = nn.Embedding.from_pretrained(
            log_weights[:, None], freeze=not trainable, sparse=trainable
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
        weights = torch.exp(torch.clamp(self.log_weights(ids).squeeze(-1), -4.0, 4.0))
        pooled = (vectors * weights[:, :, None] * mask[:, :, None]).sum(dim=1)
        pooled = pooled / mask.sum(dim=1).clamp_min(1.0)[:, None]
        return F.normalize(pooled, dim=1)

    def encode_texts(self, texts: list[str], batch_size: int = 256) -> torch.Tensor:
        return torch.cat(
            [
                self.encode_ids(self.token_ids(texts[start : start + batch_size]))
                for start in range(0, len(texts), batch_size)
            ]
        )

    def export(self, metadata: dict[str, Any]) -> StaticModel:
        config = dict(self.source.config)
        config["quirl_command_tuning"] = metadata
        weights = torch.exp(
            torch.clamp(self.log_weights.weight.detach().squeeze(-1), -4.0, 4.0)
        ).numpy()
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


@torch.no_grad()
def evaluate(
    model: TunableStaticModel,
    documents: dict[str, str],
    examples: list[RetrievalExample],
) -> dict[str, Any]:
    commands = sorted(documents)
    indexes = {command: index for index, command in enumerate(commands)}
    document_vectors = model.encode_texts([documents[command] for command in commands])
    slices: dict[str, list[int]] = defaultdict(list)
    all_ranks = []
    for start in range(0, len(examples), 256):
        batch = examples[start : start + 256]
        query_vectors = model.encode_texts([example.query for example in batch])
        scores = query_vectors @ document_vectors.T
        for row, example in enumerate(batch):
            expected_score = scores[row, indexes[example.command]]
            rank = int((scores[row] > expected_score).sum()) + 1
            all_ranks.append(rank)
            slices[example.slice].append(rank)
    return {
        "overall": retrieval_metrics(all_ranks),
        "slices": {name: retrieval_metrics(ranks) for name, ranks in sorted(slices.items())},
    }


@torch.no_grad()
def mine_hard_negatives(
    source: StaticModel,
    documents: dict[str, str],
    training: dict[str, list[str]],
) -> dict[str, list[str]]:
    commands = sorted(documents)
    document_vectors = source.encode(
        [documents[command] for command in commands],
        max_length=TOKENS_MAX,
        batch_size=256,
        use_multiprocessing=False,
    )
    negatives = {}
    for command, queries in sorted(training.items()):
        query_vectors = source.encode(
            queries[:8], max_length=TOKENS_MAX, use_multiprocessing=False
        )
        scores = document_vectors @ np.mean(query_vectors, axis=0)
        order = sorted(range(len(commands)), key=lambda index: (-scores[index], commands[index]))
        negatives[command] = [
            candidate
            for index in order
            if (candidate := commands[index]) != command
        ][:HARD_NEGATIVES_MAX]
    return negatives


def train_trial(
    source: StaticModel,
    trial: Trial,
    training: dict[str, list[str]],
    documents: dict[str, str],
    validation: list[RetrievalExample],
    hard_negatives: dict[str, list[str]],
    epochs: int,
) -> tuple[TunableStaticModel, dict[str, Any]]:
    torch.manual_seed(SEED)
    randomizer = random.Random(SEED)
    model = TunableStaticModel(source, trainable=True)
    optimizer = torch.optim.SparseAdam([model.log_weights.weight], lr=trial.learning_rate)
    commands = sorted(training)
    best_key = (-1.0, -1.0)
    best_weights: torch.Tensor | None = None
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
            queries = [
                training[command][(epoch - 1) % len(training[command])]
                for command in batch_commands
            ]
            positives = [documents[command] for command in batch_commands]
            negative_commands = []
            batch_set = set(batch_commands)
            for command in batch_commands:
                for negative in hard_negatives[command]:
                    if negative not in batch_set and negative not in negative_commands:
                        negative_commands.append(negative)
            negatives = [documents[command] for command in negative_commands]
            query_ids = model.token_ids(queries)
            positive_ids = model.token_ids(positives)
            negative_ids = model.token_ids(negatives)
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
            loss = (query_loss + symmetric_loss) / 2
            optimizer.zero_grad(set_to_none=True)
            loss.backward()
            gradient = model.log_weights.weight.grad
            if gradient is not None:
                gradient = gradient.coalesce()
                gradient.values().clamp_(-2.0, 2.0)
                model.log_weights.weight.grad = gradient
            optimizer.step()
            with torch.no_grad():
                model.log_weights.weight.clamp_(-4.0, 4.0)
            observed_loss = float(loss.detach())
            if not math.isfinite(observed_loss):
                raise RuntimeError(f"non-finite loss in trial {trial.name}")
            epoch_losses.append(observed_loss)
        losses.append(statistics.mean(epoch_losses))
        if epoch % VALIDATION_INTERVAL == 0 or epoch == epochs:
            score = evaluate(model, documents, validation)
            overall = score["overall"]
            key = (overall["recall_at_10"], overall["mrr"])
            validation_history.append({"epoch": epoch, **overall})
            if key > best_key:
                best_key = key
                best_epoch = epoch
                best_weights = model.log_weights.weight.detach().clone()
                stale_intervals = 0
            else:
                stale_intervals += 1
                if stale_intervals >= PATIENCE_INTERVALS:
                    break
    if best_weights is None:
        raise RuntimeError(f"trial did not produce a checkpoint: {trial.name}")
    with torch.no_grad():
        model.log_weights.weight.copy_(best_weights)
    return model, {
        "trial": asdict(trial),
        "best_epoch": best_epoch,
        "epochs_run": len(losses),
        "initial_loss": losses[0],
        "final_loss": losses[-1],
        "training_seconds": time.perf_counter() - started,
        "validation_history": validation_history,
        "best_validation": evaluate(model, documents, validation),
    }


def model_source_identity(path: Path) -> dict[str, Any]:
    files = {}
    total = 0
    for name, limit in (
        ("config.json", 1024 * 1024),
        ("tokenizer.json", 4 * 1024 * 1024),
        ("model.safetensors", 64 * 1024 * 1024),
    ):
        size, digest = file_sha256(path / name, limit)
        if digest != SOURCE_FILE_SHA256[name]:
            raise RuntimeError(f"source model file has the wrong identity: {name}")
        total += size
        files[name] = {"bytes": size, "sha256": digest}
    if total > MODEL_FILES_BYTES_MAX:
        raise RuntimeError("source model files exceed their aggregate byte bound")
    return {
        "repository": SOURCE_REPOSITORY,
        "revision": SOURCE_REVISION,
        "path": str(path.resolve()),
        "bytes": total,
        "dimensions": 256,
        "files": files,
    }


def pipeline_identity() -> dict[str, Any]:
    directory = Path(__file__).resolve().parent
    files = {}
    for name in ("train.py", "pyproject.toml", "uv.lock"):
        size, digest = file_sha256(directory / name, 1024 * 1024)
        files[name] = {"bytes": size, "sha256": digest}
    return {"schema_version": TRAINING_SCHEMA_VERSION, "files": files}


def write_manifest(model_path: Path, repository: str, revision: str) -> dict[str, Any]:
    config = file_sha256(model_path / "config.json", 1024 * 1024)
    tokenizer = file_sha256(model_path / "tokenizer.json", 4 * 1024 * 1024)
    weights = file_sha256(model_path / "model.safetensors", 64 * 1024 * 1024)
    manifest = {
        "schema_version": 1,
        "repository": repository,
        "revision": revision,
        "dimensions": 256,
        "assets": {
            "config_sha256": config[1],
            "tokenizer_sha256": tokenizer[1],
            "model_sha256": weights[1],
        },
    }
    (model_path / "quirl-model.json").write_text(
        json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n",
        encoding="utf-8",
    )
    return {"manifest": manifest, "bytes": config[0] + tokenizer[0] + weights[0]}


def save_candidate(
    model: TunableStaticModel,
    output: Path,
    metadata: dict[str, Any],
    revision: str,
) -> dict[str, Any]:
    float_path = output / "selected-f32"
    exported = model.export(metadata)
    exported.save_pretrained(float_path, model_name=float_path.name)
    float_identity = write_manifest(
        float_path, "local/quirl-potion-base-8M-command-v1", revision
    )
    int8_path = output / "selected-int8"
    quantized = quantize_model(exported, quantize_to="int8")
    quantized.config["quirl_command_quantization"] = "global_int8_cosine"
    quantized.save_pretrained(int8_path, model_name=int8_path.name)
    int8_identity = write_manifest(
        int8_path, "local/quirl-potion-base-8M-command-v1-int8", revision
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


def main() -> int:
    arguments = parse_arguments()
    if not 20 <= arguments.epochs <= EPOCHS_MAX:
        raise RuntimeError(f"epochs must be between 20 and {EPOCHS_MAX}")
    torch.set_num_threads(min(8, os.cpu_count() or 1))
    torch.use_deterministic_algorithms(True)
    output = prepare_output(arguments.output)
    database_identity = file_sha256(arguments.database, DATABASE_BYTES_MAX)
    fixture_identity = file_sha256(arguments.fixture, 1024 * 1024)
    source_identity = model_source_identity(arguments.model)
    training_pipeline = pipeline_identity()
    documents, titles = read_command_documents(arguments.database)
    fixture_examples, unseen_commands = read_fixture(arguments.fixture, documents)
    training, validation, test, dataset = load_training_data(
        arguments.dataset_cache, documents, titles, unseen_commands
    )
    assert_no_fixture_leakage(fixture_examples, documents, training)
    source = StaticModel.from_pretrained(arguments.model)
    if len(source.tokens) > VOCABULARY_MAX or source.embedding.shape[1] > DIMENSIONS_MAX:
        raise RuntimeError("source model exceeds vocabulary or dimension bounds")
    if source.embedding.shape[1] != 256:
        raise RuntimeError("first training version requires exactly 256 dimensions")
    hard_negatives = mine_hard_negatives(source, documents, training)
    baseline = TunableStaticModel(source, trainable=False)
    report: dict[str, Any] = {
        "schema_version": TRAINING_SCHEMA_VERSION,
        "seed": SEED,
        "selection_rule": "NL2Bash validation Recall@10, then MRR",
        "training_device": "cpu",
        "database": {
            "path": str(arguments.database.resolve()),
            "bytes": database_identity[0],
            "sha256": database_identity[1],
            "command_documents": len(documents),
        },
        "fixture": {
            "path": str(arguments.fixture.resolve()),
            "bytes": fixture_identity[0],
            "sha256": fixture_identity[1],
            "queries": len(fixture_examples),
            "role": "post-selection evaluation only",
            "leakage_check": "passed",
        },
        "source_model": source_identity,
        "training_pipeline": training_pipeline,
        "dataset": dataset,
        "baseline": {
            "validation": evaluate(baseline, documents, validation),
            "test": evaluate(baseline, documents, test),
            "quirl_fixture": evaluate(baseline, documents, fixture_examples),
        },
        "trials": [],
    }
    trials = [
        Trial("weights-lr-0.005", 0.005),
        Trial("weights-lr-0.01", 0.01),
        Trial("weights-lr-0.03", 0.03),
    ]
    if len(trials) > TRIALS_MAX:
        raise RuntimeError("trial count exceeds its configured bound")
    selected_key = (-1.0, -1.0)
    selected_model: TunableStaticModel | None = None
    selected_report: dict[str, Any] | None = None
    for trial in trials:
        print(f"training {trial.name}", file=sys.stderr, flush=True)
        model, trial_report = train_trial(
            source, trial, training, documents, validation, hard_negatives, arguments.epochs
        )
        trial_report["test"] = evaluate(model, documents, test)
        report["trials"].append(trial_report)
        score = trial_report["best_validation"]["overall"]
        key = (score["recall_at_10"], score["mrr"])
        if key > selected_key:
            selected_key = key
            selected_model = model
            selected_report = trial_report
    if selected_model is None or selected_report is None:
        raise RuntimeError("training sweep did not select a candidate")
    selected_report["quirl_fixture"] = evaluate(
        selected_model, documents, fixture_examples
    )
    identity_material = json.dumps(
        {
            "schema_version": TRAINING_SCHEMA_VERSION,
            "seed": SEED,
            "database_sha256": database_identity[1],
            "fixture_sha256": fixture_identity[1],
            "source": source_identity["files"],
            "training_pipeline": training_pipeline,
            "dataset": dataset,
            "selected_trial": selected_report["trial"],
            "best_epoch": selected_report["best_epoch"],
        },
        sort_keys=True,
        separators=(",", ":"),
    )
    revision = "quirl-command-v1-" + hashlib.sha256(
        identity_material.encode("utf-8")
    ).hexdigest()[:16]
    report["selected"] = {
        "trial": selected_report["trial"],
        "best_epoch": selected_report["best_epoch"],
        "revision": revision,
        "validation": selected_report["best_validation"],
        "test": selected_report["test"],
        "quirl_fixture": selected_report["quirl_fixture"],
    }
    report["artifacts"] = save_candidate(
        selected_model,
        output,
        {
            "schema_version": TRAINING_SCHEMA_VERSION,
            "seed": SEED,
            "dataset_revision": NL2BASH_REVISION,
            "source_repository": SOURCE_REPOSITORY,
            "source_revision": SOURCE_REVISION,
            "source_file_sha256": SOURCE_FILE_SHA256,
            "training_pipeline": training_pipeline,
            "database_sha256": database_identity[1],
            "fixture_sha256": fixture_identity[1],
            "fixture_role": "post-selection evaluation only",
            "unseen_commands": sorted(unseen_commands),
            "trial": selected_report["trial"],
            "best_epoch": selected_report["best_epoch"],
        },
        revision,
    )
    report_path = output / "training-report.json"
    report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(
        json.dumps(
            {
                "report": str(report_path),
                "selected": report["selected"],
                "artifacts": report["artifacts"],
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
        print(f"retrieval training: {error}", file=sys.stderr)
        raise SystemExit(1) from None
