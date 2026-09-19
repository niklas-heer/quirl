+++
schema_version = 1
id = "01M2XHZ6WYP05WCZ6BXVKE4AMW"
title = "Preview runtime layers"
date = "2026-08-15"
status = "superseded"
tags = ["runtime"]
supersedes = []
superseded_by = ["01M2XHZ6Z9746X4SH1P1PET9C7"]
depends_on = []
related_to = []
+++
- Superseded by: [ADR 0016](2026-08-16_192326057_reconcile-runtime-layering-and-ownership-contracts.md)

## Context

Phase 1 requires command-mode built-ins, external processes, byte pipes,
redirections, conditions, and jobs to execute through one native graph. Keeping
that lifecycle in the `quirl-core` foundation would add operating-system and
terminal dependencies to a crate whose dependency ceiling is serde-level.

## Decision

`quirl-syntax` owns the serializable command graph and its span-aware parser.
`quirl-process` depends on `quirl-core` and `quirl-syntax` and owns native
process startup, pipe/redirection wiring, process groups, and structured job
state. `quirl-cli` is the composition root. Existing `quirl-core::CommandRunner`
remains temporarily for the restricted Lua host boundary and is not used by
the interactive Preview executor.

Future picker and index crates may likewise depend inward on foundation
schemas, but foundation crates never depend on them.

## Consequences

The interactive shell no longer needs `$SHELL -c` for the accepted native
command grammar. Exact dialect islands and the legacy Lua process adapter stay
explicit compatibility paths. Terminal-specific suspend/foreground ownership
can evolve in `quirl-process` without leaking OS dependencies into foundations.
