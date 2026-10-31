//! Crash-safe persistence for small files holding secrets or state.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Atomically replaces `path` with `contents`: writes a temporary sibling file, then renames it.
/// The file is readable by its owner only (0600 on unix). Parent directories are created.
pub fn write_private_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("tmp");
    write_private(&tmp_path, contents)?;
    fs::rename(&tmp_path, path)
}

fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    // A leftover file could carry looser permissions; always create it fresh with 0600.
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}
