+++
schema_version = 1
id = "01M3QV21G099AW684N4YZ0Z30R"
title = "Complete like Zsh and ask Zsh for arguments"
date = "2026-09-30"
status = "accepted"
tags = ["completion", "zsh", "interactive"]
supersedes = []
superseded_by = []
depends_on = []
related_to = ["01M2XHZ70A9T5C8NJBK1Y98MNF", "01M2XHZ73YV26TQBW9G9084Y8F"]
+++

## Decision

Zsh is the behavioral reference for interactive completion. An explicit Tab
waits until every completion source has answered, then acts like Zsh's
default widget:

- a single match is inserted with its natural suffix: a space, or nothing
  after a directory `/`, an assignment `=`, or a `:`;
- several matches extend the word by their longest shared prefix and open the
  menu; a later Tab advances it;
- scattered fuzzy matches are listed but never typed.

Catalog matches are ranked in tiers: exact-case prefix, then case-insensitive
prefix, then fuzzy subsequence. Only the best non-empty tier is shown,
alphabetically with an exact match first. Command paths complete one word at a
time.

Argument values come from the installed Zsh's own completion functions. On
explicit Tab only, the composition root sends the current command segment
through the existing bounded `LocalCompletionProcess` boundary. That boundary
runs no user startup files, clears the environment except `PATH`, `HOME`,
`USER`, and `LOGNAME`, contains the process group, and enforces a 1.5 s
deadline and a 1,024-candidate limit. Zsh's bundled function directories come
before site directories. The adapter emits each `compadd` group in Zsh's
display order. Quirl-owned commands (`cd`, `quirl`, `mode`, `help`, `exit`),
command positions, and quoted words are not sent to Zsh.

## Context

The catalog knows command structure, but many arguments are dynamic: Git
refs, Make targets, SSH hosts, process IDs, package names. Before this change
Tab only opened a menu. It never inserted a unique match or a common prefix,
fuzzy multi-word catalog paths crowded out plain command names, and dynamic
arguments had no source. Because the catalog is learned incrementally, a
partial catalog would also make unique insertion wrong: with only
`git checkout` known, `git ch` would insert it even though Zsh offers
`checkout`, `cherry`, and `cherry-pick`.

Alternatives considered:

- Reimplement argument providers per command in Lua or Rust. This duplicates
  hundreds of maintained Zsh functions and would lag behind them.
- Keep a persistent Zsh completion server. This is faster (no `compinit` per
  request) but needs a long-lived process with its own lifecycle, restart, and
  isolation design. It can replace the per-request process later without
  changing the interaction contract.
- Source the user's `.zshrc` to reproduce their exact `fpath`. That executes
  arbitrary user code on every Tab, so `QUIRL_ZSH_PATH` covers custom
  completion directories instead.
- Run Zsh for automatic as-you-type suggestions. The per-keystroke cost and
  process churn are unjustified; automatic completion stays in-process.

## Consequences

- Tab produces the same command line as Zsh for files, directories, variables,
  commands, subcommands, and every argument Zsh can complete.
- An explicit Tab in an argument position costs one bounded Zsh process, about
  200 ms including `compinit`. The menu stays hidden until all sources answer,
  so a unique insertion does not flash a menu first; the status bar shows
  that completion is streaming.
- Without Zsh, or without a function for the command, the catalog, filesystem,
  environment, and plugin sources still answer.
- Zsh descriptions produced through `_describe` are not captured yet, so some
  live candidates show no summary; catalog candidates keep theirs.
