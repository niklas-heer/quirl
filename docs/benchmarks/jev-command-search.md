# Jev command-selection experiment

Measured 2026-09-19 with `jev-1.13.0` through TypeSafe's official HTTPS API.
This is research evidence, not a decision to add Jev to Quirl.

## Result

Jev handled this small command-discovery benchmark well. Of 112 distinct
hand-authored cases, 111 returned validated answers. All 111 were acceptable
after correcting one demonstrably mistaken expected label. One HTTP-200
response failed the harness validator and remains unassessed. This is not
evidence of perfect accuracy on real shell workloads.

| Set | Requests | Outcome | Median | p95 |
| --- | ---: | --- | ---: | ---: |
| Initial | 76 | 76 acceptable | 458.21 ms | 555.45 ms |
| Challenge | 36 | 35 acceptable after one label correction; 1 validation rejection | 448.13 ms | 812.36 ms |
| Reversed candidate order | 32 | 32 acceptable | 441.07 ms | 506.34 ms |
| Combined main measurements | 144 | 143 acceptable; 1 unassessed | 451.79 ms | 545.57 ms |

The initial set covers 32 ordinary command requests, eight distinctions between
similar commands, eight multilingual requests, four unavailable capabilities,
four ambiguous requests, four requests requiring command composition, and 16
flag requests. The challenge adds 24 flag questions and 12 command questions
covering negation, misleading filenames, unsupported guarantees, and composition.
It was authored after seeing the initial results and is exploratory, not an
independent held-out test set. One intentionally broad compression request accepts
several plausible commands or clarification.

Reversing candidate order sometimes changed the selected equivalent tool, such
as `rg` versus `grep`, but all 32 remained within their original accepted sets.
The subsets and expected labels were fixed before each corresponding run.

## Setup and measurement

- Source: the checked-in native catalog, all 62 command records, without
  installed-executable filtering or Quirl's builtins. Each command choice used
  its name, summary, and description; flag questions used the selected command's
  macOS/any-platform flag metadata.
- Catalog SHA-256:
  `8ed8f724753201bade47df6d30f22676cfa1d25a7063cae7a47a4b0371ecbf71`.
- Command selection used one Choice question over the full command set plus
  explicit `NONE`, `CLARIFY`, and `COMPOSE` alternatives. It did not first
  retrieve a shortlist and did not generate command text or arguments.
- Instructions allowed normal flags and arguments but excluded invented
  scripts, arbitrary subprocess workarounds, and partial steps substituted for
  the complete requested outcome.
- Calls were sequential from the test machine, with one persistent Python
  process in a persistent shell. Each set reused an HTTPS connection. The first
  request in each set included connection setup; measured first requests were
  832–922 ms. Main-request minimum/maximum were 280.70/922.45 ms.
- Latency starts before HTTP request transmission and ends after reading the
  response body. It includes network/service time and excludes fixture loading,
  1Password access, request serialization, and response validation. p95 uses
  the nearest-rank definition. Geography and server latency were not measured.
- Each request has a 10-second socket timeout, 200 KB request cap, and 1 MB
  response cap. Each set has a 100-case cap and a 600-second wall-budget check
  between requests. There are no automatic retries. A failed validation closes
  the connection before the next request. Socket timeouts are inactivity bounds,
  not a strict total HTTP deadline; this is research tooling, not the runtime.
- The credential was captured directly from 1Password into process memory.
  Only synthetic queries and public catalog metadata went to TypeSafe. No
  returned command was executed. No credentials are included in these artifacts.

Known usage across the 144 main requests and five diagnostic requests was
321,152 input tokens. At the documented $0.042 per million input tokens, that is
**$0.01349**, excluding the rejected response's unretained usage. This is an
estimate, not an invoice. Output tokens are uncharged under the documented
pricing. See [TypeSafe's model documentation](https://docs.typesafe.ai/models).
An additional advisory knowledge-capture call is outside these benchmark totals.

## Investigated anomalies

### Incorrect expected label

Challenge `stress-command-07` asks for recursive directory-size totals. The
original expected answer was `NONE`; Jev selected `tree` with confidence 0.59.
Inspection of the catalog showed `tree --du` explicitly computes directory
sizes from their contents, and `eza --total-size` also provides directory
totals. The test author had omitted these options while labeling the case.

The evidence preserves the original query, expected label, response, and
uncorrected summary. A separate correction records `tree`/`eza` as acceptable.
The corrected result above must not be presented as an unchanged preregistered
score. Disk allocation versus apparent size was not separately specified or
tested in this query.

### Unclassified validation rejection

Challenge `stress-flag-11` asks ripgrep to search all file types except a
specified type; the expected answer is `--type-not`. The API returned HTTP 200
in 444.93 ms, but one harness assertion failed. The original harness did not
retain the response before validation, so the failing field and the model's
choice cannot be recovered. An API defect cannot be distinguished from an
overstrict harness check from this evidence.

Five subsequent diagnostic requests, using reversed candidate order, all selected `--type-not`, with confidence
0.96–0.98, complete option maps, and probabilities summing to 1. These probes
do not erase or replace the original unassessed sample or reproduce its exact
candidate order. The saved harness now
retains parsed answer/model/usage fields before validation for future diagnosis;
this change does not retroactively recover the missing response.

## Interpretation and limits

This supports further evaluation of Jev as an optional command/flag selector.
The full 62-command catalog already fits one request, so an initial prototype
need not assume a separate reranking stage. Larger catalogs require separate
coverage, payload, cost, and latency measurements.

This benchmark does **not** measure production hybrid-search improvement,
candidate recall, complete command construction, correct argument values,
execution safety, adversarial metadata, outage behavior, or sustained service
availability. It has no same-machine local-retrieval baseline and no other-model
baseline. The historical 118.6 ms local cold median in
[ADR 0025](../decisions/0025-fine-tuned-command-retrieval-model.md) uses a different
workload and is not a controlled comparison. A 452 ms median network request
does not establish an improvement to ordinary local completion latency.

Confidence is not a correctness or execution guarantee. Several correct answers
had confidence near 0.5, including ambiguous routing and equivalent flag choices.
No acceptance threshold was tuned or validated here. These short synthetic
queries and 62 command descriptions are not representative of arbitrary shell
requests or a fully composed catalog. Future promotion needs fresh independently
labeled queries and the production retrieval baseline, including no-match and
multilingual cases, without selecting on this now-consumed fixture.

## Evidence and reproduction

[The experiment directory](../../spikes/jev-command-search/README.md) contains
the harness and a compact JSON evidence bundle with frozen requests, original
labels, all retained answers/distributions, timing, usage, and the explicit label
correction. Recheck these findings when the model, prompt, catalog, network
location, or intended workload changes.
