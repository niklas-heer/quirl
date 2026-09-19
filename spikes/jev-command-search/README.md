# Jev command search experiment

See the [measured report](../../docs/benchmarks/jev-command-search.md).
These standard-library Python scripts are isolated research tooling, not a
product integration. They never execute selected commands.

`evidence.json` preserves the 2026-09-19 run. In each stored request, replace
`criteria_ref` and `criteria_order` with a `criteria` dictionary whose values
come from the referenced `criteria_maps` entry, inserted in `criteria_order`.
Those two storage fields are not TypeSafe API fields. Expected answers are
separate from the request and were never sent to the model. Original summaries
remain uncorrected; `corrections` records the one label amendment separately.

To reproduce with a current catalog, start a persistent shell and a Python
session from the repository root. Load your own TypeSafe credential directly
into process memory, without printing it. For a 1Password item with a
`credential` field:

```python
import pathlib, runpy, subprocess, sys
key = subprocess.run(
    ["op", "item", "get", "YOUR_ITEM_ID", "--fields", "credential", "--reveal"],
    capture_output=True, text=True, timeout=60, check=True,
).stdout.strip()
sys.path.insert(0, str(pathlib.Path("spikes/jev-command-search").resolve()))
import evaluate
evaluate.fixture()
evaluate.evaluate(key)
import stress
stress.freeze()
stress.run(key)
stress.repeat(key)
import diagnostic
diagnostic.run(key)
del key
```

This makes at most 149 explicit inference requests. Outputs use exclusive
creation under `target/jev-command-search`; choose a new output location or
archive a previous experiment before reproducing. Keep the interpreter open
through the sequence, then exit it to release the credential. `evaluate.py`
run directly only freezes the initial fixture and makes no API request.

The original response rejected by an assertion cannot be recovered. For future
runs, the retained harness saves parsed answer/model/usage before validating it.
The diagnostic probes use reversed candidate order, as in the observed run;
this is now explicit instead of inheriting the preceding repeat's configuration.
The original challenge fixture still contains the known incorrect directory-size
label so reproduction remains transparent; apply the evidence bundle's correction
when interpreting its score. No prompt was tuned after observing the scores.

The benchmark assumes a trusted checked-in catalog and hand-authored fixtures.
Do not use it as a general reader for untrusted SQLite or JSON files. No
dependencies need to be installed. The model and published token price are pinned
in the experiment code; recheck availability and pricing before another run.

## Compare with Luna through the local Codex CLI

`luna.py` replays the same 144 frozen cases and candidate orders, including the
32 repeat cases. It applies the previously recorded directory-size label
correction before scoring. It uses an existing authenticated Codex CLI; it does
not read an API key or log in for you.

```sh
python3 spikes/jev-command-search/luna.py \
  --output target/luna-command-search-low --effort low
python3 spikes/jev-command-search/compare.py target/luna-command-search-low
```

Each case starts a fresh, ephemeral `codex exec` in an isolated temporary
directory, disables tool features and host skill discovery, and uses a read-only
sandbox with user configuration and project documents excluded. Luna receives
the original state, decision instructions, and ordered options. Its output
schema requests only `{"choice":"..."}`, with an enum of the admitted options;
it is not asked to generate a probability distribution. All results are
validated without executing choices.

The harness records both complete CLI process duration and the JSONL
`turn.started` to `turn.completed` interval. The latter still includes backend,
network, and CLI work; it is not pure model inference time. A new CLI process
per case is different from Jev's reused HTTP connection and Quirl's persistent
interactive app-server. Token usage includes Codex's surrounding context.
The CLI's model alias and its `low` reasoning setting are explicit; there is
no silent model fallback or auto-retry. The results do not report a resolved
Luna model snapshot.

The output directory must not exist. Bounds are 144 cases, 200 KB per prompt,
1 MB each for stdout/stderr, 100 events, 30 seconds per CLI process, and a
20-minute budget checked between cases. Every exit kills/reaps the process
group. Three consecutive errors stop the run. `--limit` supports small pilot
runs, but the comparison script requires a complete 144-case run.

Cost figures use the recorded input, cached-input, and output token counts at
published API rates. They are API-equivalent estimates, not a ChatGPT invoice.
`compare.py` also estimates published credit consumption and compares costs on
the 143 cases with retained usage from both providers. A one-case preliminary
connectivity/schema pilot is excluded from the saved scored run.
