# Security policy

Quirl executes commands and hosts extensions, so security reports are taken
seriously throughout the 0.1.0 release-candidate process and after publication.

## Supported versions

The latest immutable `0.1.x` GitHub Release is the supported release line. When
no `0.1.x` release exists yet, security fixes are made on `main` and included
in the next exact candidate. Old commits, local forks, and untagged build
artifacts are not maintained as separate support lines.

## Report a vulnerability

Please do not open a public issue for a suspected vulnerability. Email
[me@nheer.io](mailto:me@nheer.io) with the subject `Quirl security report`.
Include, where possible:

- the affected commit, platform, and terminal;
- the security boundary you expected Quirl to enforce;
- minimal reproduction steps or a proof of concept;
- the observed impact and any known workaround;
- whether the report or its details may be credited publicly.

Do not include real credentials, private plugin sources, or other people's data.
The maintainer will coordinate validation, a fix, release timing, and disclosure
with you. Please keep the report private until that process is complete. Quirl
does not currently operate a bug-bounty program.

## Scope

Reports about the following boundaries are particularly useful:

- escaping the restricted Lua runtime or bypassing a `LuaPolicy` budget;
- gaining plugin capabilities that were not granted and integrity-locked;
- command, argument, environment, or shell-island injection;
- terminal escape injection or unsafe rendering of untrusted text;
- unbounded capture, resource exhaustion, cancellation failure, or orphaned
  process trees;
- unauthorized configuration, plugin-state, recovery, or history changes;
- secrets exposed through diagnostics, generated context, demos, or logs.

Known limitations and residual risks are tracked in the
[security and accessibility audit](docs/security-accessibility-audit-v0.1.md).
A documented limitation may still be worth reporting if its impact or reach is
greater than the documentation describes.
