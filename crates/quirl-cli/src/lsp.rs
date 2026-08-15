use quirl_catalog::Catalog;
use quirl_core::ShellError;
use std::io;

pub fn execute(catalog: Catalog) -> Result<i32, ShellError> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    quirl_lsp::serve(&mut reader, &mut writer, catalog)?;
    Ok(0)
}
