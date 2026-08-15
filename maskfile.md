# Quirl development tasks

The commands below are the canonical local verification workflow for Quirl.
Run `mask check` before every commit.

## fmt

> Format every crate in the Rust workspace.

```sh
cargo fmt --all
```

## lint

> Run Clippy for every workspace target and deny warnings.

```sh
cargo clippy --workspace --all-targets -- -D warnings
```

## test

> Run Rust workspace tests followed by the guest-side Lua tests.

```sh
cargo test --workspace \
  && cargo run -p quirl-cli -- test examples/lua_tests.lua
```

## check

> Run the complete pre-commit verification suite without modifying sources.

```sh
cargo fmt --all -- --check \
  && cargo run --quiet -p quirl-cli -- fmt examples --check \
  && $MASK lint \
  && $MASK test
```

## sdk

> Regenerate the checked-in LuaLS SDK stub from the Rust `HOST_API` definitions.

```sh
cargo run --quiet -p quirl-cli -- sdk --format text > docs/quirl.lua
```

## run

> Start the interactive Quirl shell.

```sh
cargo run -p quirl-cli
```
