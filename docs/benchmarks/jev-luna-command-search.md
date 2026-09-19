# Jev versus Luna command selection

Measured 2026-09-19. This extends the
[Jev experiment](jev-command-search.md) with the local Codex CLI, using the
same 144 frozen requests: 112 distinct cases and 32 candidate-order repeats.
It is research evidence, not a product integration or model adoption decision.

## Results

| Measurement | Jev 1.13.0, HTTPS | GPT-5.6 Luna, Codex CLI, low effort |
| --- | ---: | ---: |
| Distinct cases acceptable | 111/112; one unassessed | 109/112 |
| Candidate-order repeats acceptable | 32/32 | 30/32 |
| All requests acceptable | 143/144 | 139/144 |
| Validated outputs | 143/144 | 144/144 |
| Median workflow latency | 451.79 ms | 3,981.83 ms |
| p95 workflow latency | 545.57 ms | 5,855.83 ms |
| API-equivalent cost, 143 matched usage records | $0.01286 | $0.26760 |

The observed Luna CLI workflow took **8.81 times** the median duration and
**20.81 times** the estimated token cost on the matched-cost cases. Both were
inexpensive in absolute terms. The costs and times describe the measured
workflows, not a controlled comparison of bare model inference.

The earlier Jev validation rejection remains unassessed; later probes do not
replace it. Its missing usage excludes that case from both sides of the paired
cost comparison. The existing directory-size label correction was applied to
both providers before scoring Luna; original labels remain in the evidence.

Luna's first 76 cases scored 74/76, its 36 challenge cases scored 35/36, and its
32 repeats scored 30/32. There were no CLI failures, tool events, warnings, or
invalid choice outputs in the scored run.

## Where the choices differed

- **“Fix the remote.”** Jev chose `CLARIFY`; Luna chose `git remote set-url` in
  both candidate orders. The request does not establish what is wrong or even
  which remote is meant. This is the clearest undesirable assumption observed.
- **Play an MP3 through the speakers.** Jev chose `NONE`; Luna chose `open` in
  both orders. The frozen rubric requires a direct capability and rejects a
  partial step. Opening the associated macOS application is nevertheless a
  plausible discovery suggestion under a looser interpretation, and playback
  behavior depends on the handler. Treat these two scored misses as rubric
  sensitive, not decisive evidence of inferior command knowledge.
- **Recursive directory-size totals.** Jev chose `tree`; Luna chose `NONE`.
  The catalog has `tree --du` and `eza --total-size`, supporting the earlier
  label correction. However, those flags were not explicit in the abbreviated
  command descriptions supplied for this command-choice question. The case
  depends partly on background command knowledge rather than complete supplied
  contracts; improve metadata coverage before using it as a promotion gate.
- Other differences were accepted alternatives: `rg`/`grep`, `rm -r`/`-R`,
  and a compression command versus clarification for an intentionally broad
  compression request. Luna correctly selected `--type-not` in the case whose
  original Jev response failed the validator.

The small, hand-authored fixture does not establish a general quality ranking.
All 40 distinct flag-selection cases passed for Luna, as did its eight
multilingual cases. Latency and cost differences are clearer than the quality
difference. Neither command construction nor execution was tested.

## Timing and interface differences

The Luna run used `codex-cli 0.155.1`, the explicit `gpt-5.6-luna` alias, and
`model_reasoning_effort="low"`, the lowest effort advertised in the installed
CLI's model metadata. The API model documentation also lists `none`, but that
was not among the CLI-advertised settings and was not tested. The CLI did not
report a resolved model snapshot, so the exact underlying Luna weights are not
pinned by these artifacts.

Each case started a fresh ephemeral CLI process in an isolated temporary
directory, with user config, project documents, host skills, and tool features
excluded. It received the same state, judgment instructions, and ordered options
as Jev, plus a short classification-only wrapper and an output schema. The
schema admitted only `{"choice":"..."}`. Luna was not asked to generate Jev's
full probability distribution or a self-reported confidence score. No selected
command was executed.

Jev used a persistent Python process and reused HTTPS connections. Its latency
includes sending the request and receiving the complete response. Luna's primary
latency includes process creation, Codex startup, network/backend work, and exit.
Its median `turn.started` to `turn.completed` interval was **3,375.27 ms**, with
p95 **5,241.15 ms**. The median time outside that interval was **594.44 ms**.
The interval is still not pure inference time, and medians should not be summed.

Luna request durations ranged from 2,861.52 to 8,756.38 ms. Calls were sequential;
repository quality checks ran only after measurement to avoid adding their CPU
load. A one-case connectivity/schema pilot is excluded from scored results.
No retry or effort change was used to replace a scored answer.

This does not benchmark a persistent Codex app-server, a direct Luna API client,
different reasoning settings, or Quirl's richer interactive planner. Those could
have materially different latency, context size, caching, and output costs.
Jev and Luna were measured at different times, without controlling cloud load.

## Token costs and ChatGPT billing

Luna reported **1,333,215 input tokens**, **zero cached input tokens**, zero cache
writes, and **2,486 output tokens**, including 177 reasoning tokens, over all
144 scored requests. At the published rates of $0.20 per million input tokens,
$0.02 per million cached input tokens, and $1.20 per million output tokens,
the complete Luna run is **$0.26963 API-equivalent**:

```text
((input - cached_input) × 0.20 + cached_input × 0.02 + output × 1.20) / 1,000,000
```

The CLI was authenticated through ChatGPT. This estimate is **not an observed
charge**, a claim of additional cash spend, or a subscription-price comparison.
Using the published Luna credit rates (5 / 0.5 / 30 credits per million input /
cached input / output tokens) yields **6.740655 estimated credits**. Actual
allowance and billing were not queried. Sources checked on the measurement date:
[Luna model pricing](https://developers.openai.com/api/docs/models/gpt-5.6-luna)
and [Codex pricing](https://learn.chatgpt.com/docs/pricing).

Jev's 143 retained main-run usage records contain **306,157 input tokens**, or
$0.012858594 at its documented $0.042 per million input tokens; its output is
uncharged under that rate. The five earlier diagnostic requests are excluded
from this comparison. See [TypeSafe model pricing](https://docs.typesafe.ai/models).

Codex's reported input includes its surrounding instructions and schema, not
only the task and catalog. The observed cost gap therefore combines different
input volumes and per-token rates. A small direct API classifier should not be
assumed to incur the same overhead. No cache savings are hypothetical: the
calculation uses the zero cache hits actually reported for this run.

## Evidence and recommendation

The [comparison JSON](../../spikes/jev-command-search/luna-low/comparison.json),
[per-request results](../../spikes/jev-command-search/luna-low/results.jsonl),
and [run metadata](../../spikes/jev-command-search/luna-low/metadata.json) retain
scores, usage, timing, prompt hashes, and the frozen evidence identity. The
[experiment README](../../spikes/jev-command-search/README.md) describes replay.
Original Jev data remains unchanged. No credential or authentication file is
stored with the evidence.

The Luna harness was checked against the original 144 cases and the prior label
correction. Offline subprocess probes verified valid-output handling, malformed
JSON rejection, output-flood rejection, and killing/reaping a child after the
30-second deadline. Its process supervisor bounds stdin, stdout, stderr, event
count, request duration, total experiment duration, and consecutive failures.

For the tested job of choosing a known command or flag, Jev is a promising
optional discovery provider. Keep local completion available, and evaluate
fresh independently labeled tasks against the production retrieval path before
adopting it. Luna's ability to construct novel commands or larger plans is
outside this experiment. Recheck the result after model, prompt, catalog,
interface, reasoning, pricing, or network changes.
