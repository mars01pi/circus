//! Copies a caller-authorized source directory, not a Git repository operation.

use std::error::Error;
use std::io::{self, Write};

use circus_sandbox::{CodeSource, SnapshotBudget};

fn main() -> Result<(), Box<dyn Error>> {
    // This program owns the source, so it can explicitly grant read access.
    // A service must authorize a tenant before opening any tenant-supplied path.
    let source_directory = tempfile::tempdir()?;
    std::fs::create_dir(source_directory.path().join("src"))?;
    std::fs::write(
        source_directory.path().join("src/message.txt"),
        b"bytes copied from an explicitly authorized source",
    )?;

    let source = CodeSource::open(source_directory.path())?;
    let mut workspace = source.copy_to_workspace(128, SnapshotBudget::new(128, 2)?)?;
    let observed = workspace.read_file("src/message.txt")?;

    workspace.cleanup()?;
    // Release the read capability before deleting its root (also on Windows).
    drop(source);
    source_directory.close()?;

    let mut stdout = io::stdout().lock();
    stdout.write_all(&observed)?;
    stdout.write_all(b"\n")?;
    Ok(())
}
