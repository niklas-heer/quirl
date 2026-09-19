# Decisions

Decision records in [vrdx](https://github.com/niklas-heer/vrdx) format: one Markdown
file per decision, with TOML metadata between `+++` lines. Read them in any editor;
the CLI is for querying, validating and creating them.

```sh
brew install niklas-heer/tap/vrdx
vrdx --dir docs/decisions list
vrdx --dir docs/decisions show <id-or-prefix>
vrdx --dir docs/decisions validate
```

Add a record with `vrdx --dir docs/decisions new "<title>" --body-file <file>`, then edit its status
and relationships in the Markdown. Run `vrdx guide` for the writing conventions.
Keep superseded records; mark the replacement instead of rewriting history.
