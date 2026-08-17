# Changelog

Notable user-visible changes to Quirl are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases use
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A native C1-core command graph on Linux and macOS with byte pipelines,
  redirects, boolean lists, bounded expansions, background jobs, and job
  control.
- An explicit typed-data mode, shared semantic catalog, completion and picker
  surfaces, durable history, generated documentation, and a stdio language
  server.
- One sandboxed Lua 5.4 SDK for configuration, scripts, prompt segments,
  completions, tests, and trusted in-process plugins.
- Permission-locked plugin packages, typed extension events, live views,
  recovery records, bounded out-of-process adapter initialization, and typed
  trusted-Lua plugin command dispatch.
- Plain, Unicode, and opt-in Nerd Font prompt profiles with `NO_COLOR` and
  `TERM=dumb` fallbacks.
- A Ratatui inline surface selected by default on capable TTYs, with a
  Quirl-owned editor, context and status rows, syntax highlighting, advisory
  diagnostics, documented completion, typed overlays, autosuggestions, and
  transient prompts that preserve normal scrollback and PTY handoff.
- Reproducible performance gates, a text-only product tour, and a real-PTY demo
  recipe for release review.
- A repository-local Cargo `xtask` with typed check, test, SDK, demo, and
  release commands; no separate task-runner installation is required.
- Bounded seeded native C1 generation with reproducible differential comparison
  against available Bash and Zsh reference shells.

### Security

- Lua VMs enforce memory, instruction, deadline, and cancellation policies and
  expose only a restricted standard library.
- Plugin sources are integrity checked and receive only their locked grants;
  isolated adapters have exact launch grants, deadlines, output bounds, and
  process-tree containment.
- Terminal-derived and extension-derived text is sanitized before rendering.

### Changed

- The capable-TTY default is now the rich Ratatui inline frame. Reedline remains
  the explicit and automatic simple-terminal fallback; its removal is not part
  of this change.
- Advanced the pre-release config contract to v3. It retains the v2 rich-surface
  settings and adds validated shared semantic themes; unversioned v0 and
  explicit v1/v2 configurations migrate to v3 defaults before validation, while
  future versions fail closed.
- Added a release-only website gate with non-mutating generated-mirror
  freshness, semantic release-evidence attribution, lint, route type checking,
  and a production build. It uses the exact `website/package-lock.json`
  dependency graph and is not part of narrow Rust-only checks.

### Known limitations

- Interactive 0.1 support targets Linux and macOS. Windows remains a
  best-effort portability target without native terminal validation.
- Here-documents, process substitution, loops, functions, conditionals, and
  dialect control forms require an explicit Bash or Zsh island.
- Wasm components are validated but not executed, and publishing remains a
  local dry-run workflow rather than a remote registry.
- The rich and Reedline editor cores intentionally coexist. Rich-keymap parity
  and a replacement minimal fallback are required before Reedline can be
  removed.

<!-- BEGIN QUIRL RELEASE EVIDENCE STATUS -->
> **Release evidence status — historical.** Artifact evidence for measured candidate `23fd5d36907fc816bdafd9aa3c2dcb3afb69feb5` and artifact `9a893a5f1a0b49d62712f331c88966113d910d94efa9651dc4feffe9fd55b637` is historical.
> Evidence commit `14e70939d039d96c195f57452a0e1ec3928194af` documents that measurement. It is not evidence for the corrected implementation, which has no fresh exact-candidate measurement.
> Human review on named Linux and macOS terminals, remote-PTY review, and real-terminal demo review remain incomplete.
<!-- END QUIRL RELEASE EVIDENCE STATUS -->

The first version entry will be cut only after the exact candidate passes the
[release checklist](docs/release-checklist.md). Until then, everything above is
part of the unreleased 0.1 candidate.
