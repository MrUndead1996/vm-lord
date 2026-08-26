//! The viewer's side of a file transfer: what `CF_HDROP` names, what a tree
//! holds, and where an arriving tree is put.
//!
//! Every filesystem object is opened with `FILE_FLAG_OPEN_REPARSE_POINT` and
//! then judged by the attributes of the handle, never by what a path looked
//! like a moment earlier. A junction is a reparse point, so a directory the
//! user copies cannot quietly stand for another one, and neither can a
//! destination a transfer is writing into.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File},
    io::{self, Read, Write},
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use vmlord_display_protocol::clipboard::{
    CHUNK,
    files::{EntryKind, MAX_ENTRIES, Policy},
    path::{PathError, ValidatedPath},
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

use crate::windows::files::{create_new, make_directory, open_no_reparse};

/// Where a viewer stages what arrives, under the user's own profile.
const STAGING: [&str; 2] = ["VMLord", "Clipboard"];

/// The marker that says a staged tree is whole, written beside it so that no
/// name inside a tree can ever be mistaken for one.
const COMMITTED: &str = "committed";

/// How many names one transfer may try before it gives up.
///
/// A transfer id restarts at one whenever a channel rebinds, while a committed
/// tree outlives the connection that brought it: without this, the first
/// transfer after a reconnect would land on a directory that is still there
/// and fail, and go on failing until the old tree aged out.
const STAGING_NAMES: u32 = 64;

/// Why a tree could not be read or could not be written.
#[derive(Debug)]
pub enum FileError {
    /// The filesystem refused something.
    Io(io::Error),
    /// A reparse point, a device, or anything else that is not a plain file or
    /// a plain directory.
    Unsupported,
    /// Over the per-file or per-transfer limit.
    TooLarge,
    /// More entries than a tree may have.
    TooMany,
    /// Two entries that are one name.
    Duplicate,
    /// A destination that is already there.
    Exists,
    /// A file that is not the length it said it was.
    Changed,
    /// A name that is not a path this protocol carries.
    Path(PathError),
    /// A path with no last component, such as a bare drive.
    NoName,
    /// No per-user profile directory to stage into.
    NoProfile,
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "the filesystem refused it: {error}"),
            Self::Unsupported => write!(f, "only files and directories may be copied"),
            Self::TooLarge => write!(f, "the transfer is over its configured limit"),
            Self::TooMany => write!(f, "the tree has more than {MAX_ENTRIES} entries"),
            Self::Duplicate => write!(f, "two entries would be the same file"),
            Self::Exists => write!(f, "the destination is already there"),
            Self::Changed => write!(f, "the file changed while it was being read"),
            Self::Path(error) => write!(f, "{error}"),
            Self::NoName => write!(f, "the path names nothing to copy"),
            Self::NoProfile => write!(f, "there is no LOCALAPPDATA to stage into"),
        }
    }
}

impl std::error::Error for FileError {}

impl From<io::Error> for FileError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<PathError> for FileError {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

/// One thing a walk of a selection produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Produced {
    /// An entry, before any of its bytes.
    Entry {
        /// Its path, relative to the root of the tree.
        path: String,
        /// What it is.
        kind: EntryKind,
        /// How long a regular file is.
        size: u64,
    },
    /// The next bytes of the entry that is open, never logged.
    Chunk(Vec<u8>),
}

/// The `DROPFILES` block that offers these paths as a file selection.
#[must_use]
pub fn dropfiles_of(paths: &[PathBuf]) -> Vec<u8> {
    // `DROPFILES`: the offset of the list, a point, `fNC`, and `fWide`.
    let mut block = Vec::new();
    block.extend_from_slice(&20u32.to_le_bytes());
    block.extend_from_slice(&[0u8; 8]);
    block.extend_from_slice(&0u32.to_le_bytes());
    block.extend_from_slice(&1u32.to_le_bytes());

    for path in paths {
        for unit in path.as_os_str().encode_wide().chain(std::iter::once(0)) {
            block.extend_from_slice(&unit.to_le_bytes());
        }
    }
    // The list ends with a NUL of its own, after the one that ended the last
    // name.
    block.extend_from_slice(&0u16.to_le_bytes());

    block
}

/// Where every session's staging lives, if this user has a profile at all.
#[must_use]
pub fn staging_root() -> Option<PathBuf> {
    let mut root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    for component in STAGING {
        root.push(component);
    }

    Some(root)
}

/// Removes staged trees that nothing can refer to any more.
///
/// Clipboard data outlives the process that put it there, so a committed tree
/// is kept for `retention` rather than deleted when the viewer exits; a tree
/// with no marker beside it never became a selection at all and goes now.
/// `root` is the staging root, whose children are sessions and whose
/// grandchildren are transfers.
pub fn cleanup(root: &Path, now: SystemTime, retention: Duration) {
    for session in directories_in(root) {
        for transfer in directories_in(&session) {
            let marker = marker_of(&transfer);
            let kept = fs::metadata(&marker)
                .and_then(|marker| marker.modified())
                .map(|at| now.duration_since(at).is_ok_and(|age| age <= retention))
                .unwrap_or(false);
            if !kept {
                remove_tree(&transfer);
                let _ = fs::remove_file(&marker);
            }
        }
    }
}

/// The plain directories directly inside one directory.
///
/// A reparse point is not one of them: cleanup deletes, and deleting through
/// something that stands for another place is how a cleanup becomes a loss.
fn directories_in(directory: &Path) -> Vec<PathBuf> {
    let Ok(listing) = fs::read_dir(directory) else {
        return Vec::new();
    };

    listing
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            fs::symlink_metadata(path).is_ok_and(|metadata| {
                metadata.is_dir()
                    && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
            })
        })
        .collect()
}

/// What one transfer's directory is called on its nth attempt.
fn staging_name(transfer: u32, attempt: u32) -> String {
    if attempt == 0 {
        transfer.to_string()
    } else {
        format!("{transfer}-{attempt}")
    }
}

/// Where the marker for a staging root lives.
fn marker_of(root: &Path) -> PathBuf {
    let mut marker = root.as_os_str().to_owned();
    marker.push(".");
    marker.push(COMMITTED);

    PathBuf::from(marker)
}

/// One directory of a walk, and where in it the walk is.
struct Frame {
    path: PathBuf,
    prefix: String,
    names: Vec<OsString>,
    at: usize,
}

/// The file being read out, and how much of it is left.
struct Reading {
    file: File,
    remaining: u64,
}

/// A depth-first walk of the paths a selection named.
pub struct SourceTree {
    policy: Policy,
    roots: Vec<PathBuf>,
    root_at: usize,
    stack: Vec<Frame>,
    reading: Option<Reading>,
    entries: usize,
    total: u64,
    keys: HashSet<String>,
}

impl SourceTree {
    /// Opens a walk of these top-level paths.
    ///
    /// # Errors
    ///
    /// [`FileError`] if a path has no name, a name the wire cannot carry, a
    /// name another top-level path already has, or is not a plain file or
    /// directory.
    pub fn open(paths: &[PathBuf], policy: Policy) -> Result<Self, FileError> {
        let mut keys = HashSet::new();
        for path in paths {
            let name = path
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or(FileError::NoName)?;
            let parsed = ValidatedPath::parse(name)?;
            if !keys.insert(parsed.windows_key().to_owned()) {
                return Err(FileError::Duplicate);
            }

            // Refuse the whole selection now rather than half-way through it.
            let opened = open_no_reparse(path)?;
            drop(opened);
        }

        Ok(Self {
            policy,
            roots: paths.to_vec(),
            root_at: 0,
            stack: Vec::new(),
            reading: None,
            entries: 0,
            total: 0,
            keys: HashSet::new(),
        })
    }

    /// The next entry, or the next chunk of the entry that is open.
    ///
    /// # Errors
    ///
    /// [`FileError`] for anything that ends the transfer: a reparse point, a
    /// limit, a name the wire cannot carry, or the filesystem refusing.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Produced>, FileError> {
        if let Some(chunk) = self.read_on()? {
            return Ok(Some(chunk));
        }

        while let Some(frame) = self.stack.last_mut() {
            let Some(name) = frame.names.get(frame.at).cloned() else {
                self.stack.pop();
                continue;
            };
            frame.at += 1;

            let text = name.to_str().ok_or(FileError::NoName)?.to_owned();
            let path = format!("{}/{text}", frame.prefix);
            let full = frame.path.join(&name);

            return self.admit(path, &full).map(Some);
        }

        let Some(root) = self.roots.get(self.root_at).cloned() else {
            return Ok(None);
        };
        self.root_at += 1;

        let name = root
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or(FileError::NoName)?
            .to_owned();

        self.admit(name, &root).map(Some)
    }

    /// Accounts for one entry and, if it is a directory, descends.
    fn admit(&mut self, path: String, full: &Path) -> Result<Produced, FileError> {
        let parsed = ValidatedPath::parse(&path)?;
        let opened = open_no_reparse(full)?;
        let kind = opened.kind;
        let size = opened.size;

        if self.entries >= MAX_ENTRIES {
            return Err(FileError::TooMany);
        }
        if size > self.policy.max_file_bytes() {
            return Err(FileError::TooLarge);
        }
        let total = self.total.checked_add(size).ok_or(FileError::TooLarge)?;
        if total > self.policy.max_transfer_bytes() {
            return Err(FileError::TooLarge);
        }
        if !self.keys.insert(parsed.windows_key().to_owned()) {
            return Err(FileError::Duplicate);
        }

        self.entries += 1;
        self.total = total;

        match kind {
            EntryKind::Directory => {
                self.stack.push(Frame {
                    path: full.to_owned(),
                    prefix: path.clone(),
                    names: names_in(full)?,
                    at: 0,
                });
            }
            EntryKind::File if size > 0 => {
                self.reading = Some(Reading {
                    file: opened.file,
                    remaining: size,
                });
            }
            EntryKind::File => {}
        }

        Ok(Produced::Entry { path, kind, size })
    }

    /// The next chunk of the file being read, if one is open.
    fn read_on(&mut self) -> Result<Option<Produced>, FileError> {
        let Some(reading) = self.reading.as_mut() else {
            return Ok(None);
        };

        let want = usize::try_from(reading.remaining.min(CHUNK as u64)).unwrap_or(CHUNK);
        let mut chunk = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            let read = reading.file.read(&mut chunk[filled..])?;
            if read == 0 {
                return Err(FileError::Changed);
            }
            filled += read;
        }

        reading.remaining -= want as u64;
        if reading.remaining == 0 {
            self.reading = None;
        }

        Ok(Some(Produced::Chunk(chunk)))
    }
}

/// Where an arriving tree is written, and what happens to it if it stops.
pub struct Staging {
    root: PathBuf,
    open: Option<File>,
    top_level: Vec<String>,
    committed: bool,
}

impl Staging {
    /// A staging root for one transfer, under this user's profile.
    ///
    /// `%LOCALAPPDATA%` is already private to the user, and a directory made
    /// there inherits that, so nothing here widens what the profile allows.
    ///
    /// # Errors
    ///
    /// [`FileError::NoProfile`] without `LOCALAPPDATA`, and [`FileError::Io`]
    /// if the directories cannot be made.
    pub fn create(session: &str, transfer: u32) -> Result<Self, FileError> {
        Self::create_at(
            &staging_root().ok_or(FileError::NoProfile)?,
            session,
            transfer,
        )
    }

    /// A staging root under a base the caller chose, for one session.
    ///
    /// The base is a parameter so that nothing but a real session ever writes
    /// into a real profile: a test names a directory of its own.
    ///
    /// # Errors
    ///
    /// [`FileError::Io`] if the directories cannot be made.
    pub fn create_at(base: &Path, session: &str, transfer: u32) -> Result<Self, FileError> {
        let mut root = base.to_owned();
        root.push(session);
        fs::create_dir_all(&root)?;

        Self::create_under(&root, transfer)
    }

    /// A staging root under a directory the caller chose.
    ///
    /// # Errors
    ///
    /// [`FileError::Io`] if the root cannot be created fresh.
    pub fn create_under(base: &Path, transfer: u32) -> Result<Self, FileError> {
        for attempt in 0..STAGING_NAMES {
            let root = base.join(staging_name(transfer, attempt));
            match make_directory(&root) {
                Ok(()) => {
                    return Ok(Self {
                        root,
                        open: None,
                        top_level: Vec::new(),
                        committed: false,
                    });
                }
                // Something of that name is already there -- a tree from
                // before a reconnect, most likely -- so this one goes beside
                // it rather than failing.
                Err(FileError::Exists) => {}
                Err(error) => return Err(error),
            }
        }

        Err(FileError::Exists)
    }

    /// Creates one entry of the arriving tree.
    ///
    /// # Errors
    ///
    /// [`FileError::Exists`] if anything is already at the destination,
    /// [`FileError::Unsupported`] if a component became a reparse point, and
    /// [`FileError::Io`] if a component is missing.
    pub fn create_entry(
        &mut self,
        path: &ValidatedPath,
        kind: EntryKind,
        size: u64,
    ) -> Result<(), FileError> {
        self.open = None;

        let mut components: Vec<&str> = path.components().collect();
        let name = components.pop().expect("a validated path has a component");

        let mut full = self.root.clone();
        for component in &components {
            full.push(component);
            // Every directory above the destination is inspected again, so a
            // component swapped for a junction after it was created is caught
            // before anything is written through it.
            let opened = open_no_reparse(&full)?;
            if opened.kind != EntryKind::Directory {
                return Err(FileError::Unsupported);
            }
        }
        full.push(name);

        match kind {
            EntryKind::Directory => make_directory(&full)?,
            EntryKind::File => {
                let file = create_new(&full)?;
                if size > 0 {
                    self.open = Some(file);
                }
            }
        }

        if components.is_empty() {
            self.top_level.push(name.to_owned());
        }

        Ok(())
    }

    /// Appends the next bytes of the entry that is open.
    ///
    /// # Errors
    ///
    /// [`FileError::Changed`] if no entry is open and [`FileError::Io`] if the
    /// write fails.
    pub fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), FileError> {
        let file = self.open.as_mut().ok_or(FileError::Changed)?;
        file.write_all(bytes)?;

        Ok(())
    }

    /// Keeps the tree, marks it whole, and answers with its top-level paths.
    ///
    /// # Errors
    ///
    /// [`FileError::Io`] if the last file or the marker cannot be written.
    pub fn commit(mut self) -> Result<Vec<PathBuf>, FileError> {
        if let Some(mut file) = self.open.take() {
            file.flush()?;
        }
        // The marker's own timestamp is what cleanup ages the tree by.
        fs::write(marker_of(&self.root), [])?;
        self.committed = true;

        Ok(self
            .top_level
            .iter()
            .map(|name| self.root.join(name))
            .collect())
    }

    /// Closes what is open and removes everything staged.
    pub fn abort(self) {
        drop(self);
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.committed {
            self.open = None;
            remove_tree(&self.root);
        }
    }
}

/// Removes a tree without ever descending through a reparse point.
fn remove_tree(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };

    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        // The link itself, whatever it points at.
        if fs::remove_dir(path).is_err() {
            let _ = fs::remove_file(path);
        }

        return;
    }

    if metadata.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                remove_tree(&entry.path());
            }
        }
        let _ = fs::remove_dir(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

/// The names in a directory, in a fixed order.
fn names_in(path: &Path) -> Result<Vec<OsString>, FileError> {
    let mut names: Vec<OsString> = fs::read_dir(path)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    names.sort();

    Ok(names)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn policy() -> Policy {
        Policy::new(1024, 4096, 3600)
    }

    fn temporary_directory() -> PathBuf {
        // A counter beside the clock: tests run at once, and a clock whose
        // resolution is coarser than they are would hand two of them the same
        // directory.
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let unique_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after the epoch")
            .as_nanos();
        let count = NEXT.fetch_add(1, Ordering::Relaxed);

        std::env::temp_dir().join(format!("vmlord-clipboard-files-test-{unique_id}-{count}"))
    }

    fn drained(source: &mut SourceTree) -> Result<Vec<Produced>, FileError> {
        let mut produced = Vec::new();
        while let Some(item) = source.next()? {
            produced.push(item);
        }

        Ok(produced)
    }

    fn entry(path: &str, kind: EntryKind, size: u64) -> Produced {
        Produced::Entry {
            path: path.to_owned(),
            kind,
            size,
        }
    }

    fn validated(path: &str) -> ValidatedPath {
        ValidatedPath::parse(path).expect("a path the protocol allows")
    }

    fn stage(staging: &mut Staging, path: &str, kind: EntryKind, size: u64) {
        staging
            .create_entry(&validated(path), kind, size)
            .unwrap_or_else(|error| panic!("{path} could not be staged: {error}"));
    }

    #[test]
    fn a_dropfiles_block_is_the_wide_list_windows_expects() {
        let block = dropfiles_of(&[PathBuf::from(r"C:\a\b.txt"), PathBuf::from(r"C:\c")]);

        assert_eq!(u32::from_le_bytes(block[0..4].try_into().unwrap()), 20);
        // `fWide`, which is what makes the list UTF-16.
        assert_eq!(u32::from_le_bytes(block[16..20].try_into().unwrap()), 1);

        let units: Vec<u16> = block[20..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let names: Vec<String> = units
            .split(|unit| *unit == 0)
            .filter(|name| !name.is_empty())
            .map(String::from_utf16_lossy)
            .collect();

        assert_eq!(names, [r"C:\a\b.txt", r"C:\c"]);
        // One NUL of its own after the last name, and one to end the list.
        assert_eq!(units[units.len() - 2..], [0, 0]);
    }

    #[test]
    fn a_path_with_no_name_is_not_a_top_level_entry() {
        let opened = SourceTree::open(&[PathBuf::from(r"C:\")], policy());

        assert!(matches!(opened, Err(FileError::NoName)));
    }

    #[test]
    fn a_tree_is_walked_depth_first_with_every_file_after_its_directory() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("tree/inner")).expect("a tree");
        fs::write(root.join("tree/a.txt"), b"abc").expect("a file");
        fs::write(root.join("tree/inner/b.bin"), b"xy").expect("a file");

        let mut source = SourceTree::open(&[root.join("tree")], policy()).expect("a tree");
        let produced = drained(&mut source).expect("a walk");

        assert_eq!(
            produced,
            vec![
                entry("tree", EntryKind::Directory, 0),
                entry("tree/a.txt", EntryKind::File, 3),
                Produced::Chunk(b"abc".to_vec()),
                entry("tree/inner", EntryKind::Directory, 0),
                entry("tree/inner/b.bin", EntryKind::File, 2),
                Produced::Chunk(b"xy".to_vec()),
            ]
        );

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn a_walk_holds_the_policy_it_was_opened_with() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("tree")).expect("a tree");
        fs::write(root.join("tree/big.bin"), vec![0u8; 2048]).expect("a file");

        let mut source = SourceTree::open(&[root.join("tree")], policy()).expect("a tree");

        assert!(matches!(drained(&mut source), Err(FileError::TooLarge)));

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn two_top_level_paths_with_one_name_are_refused() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("one")).expect("a directory");
        fs::create_dir_all(root.join("two")).expect("a directory");
        fs::write(root.join("one/same.txt"), b"a").expect("a file");
        fs::write(root.join("two/same.txt"), b"b").expect("a file");

        let opened = SourceTree::open(
            &[root.join("one/same.txt"), root.join("two/same.txt")],
            policy(),
        );

        assert!(matches!(opened, Err(FileError::Duplicate)));

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn a_reparse_point_never_leaves_this_side() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("real")).expect("a directory");
        fs::write(root.join("real/a.txt"), b"abc").expect("a file");

        // Making one needs a privilege this environment may not have, and a
        // test that quietly passes for that reason would be worse than one
        // that says so.
        let Ok(()) = std::os::windows::fs::symlink_dir(root.join("real"), root.join("link")) else {
            eprintln!("skipped: this environment cannot create a reparse point");
            fs::remove_dir_all(&root).expect("the tree");
            return;
        };

        let opened = SourceTree::open(&[root.join("link")], policy());

        assert!(matches!(opened, Err(FileError::Unsupported)));

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn a_staged_tree_is_only_the_top_level_paths_it_committed() {
        let base = temporary_directory();
        fs::create_dir_all(&base).expect("a base directory");

        let mut staging = Staging::create_under(&base, 1).expect("a staging root");
        stage(&mut staging, "tree", EntryKind::Directory, 0);
        stage(&mut staging, "tree/a.txt", EntryKind::File, 3);
        staging.write_chunk(b"abc").expect("a chunk");
        stage(&mut staging, "loose.txt", EntryKind::File, 0);

        let committed = staging.commit().expect("a whole tree");

        assert_eq!(
            committed,
            vec![
                base.join("1").join("tree"),
                base.join("1").join("loose.txt")
            ]
        );
        assert_eq!(
            fs::read(base.join("1").join("tree").join("a.txt")).expect("a staged file"),
            b"abc"
        );

        fs::remove_dir_all(&base).expect("the tree");
    }

    #[test]
    fn an_abandoned_tree_is_removed_rather_than_left_half_written() {
        let base = temporary_directory();
        fs::create_dir_all(&base).expect("a base directory");

        let mut staging = Staging::create_under(&base, 1).expect("a staging root");
        stage(&mut staging, "tree", EntryKind::Directory, 0);
        stage(&mut staging, "tree/a.txt", EntryKind::File, 3);
        staging.write_chunk(b"ab").expect("a chunk");
        staging.abort();

        assert!(!base.join("1").exists());

        fs::remove_dir_all(&base).expect("the tree");
    }

    #[test]
    fn a_staged_entry_is_created_new_and_never_over_something_that_is_there() {
        let base = temporary_directory();
        fs::create_dir_all(&base).expect("a base directory");

        let mut staging = Staging::create_under(&base, 1).expect("a staging root");
        stage(&mut staging, "a.txt", EntryKind::File, 1);

        assert!(matches!(
            staging.create_entry(&validated("a.txt"), EntryKind::File, 1),
            Err(FileError::Exists)
        ));
        assert!(matches!(
            staging.create_entry(&validated("missing/a.txt"), EntryKind::File, 1),
            Err(FileError::Io(_))
        ));

        staging.abort();
        fs::remove_dir_all(&base).expect("the tree");
    }

    #[test]
    fn cleanup_keeps_a_committed_tree_until_its_retention_runs_out() {
        let root = temporary_directory();
        let base = root.join("session");
        fs::create_dir_all(&base).expect("a session directory");

        let mut staging = Staging::create_under(&base, 1).expect("a staging root");
        stage(&mut staging, "a.txt", EntryKind::File, 0);
        staging.commit().expect("a whole tree");

        cleanup(&root, SystemTime::now(), Duration::from_secs(3600));
        assert!(base.join("1").exists());

        // A day later, by asking as if it were.
        cleanup(
            &root,
            SystemTime::now() + Duration::from_secs(7200),
            Duration::from_secs(3600),
        );
        assert!(!base.join("1").exists());
        assert!(!base.join("1.committed").exists());

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn cleanup_removes_a_tree_that_was_never_committed() {
        let root = temporary_directory();
        let base = root.join("session");
        fs::create_dir_all(base.join("2")).expect("a stale staging root");
        fs::write(base.join("2/half.bin"), b"ab").expect("a half-written file");

        cleanup(&root, SystemTime::now(), Duration::from_secs(3600));

        assert!(!base.join("2").exists());

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn a_transfer_whose_name_is_taken_is_staged_beside_what_is_there() {
        let base = temporary_directory();
        fs::create_dir_all(&base).expect("a base directory");

        // What a reconnect looks like: the ids start again at one, and the
        // tree the last connection committed is still there.
        let first = Staging::create_under(&base, 1).expect("a staging root");
        first.commit().expect("a whole tree");
        let second = Staging::create_under(&base, 1).expect("a second staging root");
        let mut third = Staging::create_under(&base, 1).expect("a third staging root");
        stage(&mut third, "a.txt", EntryKind::File, 0);

        assert!(base.join("1").is_dir());
        assert!(base.join("1-1").is_dir());
        assert_eq!(
            third.commit().expect("a whole tree"),
            vec![base.join("1-2").join("a.txt")]
        );

        second.abort();
        assert!(!base.join("1-1").exists(), "an aborted tree stayed behind");

        fs::remove_dir_all(&base).expect("the tree");
    }
}
