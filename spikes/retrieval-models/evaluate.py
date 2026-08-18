#!/usr/bin/env python3
"""Bounded, network-free evaluation for Quirl's local command retriever."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import resource
import signal
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from typing import Any, Iterable, Mapping, Sequence
from urllib.parse import quote


FIXTURE_SCHEMA_VERSION = 1
REPORT_SCHEMA_VERSION = 1
FIXTURE_BYTES_MAX = 1024 * 1024
DESCRIPTION_BYTES_MAX = 4096
SPLITS_MAX = 16
GROUPS_MAX = 256
QUERIES_MAX = 512
EXPECTED_TARGETS_MAX = 16
IDENTIFIER_BYTES_MAX = 128
TARGET_BYTES_MAX = 512
QUERY_BYTES_MAX = 4096
QUERY_WORDS_MIN = 6
RESULTS_MAX = 100
SUBPROCESS_OUTPUT_BYTES_MAX = 512 * 1024
SUBPROCESS_TIMEOUT_SECONDS_MAX = 300.0
DATABASE_BYTES_MAX = 128 * 1024 * 1024
BINARY_BYTES_MAX = 256 * 1024 * 1024
SEMANTIC_DOCUMENTS_MAX = 65_536
SEMANTIC_SCAN_BYTES_MAX = 128 * 1024 * 1024
MODEL_FILES_MAX = 64
MODEL_DEPTH_MAX = 8
MODEL_BYTES_MAX = 128 * 1024 * 1024
READ_CHUNK_BYTES = 64 * 1024

IDENTIFIER_PATTERN = re.compile(r"^[a-z0-9][a-z0-9_-]*$")
LANGUAGE_PATTERN = re.compile(r"^[a-z]{2,3}(?:-[A-Z]{2})?$")


class EvaluationError(RuntimeError):
    """Expected invalid input, resource-limit, or subprocess failure."""


@dataclass(frozen=True)
class CommandGroup:
    group_id: str
    expected_targets: tuple[str, ...]
    destructive: bool
    split: str


@dataclass(frozen=True)
class QueryCase:
    query_id: str
    group_id: str
    language: str
    text: str


@dataclass(frozen=True)
class Fixture:
    name: str
    description: str
    groups: Mapping[str, CommandGroup]
    queries: tuple[QueryCase, ...]
    split_names: tuple[str, ...]


@dataclass(frozen=True)
class ProcessResult:
    stdout: bytes
    stderr: bytes
    elapsed_ms: float


def _fail(message: str) -> None:
    raise EvaluationError(message)


def _expect_object(
    value: Any,
    label: str,
    required: set[str],
    optional: set[str] | None = None,
) -> dict[str, Any]:
    if not isinstance(value, dict):
        _fail(f"{label} must be an object")
    optional = optional or set()
    keys = set(value)
    unknown = keys - required - optional
    missing = required - keys
    if unknown:
        _fail(f"{label} contains unknown field(s): {', '.join(sorted(unknown))}")
    if missing:
        _fail(f"{label} is missing field(s): {', '.join(sorted(missing))}")
    return value


def _expect_list(value: Any, label: str, count_max: int) -> list[Any]:
    if not isinstance(value, list):
        _fail(f"{label} must be an array")
    if not value or len(value) > count_max:
        _fail(f"{label} count must be between 1 and {count_max}; observed {len(value)}")
    return value


def _expect_string(value: Any, label: str, bytes_max: int) -> str:
    if not isinstance(value, str) or not value.strip():
        _fail(f"{label} must be a non-empty string")
    observed = len(value.encode("utf-8"))
    if observed > bytes_max:
        _fail(f"{label} exceeds {bytes_max} bytes; observed {observed}")
    return value


def _expect_identifier(value: Any, label: str) -> str:
    identifier = _expect_string(value, label, IDENTIFIER_BYTES_MAX)
    if not IDENTIFIER_PATTERN.fullmatch(identifier):
        _fail(f"{label} must match {IDENTIFIER_PATTERN.pattern}")
    return identifier


def _expect_bool(value: Any, label: str) -> bool:
    if type(value) is not bool:
        _fail(f"{label} must be a boolean")
    return value


def _load_json_bounded(path: Path, bytes_max: int, label: str) -> Any:
    try:
        with path.open("rb") as stream:
            encoded = stream.read(bytes_max + 1)
    except OSError as error:
        raise EvaluationError(f"could not read {label} {path}: {error}") from error
    if len(encoded) > bytes_max:
        _fail(f"{label} exceeds {bytes_max} bytes; observed at least {len(encoded)}")
    try:
        return json.loads(encoded)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise EvaluationError(f"{label} is not valid UTF-8 JSON: {error}") from error


def load_fixture(path: Path) -> Fixture:
    return validate_fixture(_load_json_bounded(path, FIXTURE_BYTES_MAX, "fixture"))


def _validate_splits(value: Any) -> tuple[list[str], dict[str, str]]:
    split_values = _expect_list(value, "fixture.splits", SPLITS_MAX)
    split_names: list[str] = []
    group_to_split: dict[str, str] = {}
    for index, split_value in enumerate(split_values):
        label = f"fixture.splits[{index}]"
        split = _expect_object(split_value, label, {"name", "command_groups"})
        split_name = _expect_identifier(split["name"], f"{label}.name")
        if split_name in split_names:
            _fail(f"fixture contains duplicate split {split_name!r}")
        split_names.append(split_name)
        members = _expect_list(split["command_groups"], f"{label}.command_groups", GROUPS_MAX)
        for member_index, member in enumerate(members):
            group_id = _expect_identifier(member, f"{label}.command_groups[{member_index}]")
            previous = group_to_split.get(group_id)
            if previous is not None:
                _fail(
                    f"command group {group_id!r} appears in both {previous!r} and {split_name!r}"
                )
            group_to_split[group_id] = split_name
    if "unseen_command" not in split_names:
        _fail("fixture must define an unseen_command split")
    return split_names, group_to_split


def _validate_groups(value: Any, group_to_split: Mapping[str, str]) -> dict[str, CommandGroup]:
    group_values = _expect_list(value, "fixture.command_groups", GROUPS_MAX)
    groups: dict[str, CommandGroup] = {}
    for index, group_value in enumerate(group_values):
        label = f"fixture.command_groups[{index}]"
        group = _expect_object(group_value, label, {"id", "expected_targets", "destructive"})
        group_id = _expect_identifier(group["id"], f"{label}.id")
        if group_id in groups:
            _fail(f"fixture contains duplicate command group {group_id!r}")
        if group_id not in group_to_split:
            _fail(f"command group {group_id!r} is not assigned to a split")
        targets = _expect_list(
            group["expected_targets"], f"{label}.expected_targets", EXPECTED_TARGETS_MAX
        )
        expected_targets = tuple(
            _expect_string(target, f"{label}.expected_targets[{target_index}]", TARGET_BYTES_MAX)
            for target_index, target in enumerate(targets)
        )
        if len(set(expected_targets)) != len(expected_targets):
            _fail(f"command group {group_id!r} contains duplicate expected targets")
        groups[group_id] = CommandGroup(
            group_id=group_id,
            expected_targets=expected_targets,
            destructive=_expect_bool(group["destructive"], f"{label}.destructive"),
            split=group_to_split[group_id],
        )
    unrecognized_members = set(group_to_split) - set(groups)
    if unrecognized_members:
        _fail(
            "split membership references undefined command group(s): "
            + ", ".join(sorted(unrecognized_members))
        )
    return groups


def _validate_queries(value: Any, groups: Mapping[str, CommandGroup]) -> tuple[QueryCase, ...]:
    query_values = _expect_list(value, "fixture.queries", QUERIES_MAX)
    queries: list[QueryCase] = []
    query_ids: set[str] = set()
    query_texts: set[str] = set()
    for index, query_value in enumerate(query_values):
        label = f"fixture.queries[{index}]"
        query = _expect_object(query_value, label, {"id", "command_group", "language", "text"})
        query_id = _expect_identifier(query["id"], f"{label}.id")
        if query_id in query_ids:
            _fail(f"fixture contains duplicate query id {query_id!r}")
        query_ids.add(query_id)
        group_id = _expect_identifier(query["command_group"], f"{label}.command_group")
        if group_id not in groups:
            _fail(f"query {query_id!r} references undefined command group {group_id!r}")
        language = _expect_string(query["language"], f"{label}.language", 16)
        if not LANGUAGE_PATTERN.fullmatch(language):
            _fail(f"query {query_id!r} has invalid language tag {language!r}")
        text = _expect_string(query["text"], f"{label}.text", QUERY_BYTES_MAX)
        if len(text.split()) < QUERY_WORDS_MIN:
            _fail(f"query {query_id!r} must contain at least {QUERY_WORDS_MIN} words")
        normalized = normalize_text(text)
        if normalized in query_texts:
            _fail(f"fixture contains duplicate normalized query text for {query_id!r}")
        query_texts.add(normalized)
        queries.append(QueryCase(query_id, group_id, language, text))
    return tuple(queries)


def validate_fixture(value: Any) -> Fixture:
    root = _expect_object(
        value,
        "fixture",
        {"schema_version", "name", "description", "splits", "command_groups", "queries"},
    )
    if type(root["schema_version"]) is not int or root["schema_version"] != FIXTURE_SCHEMA_VERSION:
        _fail(
            f"fixture schema_version must be {FIXTURE_SCHEMA_VERSION}; "
            f"observed {root['schema_version']!r}"
        )
    name = _expect_identifier(root["name"], "fixture.name")
    description = _expect_string(root["description"], "fixture.description", DESCRIPTION_BYTES_MAX)
    split_names, group_to_split = _validate_splits(root["splits"])
    groups = _validate_groups(root["command_groups"], group_to_split)
    queries = _validate_queries(root["queries"], groups)
    if not any(not query.language.lower().startswith("en") for query in queries):
        _fail("fixture must contain at least one multilingual query")
    if not any(groups[query.group_id].destructive for query in queries):
        _fail("fixture must contain at least one destructive query")
    if not any(groups[query.group_id].split == "unseen_command" for query in queries):
        _fail("fixture must contain at least one unseen-command query")
    return Fixture(name, description, groups, queries, tuple(split_names))


def normalize_text(value: str) -> str:
    return " ".join("".join(character.casefold() if character.isalnum() else " " for character in value).split())


def _explicit_regular_file(path: Path, label: str, bytes_max: int) -> Path:
    if path.is_symlink():
        _fail(f"{label} must be an explicit regular file, not a symlink: {path}")
    try:
        metadata = path.stat()
    except OSError as error:
        raise EvaluationError(f"could not inspect {label} {path}: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        _fail(f"{label} is not a regular file: {path}")
    if metadata.st_size > bytes_max:
        _fail(f"{label} exceeds {bytes_max} bytes; observed {metadata.st_size}")
    return path.resolve()


def hash_file(path: Path, bytes_max: int) -> tuple[int, str]:
    size = path.stat().st_size
    if size > bytes_max:
        _fail(f"file {path} exceeds {bytes_max} bytes; observed {size}")
    digest = hashlib.sha256()
    observed = 0
    with path.open("rb") as stream:
        while True:
            chunk = stream.read(READ_CHUNK_BYTES)
            if not chunk:
                break
            observed += len(chunk)
            if observed > bytes_max:
                _fail(f"file {path} grew beyond {bytes_max} bytes while hashing")
            digest.update(chunk)
    if observed != size:
        _fail(f"file {path} changed size while hashing; expected {size}, observed {observed}")
    return observed, digest.hexdigest()


def assert_file_stable(
    path: Path,
    bytes_max: int,
    expected: tuple[int, str],
    label: str,
) -> None:
    observed = hash_file(path, bytes_max)
    if observed != expected:
        _fail(f"{label} changed while the evaluation was running")


def inspect_model(path: Path) -> dict[str, Any]:
    if path.is_symlink():
        _fail(f"model root must be a real directory, not a symlink: {path}")
    try:
        root_metadata = path.stat()
    except OSError as error:
        raise EvaluationError(f"could not inspect model root {path}: {error}") from error
    if not stat.S_ISDIR(root_metadata.st_mode):
        _fail(f"model root is not a directory: {path}")
    root = path.resolve()
    pending: list[tuple[Path, int]] = [(root, 0)]
    files: list[Path] = []
    while pending:
        directory, depth = pending.pop()
        if depth > MODEL_DEPTH_MAX:
            _fail(f"model tree exceeds depth {MODEL_DEPTH_MAX}: {directory}")
        try:
            entries = sorted(os.scandir(directory), key=lambda entry: entry.name)
        except OSError as error:
            raise EvaluationError(f"could not scan model directory {directory}: {error}") from error
        for entry in entries:
            if entry.is_symlink():
                _fail(f"model tree contains a symlink: {entry.path}")
            if entry.is_dir(follow_symlinks=False):
                pending.append((Path(entry.path), depth + 1))
            elif entry.is_file(follow_symlinks=False):
                files.append(Path(entry.path))
                if len(files) > MODEL_FILES_MAX:
                    _fail(f"model tree exceeds {MODEL_FILES_MAX} files")
            else:
                _fail(f"model tree contains a special file: {entry.path}")
    files.sort(key=lambda file: file.relative_to(root).as_posix())
    total_bytes = 0
    combined = hashlib.sha256()
    encoded_files: list[dict[str, Any]] = []
    for file in files:
        relative = file.relative_to(root).as_posix()
        size, digest = hash_file(file, MODEL_BYTES_MAX)
        total_bytes += size
        if total_bytes > MODEL_BYTES_MAX:
            _fail(f"model tree exceeds {MODEL_BYTES_MAX} bytes; observed at least {total_bytes}")
        identity = f"{relative}\0{size}\0{digest}\n".encode("utf-8")
        combined.update(identity)
        encoded_files.append({"path": relative, "bytes": size, "sha256": digest})
    if not encoded_files:
        _fail(f"model root contains no files: {root}")
    return {
        "path": str(root),
        "bytes": total_bytes,
        "content_sha256": combined.hexdigest(),
        "files": encoded_files,
    }


def _terminate_process_group(process: subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        if process.poll() is None:
            process.kill()


def run_bounded(
    arguments: Sequence[str],
    environment: Mapping[str, str],
    timeout_seconds: float,
    allowed_statuses: frozenset[int] = frozenset({0}),
) -> ProcessResult:
    if not 0.0 < timeout_seconds <= SUBPROCESS_TIMEOUT_SECONDS_MAX:
        _fail(
            f"subprocess timeout must be positive and at most "
            f"{SUBPROCESS_TIMEOUT_SECONDS_MAX} seconds"
        )
    started = time.perf_counter()
    try:
        process = subprocess.Popen(
            list(arguments),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=dict(environment),
            start_new_session=True,
        )
    except OSError as error:
        raise EvaluationError(f"could not start {arguments[0]}: {error}") from error
    assert process.stdout is not None
    assert process.stderr is not None
    overflow = threading.Event()
    buffers = {"stdout": bytearray(), "stderr": bytearray()}

    def read_stream(name: str, stream: Any) -> None:
        while True:
            chunk = stream.read(8192)
            if not chunk:
                return
            remaining = SUBPROCESS_OUTPUT_BYTES_MAX - len(buffers[name])
            buffers[name].extend(chunk[: max(0, remaining)])
            if len(chunk) > remaining:
                overflow.set()
                return

    readers = [
        threading.Thread(target=read_stream, args=("stdout", process.stdout), daemon=True),
        threading.Thread(target=read_stream, args=("stderr", process.stderr), daemon=True),
    ]
    for reader in readers:
        reader.start()
    deadline = started + timeout_seconds
    failure: str | None = None
    while process.poll() is None:
        if overflow.is_set():
            failure = f"subprocess output exceeded {SUBPROCESS_OUTPUT_BYTES_MAX} bytes"
            _terminate_process_group(process)
            break
        if time.perf_counter() >= deadline:
            failure = f"subprocess exceeded its {timeout_seconds:g}-second deadline"
            _terminate_process_group(process)
            break
        time.sleep(0.005)
    try:
        process.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        _terminate_process_group(process)
        process.wait(timeout=2.0)
    for reader in readers:
        reader.join(timeout=2.0)
    if any(reader.is_alive() for reader in readers):
        _terminate_process_group(process)
        for reader in readers:
            reader.join(timeout=2.0)
        if any(reader.is_alive() for reader in readers) and failure is None:
            failure = "subprocess output reader did not terminate"
    if overflow.is_set() and failure is None:
        failure = f"subprocess output exceeded {SUBPROCESS_OUTPUT_BYTES_MAX} bytes"
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    if failure is not None:
        _fail(failure)
    stderr = bytes(buffers["stderr"])
    if process.returncode not in allowed_statuses:
        message = stderr.decode("utf-8", "replace").strip()
        _fail(f"subprocess exited with status {process.returncode}: {message}")
    return ProcessResult(bytes(buffers["stdout"]), stderr, elapsed_ms)


def _decode_json_output(result: ProcessResult, label: str) -> Any:
    try:
        return json.loads(result.stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise EvaluationError(f"{label} returned invalid UTF-8 JSON: {error}") from error


def parse_search_results(value: Any, requested_limit: int) -> list[dict[str, Any]]:
    if not isinstance(value, list):
        _fail("quirl ai search JSON must be an array")
    if len(value) > requested_limit:
        _fail(f"quirl returned {len(value)} results for requested limit {requested_limit}")
    parsed: list[dict[str, Any]] = []
    observed_targets: set[str] = set()
    for index, item_value in enumerate(value):
        item = _expect_object(
            item_value,
            f"search result {index}",
            {"command", "target", "kind", "summary", "score", "semantic", "mode"},
        )
        command = _expect_string(item["command"], f"search result {index}.command", TARGET_BYTES_MAX)
        target = _expect_string(item["target"], f"search result {index}.target", TARGET_BYTES_MAX)
        kind = _expect_string(item["kind"], f"search result {index}.kind", 32)
        if kind != "command":
            _fail(f"search result {index}.kind must be 'command'; observed {kind!r}")
        if target in observed_targets:
            _fail(f"quirl returned duplicate target {target!r}")
        observed_targets.add(target)
        summary = _expect_string(item["summary"], f"search result {index}.summary", 16 * 1024)
        if type(item["score"]) not in (int, float) or not math.isfinite(float(item["score"])):
            _fail(f"search result {index}.score must be finite")
        semantic = _expect_bool(item["semantic"], f"search result {index}.semantic")
        mode = _expect_string(item["mode"], f"search result {index}.mode", 16)
        if mode not in {"lexical", "hybrid"}:
            _fail(f"search result {index}.mode is unsupported: {mode!r}")
        if semantic != (mode == "hybrid"):
            _fail(f"search result {index} has inconsistent semantic and mode fields")
        parsed.append(
            {
                "command": command,
                "target": target,
                "kind": kind,
                "summary": summary,
                "score": float(item["score"]),
                "semantic": semantic,
                "mode": mode,
            }
        )
    return parsed


def assert_queries_are_not_indexed(database: Path, fixture: Fixture) -> None:
    encoded_path = quote(str(database), safe="/")
    try:
        connection = sqlite3.connect(
            f"file:{encoded_path}?mode=ro&immutable=1",
            uri=True,
            timeout=1.0,
        )
    except sqlite3.Error as error:
        raise EvaluationError(f"could not open database read-only: {error}") from error
    try:
        connection.execute("PRAGMA query_only = ON")
        row = connection.execute("SELECT count(*) FROM semantic_documents").fetchone()
        if row is None or type(row[0]) is not int:
            _fail("database did not return a semantic-document count")
        if row[0] > SEMANTIC_DOCUMENTS_MAX:
            _fail(
                f"database exceeds {SEMANTIC_DOCUMENTS_MAX} semantic documents; observed {row[0]}"
            )
        normalized_queries = [(query.query_id, normalize_text(query.text)) for query in fixture.queries]
        scanned_bytes = 0
        for title, body in connection.execute(
            "SELECT title, body FROM semantic_documents ORDER BY document_id"
        ):
            if not isinstance(title, str) or not isinstance(body, str):
                _fail("database contains a non-text semantic document")
            scanned_bytes += len(title.encode("utf-8")) + len(body.encode("utf-8"))
            if scanned_bytes > SEMANTIC_SCAN_BYTES_MAX:
                _fail(
                    f"semantic-document scan exceeds {SEMANTIC_SCAN_BYTES_MAX} bytes; "
                    f"observed at least {scanned_bytes}"
                )
            normalized_documents = (normalize_text(title), normalize_text(body))
            for query_id, normalized_query in normalized_queries:
                if any(normalized_query in document for document in normalized_documents):
                    _fail(
                        f"fixture query {query_id!r} is copied into a product semantic document"
                    )
    except sqlite3.Error as error:
        raise EvaluationError(f"could not validate database semantic documents: {error}") from error
    finally:
        connection.close()


def nearest_rank(values: Sequence[float], percentile: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, math.ceil(percentile * len(ordered)) - 1)
    return ordered[index]


def metric_summary(records: Sequence[dict[str, Any]]) -> dict[str, Any]:
    count = len(records)
    if count == 0:
        return {
            "queries": 0,
            "recall_at_1": None,
            "recall_at_5": None,
            "recall_at_10": None,
            "mrr": None,
            "latency_ms": None,
        }
    ranks = [record["rank"] for record in records]
    latencies = [float(record["latency_ms"]) for record in records]
    recall = lambda limit: sum(rank is not None and rank <= limit for rank in ranks) / count
    reciprocal_rank = sum(0.0 if rank is None else 1.0 / rank for rank in ranks) / count
    return {
        "queries": count,
        "recall_at_1": round(recall(1), 6),
        "recall_at_5": round(recall(5), 6),
        "recall_at_10": round(recall(10), 6),
        "mrr": round(reciprocal_rank, 6),
        "latency_ms": {
            "min": round(min(latencies), 3),
            "p50": round(float(nearest_rank(latencies, 0.50)), 3),
            "p95": round(float(nearest_rank(latencies, 0.95)), 3),
            "p99": round(float(nearest_rank(latencies, 0.99)), 3),
            "max": round(max(latencies), 3),
        },
    }


def child_peak_rss() -> tuple[int | None, str]:
    try:
        observed = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    except (AttributeError, ValueError):
        return None, "unavailable"
    multiplier = 1 if sys.platform == "darwin" else 1024
    return int(observed * multiplier), "resource.getrusage(RUSAGE_CHILDREN).ru_maxrss"


def _base_environment(
    database: Path, model: Path | None, automatic_model: bool
) -> dict[str, str]:
    environment = dict(os.environ)
    environment["QUIRL_INDEX_PATH"] = str(database)
    if automatic_model:
        environment.pop("QUIRL_MODEL_PATH", None)
    elif model is None:
        disabled = database.parent / f".quirl-eval-model-disabled-{os.getpid()}"
        if disabled.exists():
            _fail(f"model-disabled sentinel unexpectedly exists: {disabled}")
        environment["QUIRL_MODEL_PATH"] = str(disabled)
    else:
        environment["QUIRL_MODEL_PATH"] = str(model)
    environment["QUIRL_TEST_AI_BOOTSTRAP_DISABLED"] = "1"
    environment["HF_HUB_OFFLINE"] = "1"
    environment["TRANSFORMERS_OFFLINE"] = "1"
    return environment


def _rank_for(targets: Sequence[str], expected: Iterable[str]) -> int | None:
    expected_set = set(expected)
    return next((index for index, target in enumerate(targets, 1) if target in expected_set), None)


def _product_metadata(
    quirl: Path,
    database: Path,
    model: Path | None,
    automatic_model: bool,
    environment: Mapping[str, str],
    timeout_seconds: float,
) -> tuple[dict[str, Any], dict[str, Any]]:
    build_result = run_bounded([str(quirl), "--build-info"], environment, timeout_seconds)
    build_info = _decode_json_output(build_result, "quirl --build-info")
    if not isinstance(build_info, dict):
        _fail("quirl --build-info JSON must be an object")
    status_result = run_bounded(
        [str(quirl), "ai", "status", "--format", "json"], environment, timeout_seconds
    )
    status = _decode_json_output(status_result, "quirl ai status")
    if not isinstance(status, dict) or status.get("network_loading") is not False:
        _fail("quirl ai status did not confirm network loading is disabled")
    if status.get("database_ready") is not True:
        _fail("quirl ai status did not accept the explicit command database")
    status_database = status.get("database_path")
    if not isinstance(status_database, str) or Path(status_database).resolve() != database:
        _fail("quirl ai status did not report the explicit command database path")
    if model is None and not automatic_model and status.get("model_ready") is not False:
        _fail("quirl ai status unexpectedly loaded a model when --model was omitted")
    if automatic_model and status.get("model_ready") is not True:
        _fail("quirl ai status did not accept the automatic pinned model")
    if model is not None:
        status_model = status.get("model_path")
        if status.get("model_ready") is not True:
            _fail("quirl ai status did not accept the explicit model directory")
        if not isinstance(status_model, str) or Path(status_model).resolve() != model:
            _fail("quirl ai status did not report the explicit model directory")
    return build_info, status


def _run_fixture_queries(
    quirl: Path,
    fixture: Fixture,
    environment: Mapping[str, str],
    limit: int,
    timeout_seconds: float,
) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for query in fixture.queries:
        group = fixture.groups[query.group_id]
        result = run_bounded(
            [
                str(quirl), "ai", "search", "--limit", str(limit),
                "--kind", "command", "--format", "json", query.text,
            ],
            environment,
            timeout_seconds,
            frozenset({0, 1}),
        )
        ranked = parse_search_results(_decode_json_output(result, "quirl ai search"), limit)
        targets = [item["target"] for item in ranked]
        records.append(
            {
                "id": query.query_id,
                "command_group": query.group_id,
                "split": group.split,
                "language": query.language,
                "destructive": group.destructive,
                "expected_targets": list(group.expected_targets),
                "rank": _rank_for(targets, group.expected_targets),
                "latency_ms": round(result.elapsed_ms, 3),
                "returned_targets": targets,
                "semantic": any(item["semantic"] for item in ranked),
            }
        )
    return records


def _slice_metrics(fixture: Fixture, records: Sequence[dict[str, Any]]) -> dict[str, Any]:
    slices: dict[str, Sequence[dict[str, Any]]] = {
        "multilingual": [
            record for record in records if not record["language"].lower().startswith("en")
        ],
        "destructive": [record for record in records if record["destructive"]],
        "unseen_command": [record for record in records if record["split"] == "unseen_command"],
    }
    for split_name in fixture.split_names:
        slices[f"split:{split_name}"] = [
            record for record in records if record["split"] == split_name
        ]
    return {name: metric_summary(selected) for name, selected in slices.items()}


def _assemble_report(
    *,
    quirl: Path,
    database: Path,
    model_report: dict[str, Any] | None,
    fixture_path: Path,
    fixture: Fixture,
    quirl_identity: tuple[int, str],
    database_identity: tuple[int, str],
    fixture_identity: tuple[int, str],
    build_info: dict[str, Any],
    status: dict[str, Any],
    records: list[dict[str, Any]],
    limit: int,
    timeout_seconds: float,
) -> dict[str, Any]:
    rss_bytes, rss_method = child_peak_rss()
    return {
        "schema_version": REPORT_SCHEMA_VERSION,
        "suite": "quirl_local_retrieval_v1",
        "network_loading": False,
        "execution_policy": "search-only; returned commands are never executed",
        "fixture": {
            "name": fixture.name,
            "description": fixture.description,
            "path": str(fixture_path),
            "bytes": fixture_identity[0],
            "sha256": fixture_identity[1],
            "query_count": len(fixture.queries),
            "command_group_count": len(fixture.groups),
            "splits": list(fixture.split_names),
            "leakage_check": "passed exact normalized substring scan of semantic_documents",
        },
        "artifacts": {
            "quirl": {
                "path": str(quirl), "bytes": quirl_identity[0],
                "sha256": quirl_identity[1], "build_info": build_info,
            },
            "database": {
                "path": str(database), "bytes": database_identity[0],
                "sha256": database_identity[1], "commands": status.get("commands"),
                "options": status.get("options"),
                "semantic_documents": status.get("semantic_documents"),
                "embeddings": status.get("embeddings"),
                "semantic_ready": status.get("semantic_ready"),
            },
            "model": model_report,
            "model_identity": status.get("model"),
            "model_ready": status.get("model_ready"),
        },
        "parameters": {
            "result_limit": limit,
            "subprocess_timeout_seconds": timeout_seconds,
            "query_execution": "one cold quirl subprocess per query",
        },
        "metrics": {
            "overall": metric_summary(records),
            "slices": _slice_metrics(fixture, records),
        },
        "resources": {
            "peak_child_rss_bytes": rss_bytes,
            "peak_child_rss_method": rss_method,
            "subprocess_output_bytes_max_per_stream": SUBPROCESS_OUTPUT_BYTES_MAX,
        },
        "queries": records,
    }


def evaluate(
    quirl: Path,
    database: Path,
    model: Path | None,
    automatic_model: bool,
    fixture_path: Path,
    limit: int,
    timeout_seconds: float,
) -> dict[str, Any]:
    if limit < 10 or limit > RESULTS_MAX:
        _fail(f"result limit must be between 10 and {RESULTS_MAX}; observed {limit}")
    quirl = _explicit_regular_file(quirl, "Quirl binary", BINARY_BYTES_MAX)
    if not os.access(quirl, os.X_OK):
        _fail(f"Quirl binary is not executable: {quirl}")
    database = _explicit_regular_file(database, "Quirl database", DATABASE_BYTES_MAX)
    fixture_path = _explicit_regular_file(fixture_path, "fixture", FIXTURE_BYTES_MAX)
    fixture_identity = hash_file(fixture_path, FIXTURE_BYTES_MAX)
    quirl_identity = hash_file(quirl, BINARY_BYTES_MAX)
    database_identity = hash_file(database, DATABASE_BYTES_MAX)
    fixture = load_fixture(fixture_path)
    if automatic_model and model is not None:
        _fail("automatic and explicit model selection are mutually exclusive")
    model_path = model.resolve() if model is not None else None
    model_report = inspect_model(model) if model is not None else None
    assert_queries_are_not_indexed(database, fixture)
    environment = _base_environment(database, model_path, automatic_model)
    build_info, status = _product_metadata(
        quirl, database, model_path, automatic_model, environment, timeout_seconds
    )
    if automatic_model:
        status_model_path = status.get("model_path")
        if not isinstance(status_model_path, str):
            _fail("quirl ai status did not report the automatic model path")
        model_path = Path(status_model_path).resolve()
        model_report = inspect_model(model_path)
    records = _run_fixture_queries(quirl, fixture, environment, limit, timeout_seconds)
    assert_file_stable(fixture_path, FIXTURE_BYTES_MAX, fixture_identity, "fixture")
    assert_file_stable(quirl, BINARY_BYTES_MAX, quirl_identity, "Quirl binary")
    assert_file_stable(database, DATABASE_BYTES_MAX, database_identity, "Quirl database")
    if model_path is not None and inspect_model(model_path) != model_report:
        _fail("model tree changed while the evaluation was running")
    return _assemble_report(
        quirl=quirl, database=database, model_report=model_report,
        fixture_path=fixture_path, fixture=fixture, quirl_identity=quirl_identity,
        database_identity=database_identity, fixture_identity=fixture_identity,
        build_info=build_info, status=status, records=records, limit=limit,
        timeout_seconds=timeout_seconds,
    )


def _self_test_fixture() -> dict[str, Any]:
    return {
        "schema_version": 1,
        "name": "self_test",
        "description": "Deterministic evaluator self-test fixture.",
        "splits": [
            {"name": "development", "command_groups": ["read_group"]},
            {"name": "held_out", "command_groups": ["write_group"]},
            {"name": "unseen_command", "command_groups": ["unseen_group"]},
        ],
        "command_groups": [
            {"id": "read_group", "expected_targets": ["alpha"], "destructive": False},
            {"id": "write_group", "expected_targets": ["beta"], "destructive": True},
            {"id": "unseen_group", "expected_targets": ["gamma"], "destructive": False},
        ],
        "queries": [
            {
                "id": "q1",
                "command_group": "read_group",
                "language": "de",
                "text": "Welche Anwendung zeigt diese erfundene Testausgabe zuverlässig an?",
            },
            {
                "id": "q2",
                "command_group": "write_group",
                "language": "en",
                "text": "Which application changes this imaginary test artifact on disk?",
            },
            {
                "id": "q3",
                "command_group": "unseen_group",
                "language": "en",
                "text": "Which utility handles this previously unseen imaginary operation correctly?",
            },
        ],
    }


def self_test() -> None:
    fixture_data = _self_test_fixture()
    fixture = validate_fixture(fixture_data)
    if len(fixture.queries) != 3:
        _fail("self-test fixture query count drifted")

    unknown = json.loads(json.dumps(fixture_data))
    unknown["queries"][0]["unexpected"] = True
    try:
        validate_fixture(unknown)
    except EvaluationError:
        pass
    else:
        _fail("deny-unknown fixture self-test failed")

    overlap = json.loads(json.dumps(fixture_data))
    overlap["splits"][1]["command_groups"].append("read_group")
    try:
        validate_fixture(overlap)
    except EvaluationError:
        pass
    else:
        _fail("disjoint split self-test failed")

    metric_records = [
        {"rank": 1, "latency_ms": 1.0},
        {"rank": 3, "latency_ms": 2.0},
        {"rank": None, "latency_ms": 4.0},
    ]
    summary = metric_summary(metric_records)
    if summary["recall_at_1"] != round(1 / 3, 6):
        _fail("Recall@1 self-test failed")
    if summary["recall_at_5"] != round(2 / 3, 6):
        _fail("Recall@5 self-test failed")
    if summary["mrr"] != round((1 + 1 / 3) / 3, 6):
        _fail("MRR self-test failed")
    if summary["latency_ms"]["p95"] != 4.0:
        _fail("latency percentile self-test failed")

    with tempfile.TemporaryDirectory(prefix="quirl-retrieval-self-test-") as temporary:
        database = Path(temporary) / "fixture.sqlite3"
        connection = sqlite3.connect(database)
        connection.execute(
            "CREATE TABLE semantic_documents "
            "(document_id TEXT PRIMARY KEY, title TEXT NOT NULL, body TEXT NOT NULL)"
        )
        connection.execute(
            "INSERT INTO semantic_documents VALUES (?, ?, ?)",
            ("one", "Fixture", fixture.queries[0].text),
        )
        connection.commit()
        connection.close()
        try:
            assert_queries_are_not_indexed(database, fixture)
        except EvaluationError:
            pass
        else:
            _fail("semantic-document leakage self-test failed")

    environment = dict(os.environ)
    result = run_bounded(
        [sys.executable, "-c", "import json; print(json.dumps({'ok': True}))"],
        environment,
        5.0,
    )
    if _decode_json_output(result, "self-test subprocess") != {"ok": True}:
        _fail("bounded subprocess self-test failed")
    try:
        run_bounded(
            [
                sys.executable,
                "-c",
                f"import sys; sys.stdout.write('x' * {SUBPROCESS_OUTPUT_BYTES_MAX + 1})",
            ],
            environment,
            5.0,
        )
    except EvaluationError:
        pass
    else:
        _fail("subprocess output-limit self-test failed")


def parse_arguments(arguments: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="run deterministic internal tests")
    parser.add_argument("--quirl", type=Path, help="explicit local Quirl executable")
    parser.add_argument("--database", type=Path, help="explicit local Quirl SQLite database")
    parser.add_argument(
        "--fixture",
        type=Path,
        default=Path(__file__).with_name("fixture-v1.json"),
        help="versioned evaluation fixture",
    )
    model_selection = parser.add_mutually_exclusive_group()
    model_selection.add_argument(
        "--model",
        type=Path,
        help="optional explicit local model directory; omission forces lexical fallback",
    )
    model_selection.add_argument(
        "--automatic-model",
        action="store_true",
        help="use Quirl's installed pinned automatic model without changing the default",
    )
    parser.add_argument("--limit", type=int, default=10, help="ranked results per query (10-100)")
    parser.add_argument(
        "--timeout-seconds",
        type=float,
        default=30.0,
        help="deadline for each Quirl subprocess (maximum 300)",
    )
    return parser.parse_args(arguments)


def main(arguments: Sequence[str]) -> int:
    options = parse_arguments(arguments)
    try:
        if options.self_test:
            if (
                options.quirl is not None
                or options.database is not None
                or options.model is not None
                or options.automatic_model
            ):
                _fail("--self-test cannot be combined with artifact arguments")
            self_test()
            print("self-test: ok")
            return 0
        if options.quirl is None or options.database is None:
            _fail("--quirl and --database are required unless --self-test is used")
        report = evaluate(
            options.quirl,
            options.database,
            options.model,
            options.automatic_model,
            options.fixture,
            options.limit,
            options.timeout_seconds,
        )
        print(json.dumps(report, indent=2, sort_keys=True, ensure_ascii=False))
        return 0
    except EvaluationError as error:
        print(f"retrieval evaluation: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
