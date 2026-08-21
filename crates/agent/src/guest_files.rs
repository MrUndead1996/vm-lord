//! Reading and writing the guest's own files.
//!
//! Small operations both recipes need and neither owns: copying a read-only
//! payload tree into `/usr/src`, writing a configuration file only when it
//! would change, reading a file that may not be there, and saying in one line
//! what a program that failed did.

use std::{fs, io, path::Path};

use crate::command::{self, Outcome};

/// Copies `source` onto `destination`, and says whether anything changed.
///
/// Files that are already byte-for-byte identical are left alone, so a
/// reconnect does not rewrite the tree DKMS is registered against -- rewriting
/// it is what would make DKMS rebuild on every session.
pub fn copy_tree(source: &Path, destination: &Path) -> io::Result<bool> {
    fs::create_dir_all(destination)?;
    let mut changed = false;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            changed |= copy_tree(&from, &to)?;
            continue;
        }
        let wanted = fs::read(&from)?;
        if fs::read(&to).is_ok_and(|present| present == wanted) {
            continue;
        }
        fs::write(&to, &wanted)?;
        changed = true;
    }

    Ok(changed)
}

/// Writes `content` only when the file does not already hold it.
pub fn write_if_different(path: &Path, content: &str) -> io::Result<()> {
    if fs::read_to_string(path).is_ok_and(|present| present == content) {
        return Ok(());
    }
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    fs::write(path, content)
}

/// A file that may not be there, as the empty string.
///
/// Every caller treats "missing" and "empty" the same way -- as a fact that is
/// not there to be read -- and an `io::Error` here would be a second way of
/// saying the same stage did not apply.
pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// One line about a program that did not succeed.
pub fn failure(what: &str, outcome: &Outcome) -> String {
    let ending = match outcome.ending {
        command::Ending::Exited(code) => format!("exited with {code}"),
        command::Ending::TimedOut => "outran its time budget".to_owned(),
        command::Ending::NotStarted => "could not be started".to_owned(),
    };
    format!("{what} {ending}: {}", outcome.output)
}
