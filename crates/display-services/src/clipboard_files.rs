//! The guest's side of a file transfer: what a selection names, what a tree
//! holds, and where an arriving tree is put.
//!
//! Everything here is descriptor-relative and follows no link. A guest walks
//! paths its own user chose, but a host that asks for them is on the other
//! side of a socket, and a tree that arrives is entirely the peer's: neither
//! may be turned into a write outside the staging root by a symlink, a `..`,
//! or a name that changed between the check and the open.

use std::{
    collections::HashSet,
    ffi::{CStr, CString, OsStr, OsString},
    fmt,
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
};

use vmlord_display_protocol::clipboard::{
    CHUNK,
    files::{EntryKind, MAX_ENTRIES, Policy},
    path::{PathError, ValidatedPath},
};

/// What a `text/uri-list` payload is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UriError {
    /// Not UTF-8, which every uri-list is.
    NotUtf8,
    /// A scheme that is not `file`.
    NotFile,
    /// A `file://` URI belonging to another machine.
    RemoteAuthority,
    /// A `%` that is not followed by two hex digits.
    Escape,
    /// A path with a NUL in it.
    Nul,
    /// A `file:` URI with no absolute path.
    NotAbsolute,
    /// A payload that named no path at all.
    Empty,
}

impl fmt::Display for UriError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 => write!(f, "the uri list is not UTF-8"),
            Self::NotFile => write!(f, "the uri is not a file uri"),
            Self::RemoteAuthority => write!(f, "the uri belongs to another host"),
            Self::Escape => write!(f, "the uri has a malformed escape"),
            Self::Nul => write!(f, "the path has a NUL"),
            Self::NotAbsolute => write!(f, "the uri names no absolute path"),
            Self::Empty => write!(f, "the uri list names nothing"),
        }
    }
}

impl std::error::Error for UriError {}

/// Why a tree could not be read or could not be written.
#[derive(Debug)]
pub enum FileError {
    /// The filesystem refused something.
    Io(io::Error),
    /// A symlink, socket, FIFO, device or anything else that is not a regular
    /// file or a directory.
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
    /// A name that is not UTF-8, which the wire has no way to carry.
    NotUtf8,
    /// No private per-user runtime directory to stage into.
    NoRuntimeDir,
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "the filesystem refused it: {error}"),
            Self::Unsupported => write!(f, "only regular files and directories may be copied"),
            Self::TooLarge => write!(f, "the transfer is over its configured limit"),
            Self::TooMany => write!(f, "the tree has more than {MAX_ENTRIES} entries"),
            Self::Duplicate => write!(f, "two entries would be the same file"),
            Self::Exists => write!(f, "the destination is already there"),
            Self::Changed => write!(f, "the file changed while it was being read"),
            Self::Path(error) => write!(f, "{error}"),
            Self::NotUtf8 => write!(f, "the name is not UTF-8"),
            Self::NoRuntimeDir => write!(f, "there is no XDG_RUNTIME_DIR to stage into"),
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

/// The two payloads a file selection is offered as in GNOME.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UriPayloads {
    /// `text/uri-list`, whose lines end in CRLF.
    pub uri_list: Vec<u8>,
    /// `x-special/gnome-copied-files`, whose first line is the operation.
    pub gnome_copied: Vec<u8>,
}

/// The paths a `text/uri-list` or GNOME copied-files payload names.
///
/// # Errors
///
/// [`UriError`] for anything that is not a local `file://` path, which is the
/// only thing this side will go on to open.
pub fn parse_uri_list(bytes: &[u8]) -> Result<Vec<PathBuf>, UriError> {
    let text = std::str::from_utf8(bytes).map_err(|_| UriError::NotUtf8)?;

    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        // A comment, a blank, or the operation GNOME puts on the first line.
        if line.is_empty() || line.starts_with('#') || line == "copy" || line == "cut" {
            continue;
        }

        paths.push(parse_uri(line)?);
    }

    if paths.is_empty() {
        return Err(UriError::Empty);
    }

    Ok(paths)
}

/// What one URI names.
fn parse_uri(uri: &str) -> Result<PathBuf, UriError> {
    let Some(rest) = uri.strip_prefix("file://") else {
        return Err(if uri.starts_with("file:") {
            UriError::NotAbsolute
        } else {
            UriError::NotFile
        });
    };

    let Some(at) = rest.find('/') else {
        return Err(UriError::NotAbsolute);
    };
    let (authority, path) = rest.split_at(at);
    if !(authority.is_empty() || authority.eq_ignore_ascii_case("localhost")) {
        return Err(UriError::RemoteAuthority);
    }

    let decoded = decode(path)?;
    if decoded.contains(&0) {
        return Err(UriError::Nul);
    }

    Ok(PathBuf::from(OsStr::from_bytes(&decoded)))
}

/// Undoes the percent-encoding of one URI path.
fn decode(path: &str) -> Result<Vec<u8>, UriError> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());

    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'%' {
            decoded.push(bytes[at]);
            at += 1;
            continue;
        }

        let digits = bytes.get(at + 1..at + 3).ok_or(UriError::Escape)?;
        let text = std::str::from_utf8(digits).map_err(|_| UriError::Escape)?;
        decoded.push(u8::from_str_radix(text, 16).map_err(|_| UriError::Escape)?);
        at += 3;
    }

    Ok(decoded)
}

/// The payloads a selection of these paths is offered as.
#[must_use]
pub fn uri_lists(paths: &[PathBuf]) -> UriPayloads {
    let uris: Vec<String> = paths.iter().map(|path| encode(path)).collect();

    UriPayloads {
        uri_list: uris
            .iter()
            .flat_map(|uri| format!("{uri}\r\n").into_bytes())
            .collect(),
        gnome_copied: format!("copy\n{}", uris.join("\n")).into_bytes(),
    }
}

/// One path as a `file://` URI.
fn encode(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(char::from(*byte));
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }

    uri
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

/// One directory of a walk, and where in it the walk is.
struct Frame {
    directory: OwnedFd,
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
///
/// Every descent is an `openat` from the directory above it with
/// `O_NOFOLLOW`, so a component that becomes a symlink between the walk and
/// the read is refused rather than followed out of the tree.
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
    /// [`FileError`] if a top-level path has no name, a name the wire cannot
    /// carry, or a name another top-level path already has.
    pub fn open(paths: &[PathBuf], policy: Policy) -> Result<Self, FileError> {
        let mut keys = HashSet::new();
        for path in paths {
            let name = path
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or(FileError::NotUtf8)?;
            let parsed = ValidatedPath::parse(name)?;
            if !keys.insert(parsed.windows_key().to_owned()) {
                return Err(FileError::Duplicate);
            }
        }

        let mut tree = Self {
            policy,
            roots: paths.to_vec(),
            root_at: 0,
            stack: Vec::new(),
            reading: None,
            entries: 0,
            total: 0,
            keys,
        };
        // Refuse the whole selection now rather than half-way through it: a
        // top-level FIFO is a mistake the user can still see the result of.
        for path in paths {
            let opened = open_no_follow(None, path.as_os_str())?;
            kind_of(&opened)?;
        }
        tree.keys.clear();

        Ok(tree)
    }

    /// The next entry, or the next chunk of the entry that is open.
    ///
    /// # Errors
    ///
    /// [`FileError`] for anything that ends the transfer: a special file, a
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

            let text = name.to_str().ok_or(FileError::NotUtf8)?;
            let path = format!("{}/{text}", frame.prefix);
            let directory = frame.directory.as_fd_borrowed();
            let opened = open_no_follow(Some(directory), &name)?;

            return self.admit(path, opened).map(Some);
        }

        let Some(root) = self.roots.get(self.root_at).cloned() else {
            return Ok(None);
        };
        self.root_at += 1;

        let name = root
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or(FileError::NotUtf8)?
            .to_owned();
        let opened = open_no_follow(None, root.as_os_str())?;

        self.admit(name, opened).map(Some)
    }

    /// Accounts for one opened entry and, if it is a directory, descends.
    fn admit(&mut self, path: String, opened: OwnedFd) -> Result<Produced, FileError> {
        let parsed = ValidatedPath::parse(&path)?;
        let kind = kind_of(&opened)?;
        let size = match kind {
            EntryKind::Directory => 0,
            EntryKind::File => size_of(&opened)?,
        };

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
                let names = names_in(&opened)?;
                self.stack.push(Frame {
                    directory: opened,
                    prefix: path.clone(),
                    names,
                    at: 0,
                });
            }
            EntryKind::File if size > 0 => {
                self.reading = Some(Reading {
                    file: File::from(opened),
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
                // Shorter than it said it was, so the entry after it would be
                // parsed against a stream that is no longer where it should be.
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
///
/// The root is created fresh and private, every component is opened from the
/// one above it without following links, and every file is created new. A tree
/// that is not committed is removed, including by [`Drop`], so a transfer that
/// ends anywhere leaves nothing behind.
pub struct Staging {
    root: PathBuf,
    directory: OwnedFd,
    open: Option<File>,
    top_level: Vec<String>,
    committed: bool,
}

impl Staging {
    /// A staging root for one transfer, under this user's runtime directory.
    ///
    /// # Errors
    ///
    /// [`FileError::NoRuntimeDir`] without `XDG_RUNTIME_DIR` -- there is no
    /// fallback to a shared `/tmp`, where another user could be waiting -- and
    /// [`FileError::Io`] if the directories cannot be made.
    pub fn create(session: &str, transfer: u32) -> Result<Self, FileError> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or(FileError::NoRuntimeDir)?;
        if runtime.is_empty() {
            return Err(FileError::NoRuntimeDir);
        }

        let mut base = PathBuf::from(runtime);
        for component in ["vmlord", "clipboard", session] {
            base.push(component);
            match make_private(&base) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                other => other?,
            }
        }

        Self::create_under(&base, transfer)
    }

    /// A staging root under a directory the caller chose.
    ///
    /// # Errors
    ///
    /// [`FileError::Io`] if the root cannot be created fresh.
    pub fn create_under(base: &Path, transfer: u32) -> Result<Self, FileError> {
        let root = base.join(transfer.to_string());
        make_private(&root)?;
        let directory = open_directory(None, root.as_os_str())?;

        Ok(Self {
            root,
            directory,
            open: None,
            top_level: Vec::new(),
            committed: false,
        })
    }

    /// Creates one entry of the arriving tree.
    ///
    /// # Errors
    ///
    /// [`FileError::Exists`] if anything is already at the destination --
    /// including a link put there while this transfer was running -- and
    /// [`FileError::Io`] if a component is missing or refuses to be opened.
    pub fn create_entry(
        &mut self,
        path: &ValidatedPath,
        kind: EntryKind,
        size: u64,
    ) -> Result<(), FileError> {
        self.open = None;

        let mut components: Vec<&str> = path.components().collect();
        let name = components.pop().expect("a validated path has a component");

        let mut directory = self.directory.try_clone()?;
        for component in &components {
            directory = open_directory(Some(directory.as_fd_borrowed()), OsStr::new(component))?;
        }

        match kind {
            EntryKind::Directory => make_private_at(&directory, name)?,
            EntryKind::File => {
                let file = create_new_at(&directory, name)?;
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

    /// Keeps the tree and answers with its top-level paths.
    ///
    /// # Errors
    ///
    /// [`FileError::Io`] if the last file cannot be flushed.
    pub fn commit(mut self) -> Result<Vec<PathBuf>, FileError> {
        if let Some(mut file) = self.open.take() {
            file.flush()?;
        }
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

/// Removes a tree without ever following a link out of it.
fn remove_tree(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };

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

/// A borrow of a descriptor, as the calls below want it.
trait AsFdBorrowed {
    fn as_fd_borrowed(&self) -> BorrowedFd<'_>;
}

impl AsFdBorrowed for OwnedFd {
    fn as_fd_borrowed(&self) -> BorrowedFd<'_> {
        // SAFETY: the descriptor is owned by `self` and outlives the borrow.
        unsafe { BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

/// The raw descriptor an `*at` call starts from.
fn start(directory: Option<BorrowedFd<'_>>) -> libc::c_int {
    directory.map_or(libc::AT_FDCWD, |fd| fd.as_raw_fd())
}

/// Opens a name without following it, whatever it turns out to be.
fn open_no_follow(directory: Option<BorrowedFd<'_>>, name: &OsStr) -> Result<OwnedFd, FileError> {
    let name = CString::new(name.as_bytes()).map_err(|_| FileError::Unsupported)?;
    // `O_NONBLOCK` so that a FIFO answers instead of waiting for a writer that
    // may never come; what it is gets decided from the descriptor afterwards.
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;

    // SAFETY: `name` is NUL-terminated and outlives the call, and the returned
    // descriptor is owned from here on.
    let opened = unsafe { libc::openat(start(directory), name.as_ptr(), flags) };
    if opened < 0 {
        let error = io::Error::last_os_error();

        // What a symlink looks like through `O_NOFOLLOW`.
        return Err(if error.raw_os_error() == Some(libc::ELOOP) {
            FileError::Unsupported
        } else {
            FileError::Io(error)
        });
    }

    // SAFETY: `openat` answered with a descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(opened) })
}

/// Opens a directory, and only a directory, without following it.
fn open_directory(directory: Option<BorrowedFd<'_>>, name: &OsStr) -> Result<OwnedFd, FileError> {
    let opened = open_no_follow(directory, name)?;
    if kind_of(&opened)? != EntryKind::Directory {
        return Err(FileError::Unsupported);
    }

    Ok(opened)
}

/// What an opened descriptor turns out to be.
fn kind_of(opened: &OwnedFd) -> Result<EntryKind, FileError> {
    let mode = stat(opened)?.st_mode & libc::S_IFMT;

    match mode {
        libc::S_IFDIR => Ok(EntryKind::Directory),
        libc::S_IFREG => Ok(EntryKind::File),
        _ => Err(FileError::Unsupported),
    }
}

/// How long an opened regular file is.
fn size_of(opened: &OwnedFd) -> Result<u64, FileError> {
    Ok(u64::try_from(stat(opened)?.st_size).unwrap_or(0))
}

/// `fstat` on an owned descriptor.
fn stat(opened: &OwnedFd) -> Result<libc::stat, FileError> {
    // SAFETY: a `stat` of zeroes is a valid one to write into, and the
    // descriptor is open for the duration of the call.
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    let taken = unsafe { libc::fstat(opened.as_raw_fd(), &raw mut status) };
    if taken < 0 {
        return Err(FileError::Io(io::Error::last_os_error()));
    }

    Ok(status)
}

/// The names in an opened directory, in a fixed order.
fn names_in(directory: &OwnedFd) -> Result<Vec<OsString>, FileError> {
    // `closedir` closes the descriptor it was given, and this one is still the
    // walk's, so the stream gets a copy of it.
    let duplicate = directory.try_clone()?;

    // SAFETY: the descriptor is a directory this side opened, and `fdopendir`
    // takes it over, so it is released rather than dropped.
    let stream = unsafe { libc::fdopendir(duplicate.as_raw_fd()) };
    if stream.is_null() {
        return Err(FileError::Io(io::Error::last_os_error()));
    }
    std::mem::forget(duplicate);

    let mut names = Vec::new();
    let outcome = loop {
        // SAFETY: nothing else writes this thread's errno between here and the
        // check below, which is how the end of a directory is told from a fault.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: `stream` is open until `closedir` below, and the entry it
        // answers with is read before the next call to `readdir`.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            break if error.raw_os_error() == Some(0) {
                Ok(())
            } else {
                Err(FileError::Io(error))
            };
        }

        // SAFETY: `d_name` is a NUL-terminated name inside the entry above.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        names.push(OsStr::from_bytes(name.to_bytes()).to_owned());
    };

    // SAFETY: the stream is open and is not used again.
    unsafe { libc::closedir(stream) };
    outcome?;

    names.sort();

    Ok(names)
}

/// Creates a directory only this user may enter.
fn make_private(path: &Path) -> io::Result<()> {
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;

    // SAFETY: `name` is NUL-terminated and outlives the call.
    let made = unsafe { libc::mkdir(name.as_ptr(), 0o700) };
    if made < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// Creates a directory under an opened one, and never over something else.
fn make_private_at(directory: &OwnedFd, name: &str) -> Result<(), FileError> {
    let name = CString::new(name).map_err(|_| FileError::Unsupported)?;

    // SAFETY: `name` is NUL-terminated and outlives the call.
    let made = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
    if made < 0 {
        let error = io::Error::last_os_error();

        return Err(if error.kind() == io::ErrorKind::AlreadyExists {
            FileError::Exists
        } else {
            FileError::Io(error)
        });
    }

    Ok(())
}

/// Creates a file under an opened directory, and never over something else.
fn create_new_at(directory: &OwnedFd, name: &str) -> Result<File, FileError> {
    let name = CString::new(name).map_err(|_| FileError::Unsupported)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    // SAFETY: `name` is NUL-terminated and outlives the call, and the returned
    // descriptor is owned from here on.
    let created = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if created < 0 {
        let error = io::Error::last_os_error();

        // `O_EXCL` answers this way for a dangling symlink as well, which is
        // exactly the case this refuses to write through.
        return Err(if error.kind() == io::ErrorKind::AlreadyExists {
            FileError::Exists
        } else {
            FileError::Io(error)
        });
    }

    // SAFETY: `openat` answered with a descriptor nothing else owns.
    Ok(unsafe { File::from_raw_fd(created) })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        time::{SystemTime, UNIX_EPOCH},
    };

    use vmlord_display_protocol::clipboard::path::MAX_DEPTH;

    use super::*;

    fn policy() -> Policy {
        Policy::new(1024, 4096, 3600)
    }

    fn temporary_directory() -> PathBuf {
        let unique_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after the epoch")
            .as_nanos();

        std::env::temp_dir().join(format!("vmlord-clipboard-files-test-{unique_id}"))
    }

    /// Everything a source tree produces, in order.
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

    #[test]
    fn a_local_uri_list_is_the_paths_it_names() {
        let list = b"file:///home/u/a%20b\r\nfile:///tmp/\xd0\xb6.txt\r\n";

        assert_eq!(
            parse_uri_list(list).expect("a local uri list"),
            vec![PathBuf::from("/home/u/a b"), PathBuf::from("/tmp/ж.txt")]
        );
    }

    #[test]
    fn a_gnome_payload_keeps_its_header_and_comments_out_of_the_paths() {
        let list = b"copy\nfile:///tmp/a\n# a comment\n\nfile:///tmp/b\n";

        assert_eq!(
            parse_uri_list(list).expect("a gnome copied-files payload"),
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
    }

    #[test]
    fn a_uri_that_is_not_a_local_file_is_refused() {
        let cases: [(&[u8], UriError); 7] = [
            (b"http://example/x", UriError::NotFile),
            (b"file://remote/share/x", UriError::RemoteAuthority),
            (b"file:///tmp/a%zz", UriError::Escape),
            (b"file:///tmp/a%2", UriError::Escape),
            (b"file:///tmp/a%00b", UriError::Nul),
            (b"file:relative", UriError::NotAbsolute),
            (b"copy\n", UriError::Empty),
        ];

        for (list, expected) in cases {
            assert_eq!(
                parse_uri_list(list),
                Err(expected),
                "{} was not refused",
                String::from_utf8_lossy(list)
            );
        }
    }

    #[test]
    fn the_payloads_a_selection_offers_name_only_its_top_level_paths() {
        let payloads = uri_lists(&[PathBuf::from("/tmp/a b"), PathBuf::from("/tmp/ж")]);

        assert_eq!(
            payloads.uri_list,
            b"file:///tmp/a%20b\r\nfile:///tmp/%D0%B6\r\n".to_vec()
        );
        assert_eq!(
            payloads.gnome_copied,
            b"copy\nfile:///tmp/a%20b\nfile:///tmp/%D0%B6".to_vec()
        );
        assert_eq!(
            parse_uri_list(&payloads.uri_list).expect("what this side wrote"),
            vec![PathBuf::from("/tmp/a b"), PathBuf::from("/tmp/ж")]
        );
    }

    #[test]
    fn a_tree_is_walked_depth_first_with_every_file_after_its_directory() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("tree/inner")).expect("a tree");
        fs::write(root.join("tree/a.txt"), b"abc").expect("a file");
        fs::write(root.join("tree/inner/b.bin"), b"xy").expect("a file");
        fs::write(root.join("tree/empty"), b"").expect("a file");

        let mut source = SourceTree::open(&[root.join("tree")], policy()).expect("a tree");
        let produced = drained(&mut source).expect("a walk");

        assert_eq!(
            produced,
            vec![
                entry("tree", EntryKind::Directory, 0),
                entry("tree/a.txt", EntryKind::File, 3),
                Produced::Chunk(b"abc".to_vec()),
                entry("tree/empty", EntryKind::File, 0),
                entry("tree/inner", EntryKind::Directory, 0),
                entry("tree/inner/b.bin", EntryKind::File, 2),
                Produced::Chunk(b"xy".to_vec()),
            ]
        );

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn nothing_but_a_file_or_a_directory_leaves_this_side() {
        let root = temporary_directory();
        fs::create_dir_all(&root).expect("a directory");
        fs::write(root.join("target"), b"abc").expect("a file");
        symlink(root.join("target"), root.join("link")).expect("a symlink");
        make_fifo(&root.join("pipe"));

        for name in ["link", "pipe"] {
            let opened = SourceTree::open(&[root.join(name)], policy());

            assert!(
                matches!(opened, Err(FileError::Unsupported)),
                "{name} was not refused"
            );
        }

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn a_link_inside_a_tree_ends_the_walk_rather_than_being_followed() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("tree")).expect("a tree");
        fs::write(root.join("secret"), b"abc").expect("a file");
        symlink(root.join("secret"), root.join("tree/link")).expect("a symlink");

        let mut source = SourceTree::open(&[root.join("tree")], policy()).expect("a tree");

        assert!(matches!(drained(&mut source), Err(FileError::Unsupported)));

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
    fn a_tree_over_the_transfer_limit_ends_the_walk() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("tree")).expect("a tree");
        for index in 0..5 {
            fs::write(root.join(format!("tree/f{index}.bin")), vec![0u8; 1024]).expect("a file");
        }

        let mut source = SourceTree::open(&[root.join("tree")], policy()).expect("a tree");

        assert!(matches!(drained(&mut source), Err(FileError::TooLarge)));

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn a_tree_deeper_than_the_protocol_allows_ends_the_walk() {
        let root = temporary_directory();
        let deep = root.join("tree").join(vec!["d"; MAX_DEPTH].join("/"));
        fs::create_dir_all(&deep).expect("a deep tree");

        let mut source = SourceTree::open(&[root.join("tree")], policy()).expect("a tree");

        assert!(matches!(drained(&mut source), Err(FileError::Path(_))));

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn two_top_level_paths_with_one_name_are_refused() {
        let root = temporary_directory();
        fs::create_dir_all(root.join("one")).expect("a directory");
        fs::create_dir_all(root.join("two")).expect("a directory");
        fs::write(root.join("one/same.txt"), b"a").expect("a file");
        fs::write(root.join("two/SAME.TXT"), b"b").expect("a file");

        let opened = SourceTree::open(
            &[root.join("one/same.txt"), root.join("two/SAME.TXT")],
            policy(),
        );

        assert!(matches!(opened, Err(FileError::Duplicate)));

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn a_staged_tree_is_only_the_top_level_paths_it_committed() {
        let root = temporary_directory();
        fs::create_dir_all(&root).expect("a runtime directory");

        let mut staging = Staging::create_under(&root, 1).expect("a staging root");
        stage(&mut staging, "tree", EntryKind::Directory, 0);
        stage(&mut staging, "tree/a.txt", EntryKind::File, 3);
        staging.write_chunk(b"abc").expect("a chunk");
        stage(&mut staging, "loose.txt", EntryKind::File, 0);

        let committed = staging.commit().expect("a whole tree");

        assert_eq!(
            committed,
            vec![root.join("1/tree"), root.join("1/loose.txt")]
        );
        assert_eq!(
            fs::read(root.join("1/tree/a.txt")).expect("a staged file"),
            b"abc"
        );

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn a_staging_root_is_private_to_this_user() {
        let root = temporary_directory();
        fs::create_dir_all(&root).expect("a runtime directory");

        let staging = Staging::create_under(&root, 1).expect("a staging root");
        let mode = fs::metadata(root.join("1"))
            .expect("a staging root")
            .permissions()
            .mode();

        assert_eq!(mode & 0o777, 0o700);

        staging.abort();
        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn an_abandoned_tree_is_removed_rather_than_left_half_written() {
        let root = temporary_directory();
        fs::create_dir_all(&root).expect("a runtime directory");

        let mut staging = Staging::create_under(&root, 1).expect("a staging root");
        stage(&mut staging, "tree", EntryKind::Directory, 0);
        stage(&mut staging, "tree/a.txt", EntryKind::File, 3);
        staging.write_chunk(b"ab").expect("a chunk");
        staging.abort();

        assert!(!root.join("1").exists());

        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn a_staged_entry_is_created_new_and_never_over_something_that_is_there() {
        let root = temporary_directory();
        fs::create_dir_all(&root).expect("a runtime directory");

        let mut staging = Staging::create_under(&root, 1).expect("a staging root");
        stage(&mut staging, "a.txt", EntryKind::File, 1);

        let again = staging.create_entry(&validated("a.txt"), EntryKind::File, 1);
        assert!(matches!(again, Err(FileError::Exists)));

        let orphan = staging.create_entry(&validated("missing/a.txt"), EntryKind::File, 1);
        assert!(matches!(orphan, Err(FileError::Io(_))));

        staging.abort();
        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn a_staged_entry_is_never_written_through_a_link_that_was_put_there() {
        let root = temporary_directory();
        fs::create_dir_all(&root).expect("a runtime directory");

        let mut staging = Staging::create_under(&root, 1).expect("a staging root");
        stage(&mut staging, "tree", EntryKind::Directory, 0);
        // A link where the next entry is about to be created, as a racing
        // process in the guest's own session could put there.
        symlink(root.join("elsewhere"), root.join("1/tree/a.txt")).expect("a symlink");

        let through = staging.create_entry(&validated("tree/a.txt"), EntryKind::File, 1);

        assert!(matches!(through, Err(FileError::Exists)));
        assert!(!root.join("elsewhere").exists());

        staging.abort();
        fs::remove_dir_all(&root).expect("the tree");
    }

    #[test]
    fn a_staging_root_needs_a_private_runtime_directory() {
        // Whatever this environment has, the refusal names the missing
        // directory rather than falling back to a shared one.
        let missing = temporary_directory().join("nothing/here");

        assert!(matches!(
            Staging::create_under(&missing, 1),
            Err(FileError::Io(_))
        ));
    }

    /// Creates one entry, or fails the test with what went wrong.
    fn stage(staging: &mut Staging, path: &str, kind: EntryKind, size: u64) {
        staging
            .create_entry(&validated(path), kind, size)
            .unwrap_or_else(|error| panic!("{path} could not be staged: {error}"));
    }

    fn validated(path: &str) -> ValidatedPath {
        ValidatedPath::parse(path).expect("a path the protocol allows")
    }

    /// A FIFO, which is one of the things that may not cross.
    fn make_fifo(path: &Path) {
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path");
        // SAFETY: `name` is a NUL-terminated path that outlives the call.
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };

        assert_eq!(made, 0, "a fifo could not be created");
    }
}
