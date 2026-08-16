# Documentation system

Quirl has two deliberately connected documentation layers. They serve different
consumers, and each has one authoritative source.

## Rust API documentation

Public Rust APIs use ordinary Rust doc comments: `//!` for crates and modules,
and `///` for public items. A useful contract states what the item represents or
does and records relevant invariants, units, limits, side effects, errors, and
security assumptions. It does not just expand the item's name into a sentence.

The workspace denies Rust's `missing_docs` lint. It also denies broken intra-doc
links, bare URLs, and malformed Rust code blocks. The existing canonical gate
runs the documentation build automatically:

```console
cargo xtask check
```

For a faster documentation-only iteration, run:

```console
cargo xtask docs
```

That command builds every workspace crate with dependencies excluded and all
Rustdoc warnings denied. Rust examples in doc comments are exercised by the
workspace tests as doctests. Generated HTML lives under `target/doc` and is not
checked in.

## Command and extension documentation

Rust has no stable runtime reflection for `///` comments, so Quirl does not
scrape source files or pretend Rustdoc can supply runtime command metadata.
Instead, the versioned semantic catalog is the command-level equivalent of a
docstring:

| Interface | Authoritative source | Consumers |
| --- | --- | --- |
| Builtin commands and arguments | `Catalog::builtin()` in `quirl-catalog` | interactive help, completion, `quirl catalog`, `quirl describe`, `quirl doc`, LSP, MCP catalog output, and `quirl agent` |
| Lua host API | `HOST_API` in `quirl-lua` | LuaLS stubs, JSON, Markdown, LSP, and the AI capability catalog |
| Imported or plugin commands | validated `CommandSpec` records with provenance | the same catalog projections after composition |

Exact catalog records must include stable identity, version, signature,
summary, details, typed arguments and their documentation, examples, I/O,
effects, and exit-code descriptions. `Catalog::quality_issues()` and its tests
reject incomplete records. The CLI contract test independently checks that
every visible Clap leaf and argument agrees structurally with the catalog, so a
new command cannot silently disappear from generated documentation or AI
discovery.

Clap doc comments remain short parser-facing navigation text. Full command
contracts belong only in the catalog. Changing a catalog record automatically
updates every catalog consumer; changing `HOST_API` requires
`cargo xtask sdk` to refresh the checked-in Lua stub.

## Adding or changing an interface

1. Add `///` documentation with the public Rust item.
2. If it is a command or argument, update `Catalog::builtin()` in the same
   change. If it is a Lua host capability, update `HOST_API` and regenerate the
   SDK.
3. Add behavioral tests for the contract, including limits and errors where
   relevant.
4. Run `cargo xtask check`. No separate documentation checklist or personal
   skill is required; the repository gate remembers the rules.
