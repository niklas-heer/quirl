//! Embeds reproducible build metadata in Quirl's benchmark executable.

// The unpublished release harness deliberately embeds the exact same source
// identity recipe as the CLI so it can prove both artifacts were built from
// one clean candidate. Loading it as a module preserves that file's inner
// crate documentation; textual inclusion would make its `//!` invalid here.
#[path = "../quirl-cli/build.rs"]
mod cli_build;

fn main() {
    cli_build::main();
}
