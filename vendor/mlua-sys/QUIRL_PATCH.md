# Quirl `mlua-sys` packaging patch

This directory is the source of crates.io `mlua-sys` 0.11.0, copied from the
published crate. Quirl changes one build-dependency constraint: vendored Lua
uses exact `lua-src` 551.0.0, which contains upstream Lua 5.5.1, instead of the
published `< 550.2.0` range that contains Lua 5.5.0.

The Rust FFI and build logic are otherwise unchanged. Remove the root
`[patch.crates-io]` entry and this directory when a published `mlua-sys`
release accepts `lua-src` 551.x. The upstream source and this copy are MIT
licensed; see `LICENSE`.
