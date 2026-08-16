# ADR 0009: Execute the narrow isolated process-adapter v1 handshake

- Status: Accepted
- Date: 2026-08-16
- Decision owners: Quirl maintainers
- Applies to: `quirl-plugin` contracts and `quirl-cli` activation

## Context

The plugin platform had validated Wasm and out-of-process boundaries, but
refused every enabled isolated plugin. That made the isolation contract
non-executing and left the platform acceptance gate incomplete.

## Decision

Quirl v0.1 executes the out-of-process `quirl.plugin.v1` initialization
handshake. `quirl-plugin` owns the serde `deny_unknown_fields` request and
response records; `quirl-cli`, the composition root, owns process creation.

The initial protocol is intentionally assertion-only. It sends one
newline-delimited JSON `initialize` request and accepts exactly one
newline-delimited `ready` response. It exposes no host callbacks, catalog
registrations, event payloads, filesystem handles, environment, or inherited
stdin. A future protocol revision must add each such authority explicitly.

An adapter must use its checksummed package entry as its executable and request
exactly `process.spawn:<entry>`. The lock must grant exactly that scoped
capability. The child gets a controlled package working directory, a cleared
environment, piped stdin/stdout/stderr, process-tree containment, a bounded
deadline, a shared stdout+stderr byte cap, and cooperative cancellation where
the caller supplies a cancellation flag. Any malformed, extra, timed-out,
oversized, nonzero, or tampered response fails closed as a `ShellError`.

Wasm components remain validated but disabled: no production component engine
is selected in v0.1.

## Consequences

- An enabled process adapter is genuinely launched and verified during
  `quirl plugin enable` and managed-plugin activation.
- The out-of-process boundary contains host integration authority, but it is
  not an operating-system sandbox for arbitrary local code. The child still
  runs with the user's OS identity; its only supported protocol authority is
  the checked, scoped launch itself.
- Process adapters may contribute static, manifest-validated catalog facts;
  runtime command/event delegation awaits a separately versioned protocol.
