//! The guest's cursor hotspots, read out of the active Xcursor theme.
//!
//! The capture process sees cursor bitmaps with their hotspots already
//! subtracted -- mutter's cursor plane carries `CRTC_X`/`CRTC_Y` of the
//! bitmap's corner and no property to read the hotspot back from -- so the
//! host has had to measure them (the viewer's `cursor.rs`). But the hotspots
//! are not secret: they sit in the Xcursor theme on the guest's disk. What
//! cannot read them is the capture process, a system unit with
//! `ProtectHome=yes`; the tray is a user unit in the graphical session and
//! can. So the tray parses the theme once, ships the resulting
//! bitmap-to-hotspot table to the host by way of the broker and the session
//! process, and the host matches incoming cursor bitmaps against it.
//!
//! ## The Xcursor file format
//!
//! Stated from libXcursor's `file.c`, and the reason the pixel bytes need no
//! conversion at all: every integer is u32 little-endian; a file is a 16-byte
//! header (magic `Xcur`, header length, version, number of TOC entries)
//! followed by 12-byte TOC entries of `{type, subtype, position}`; an image
//! chunk has type `0xfffd0002` and subtype `nominal size`, and after its
//! 16-byte chunk header carries `width, height, xhot, yhot, delay` and
//! `width * height` u32 pixels of straight (not premultiplied) alpha.
//! On little-endian those pixels are byte for byte B, G, R, A -- the same
//! memory layout as `DRM_FORMAT_ARGB8888` and the same layout the capture
//! reads off the plane and the cursor-image record carries. An animated
//! cursor is several image chunks in one file, each frame with its own
//! hotspot and delay, so every chunk becomes its own table entry.
//!
//! ## The dconf read
//!
//! The active theme is found from `XCURSOR_THEME` first, else from dconf.
//! The dconf D-Bus service (`ca.desrt.dconf.Writer`) has no read method, so
//! the database file `~/.config/dconf/user` is read directly: a GVDB file.
//! The `gvdb` crate was considered for this and turned down -- reading one
//! string key out of a database a few hundred kilobytes across would have
//! pulled `serde`, `zerocopy` and `zvariant` into a statically linked musl
//! binary, against the no-new-C rule's spirit and the payload's size. The
//! reader below is the slice of the format that lookup needs (GLib's
//! `gvdb-reader.c` and `gvariant-serialiser.c`, read for this task): a
//! 24-byte header whose signature spells `GVariant`, a root pointer, and a
//! hash table of 24-byte items whose value is a GVariant -- serialised as
//! the child's bytes, a NUL, then the child's type string.
//!
//! A dconf database that names no theme answers "default", which is what the
//! desktop itself would fall back to. A database that cannot be read at all
//! answers nothing, and no table is sent: guessing a theme is what the
//! refusal exists to avoid.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The largest cursor edge this table may carry, in pixels.
///
/// The codec's own cap: a bigger bitmap could not be anchored as a Windows
/// cursor image anyway, so an entry past it is dropped rather than sent.
pub const MAX_DIMENSION: u32 = 256;

/// What one chunk of the table is held under, in bytes.
///
/// A chunk becomes one datagram on the sockets between the tray and the
/// session process, and one record on the frame channel; both move many
/// times this much, and the bound keeps the table's traffic small rather
/// than solves a hard limit. One entry wider than this alone -- a cursor of
/// 180 pixels and up -- is still sent, as its own chunk.
pub const CHUNK_BUDGET: usize = 128 * 1024;

/// What the whole table is held under, in bytes.
///
/// A theme's bitmaps at every nominal size can add up to more than a frame
/// channel wants to carry on a reconnect; when the table is over this, the
/// largest entries are dropped first, because a large cursor is the one a
/// host is least likely to be sent mid-motion and the small arrow is the one
/// worth anchoring exactly.
pub const TABLE_BUDGET: usize = 4 * 1024 * 1024;

/// The theme a desktop means when it names nothing.
pub const DEFAULT_THEME: &str = "default";

/// How many themes one walk may visit.
///
/// libXcursor's own bound. What it bounds is a theme file's `Inherits`
/// listing a cycle, which would otherwise walk forever.
const MAX_THEME_DEPTH: usize = 32;

/// The dconf key the desktop's cursor theme is set under.
const CURSOR_THEME_KEY: &str = "/org/gnome/desktop/interface/cursor-theme";

/// One bitmap of the theme and where within it the pointer points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HotspotEntry {
    /// Straight alpha, one byte per channel in the order B, G, R, A: the
    /// layout both the cursor plane and a cursor image record carry.
    /// Tightly packed rows, `width * height * 4` long.
    pub pixels: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Where the pointer points, from the bitmap's left edge.
    pub hotspot_x: u32,
    /// Where the pointer points, from its top edge.
    pub hotspot_y: u32,
}

impl HotspotEntry {
    /// What the entry costs on the wire, over-estimated so that a chunk
    /// sized by it stays under its bound however protobuf pads it.
    #[must_use]
    pub fn wire_size(&self) -> usize {
        self.pixels.len() + 32
    }

    /// Whether the entry is one the codec could anchor as a cursor.
    ///
    /// The format's own sanity limits are wider -- libXcursor reads bitmaps
    /// to `0x7fff` and allows a hotspot on an edge -- but an entry the
    /// receiver would drop is not worth the bytes.
    #[must_use]
    pub fn is_sendable(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= MAX_DIMENSION
            && self.height <= MAX_DIMENSION
            && self.hotspot_x < self.width
            && self.hotspot_y < self.height
            && self.pixels.len() == self.width as usize * self.height as usize * 4
    }
}

/// What one theme walk produced, counted so that a caller can say what it
/// did without this module logging.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Collection {
    /// The table to send, deduplicated and within [`TABLE_BUDGET`].
    pub entries: Vec<HotspotEntry>,
    /// Files in the theme's `cursors/` directories that could not be read
    /// or parsed. Skipped, not fatal: a theme with one broken file in it
    /// still anchors the rest.
    pub unreadable: usize,
    /// How many entries [`TABLE_BUDGET`] forced out, largest first.
    pub dropped: usize,
}

/// The `Xcur` magic, read as one little-endian word.
const XCURSOR_MAGIC: u32 = 0x7275_6358;

/// The chunk type of a cursor bitmap.
const XCURSOR_IMAGE_TYPE: u32 = 0xfffd_0002;

/// The bytes of a 16-byte file header plus one 12-byte TOC entry.
const FILE_HEADER_LEN: usize = 16;
const TOC_ENTRY_LEN: usize = 12;
/// A chunk header: its own length, type, subtype and version.
const CHUNK_HEADER_LEN: usize = 16;
/// The image's five u32 fields after the chunk header.
const IMAGE_FIELDS_LEN: usize = 20;

/// The largest edge the format itself allows.
const FORMAT_MAX_DIMENSION: u32 = 0x7fff;

/// Reads a little-endian u32, or nothing where the file is too short.
fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let four = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([four[0], four[1], four[2], four[3]]))
}

/// Parses every readable cursor image out of an Xcursor file's bytes.
///
/// All nominal sizes and all animation frames come back, in the file's own
/// order. A file that is not an Xcursor file at all, and an image chunk
/// that fails its sanity limits, yield nothing; the rest of a file with one
/// bad chunk still yields the good chunks.
#[must_use]
pub fn images_from_bytes(bytes: &[u8]) -> Vec<HotspotEntry> {
    let mut images = Vec::new();
    let Some(magic) = read_u32(bytes, 0) else {
        return images;
    };
    let Some(header_len) = read_u32(bytes, 4) else {
        return images;
    };
    let Some(version) = read_u32(bytes, 8) else {
        return images;
    };
    let Some(ntoc) = read_u32(bytes, 12) else {
        return images;
    };
    if magic != XCURSOR_MAGIC || header_len < FILE_HEADER_LEN as u32 || version < 1 {
        return images;
    }

    // A TOC entry is twelve bytes, so a file cannot honestly name more of
    // them than that many times its own length; capping the count here is
    // what keeps a crafted `ntoc` of four billion from turning into a very
    // patient loop of failed reads.
    let ntoc = (ntoc as usize).min(bytes.len() / TOC_ENTRY_LEN + 1);
    for entry in 0..ntoc {
        let at = FILE_HEADER_LEN + entry * TOC_ENTRY_LEN;
        let (Some(chunk_type), Some(position)) = (read_u32(bytes, at), read_u32(bytes, at + 8))
        else {
            break;
        };
        if chunk_type != XCURSOR_IMAGE_TYPE {
            continue;
        }
        if let Some(image) = image_at(bytes, position as usize) {
            images.push(image);
        }
    }

    images
}

/// Parses one image chunk at its TOC position.
fn image_at(bytes: &[u8], position: usize) -> Option<HotspotEntry> {
    let chunk_len = read_u32(bytes, position)?;
    let chunk_type = read_u32(bytes, position + 4)?;
    if chunk_len < (CHUNK_HEADER_LEN + IMAGE_FIELDS_LEN) as u32 || chunk_type != XCURSOR_IMAGE_TYPE
    {
        return None;
    }

    let base = position + CHUNK_HEADER_LEN;
    let width = read_u32(bytes, base)?;
    let height = read_u32(bytes, base + 4)?;
    let hotspot_x = read_u32(bytes, base + 8)?;
    let hotspot_y = read_u32(bytes, base + 12)?;
    // The frame delay of an animated cursor. Not carried: the table maps a
    // bitmap to a hotspot, and every frame of one animation carries the
    // bitmap the host will be matched against, whatever pace it changes at.
    let _delay = read_u32(bytes, base + 16)?;

    let pixels_len = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if width == 0
        || height == 0
        || width > FORMAT_MAX_DIMENSION
        || height > FORMAT_MAX_DIMENSION
        || hotspot_x > width
        || hotspot_y > height
    {
        return None;
    }

    let pixels = bytes.get(base + IMAGE_FIELDS_LEN..base + IMAGE_FIELDS_LEN + pixels_len)?;

    Some(HotspotEntry {
        pixels: pixels.to_vec(),
        width,
        height,
        hotspot_x,
        hotspot_y,
    })
}

/// The directories a theme is looked for in, in order.
///
/// `XCURSOR_PATH`, when set, replaces the whole list, in libXcursor's own
/// `:`-separated spelling.
#[must_use]
pub fn search_dirs(xcursor_path: Option<&str>, home: &Path) -> Vec<PathBuf> {
    if let Some(path) = xcursor_path {
        return path
            .split(':')
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from)
            .collect();
    }

    vec![
        home.join(".local/share/icons"),
        home.join(".icons"),
        PathBuf::from("/usr/share/icons"),
        PathBuf::from("/usr/share/pixmaps"),
    ]
}

/// Collects the cursor-hotspot table for a theme, walking its inherits.
///
/// For each theme in the chain -- this one first, then whatever its
/// `index.theme` inherits, to a depth of [`MAX_THEME_DEPTH`] -- the first
/// directory in `dirs` holding that theme is the one read; a theme the
/// chain reaches twice, including one that inherits itself, is read once.
/// When the chain yields nothing at all, the walk is made once more for
/// [`DEFAULT_THEME`], which is the desktop's own last resort. The table is
/// deduplicated -- theme files alias the same bitmap under many names --
/// and held under [`TABLE_BUDGET`].
#[must_use]
pub fn collect_table(theme: &str, dirs: &[PathBuf]) -> Collection {
    let mut entries = Vec::new();
    let mut unreadable = 0;
    walk_theme(
        theme,
        dirs,
        &mut HashSet::new(),
        0,
        &mut entries,
        &mut unreadable,
    );
    if entries.is_empty() && theme != DEFAULT_THEME {
        walk_theme(
            DEFAULT_THEME,
            dirs,
            &mut HashSet::new(),
            0,
            &mut entries,
            &mut unreadable,
        );
    }

    let sendable: Vec<_> = entries
        .into_iter()
        .filter(HotspotEntry::is_sendable)
        .collect();
    let deduplicated = dedupe(sendable);
    let (mut kept, dropped) = trim_to_budget(deduplicated);
    kept.shrink_to_fit();

    Collection {
        entries: kept,
        unreadable,
        dropped,
    }
}

/// Reads one theme's cursors, then its inherits'.
fn walk_theme(
    theme: &str,
    dirs: &[PathBuf],
    visited: &mut HashSet<String>,
    depth: usize,
    entries: &mut Vec<HotspotEntry>,
    unreadable: &mut usize,
) {
    if theme.is_empty() || depth > MAX_THEME_DEPTH || !visited.insert(theme.to_owned()) {
        return;
    }
    let Some(home) = dirs.iter().find(|dir| dir.join(theme).is_dir()) else {
        return;
    };

    // The first cursors directory found for this theme is the theme's: a
    // system theme shadowed by one of the same name in the user's home is
    // the user's, exactly as libXcursor reads it.
    if let Ok(files) = std::fs::read_dir(home.join(theme).join("cursors")) {
        for file in files.flatten() {
            // A directory is not a cursor. Anything else -- including a
            // symlink, which is how themes alias one name to another's
            // bitmap -- is read; what cannot be read is skipped and
            // counted, never fatal.
            if file.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Ok(bytes) = std::fs::read(file.path()) else {
                *unreadable += 1;
                continue;
            };
            let parsed = images_from_bytes(&bytes);
            if parsed.is_empty() {
                *unreadable += 1;
            } else {
                entries.extend(parsed);
            }
        }
    }

    for inherited in inherits(&home.join(theme)) {
        walk_theme(&inherited, dirs, visited, depth + 1, entries, unreadable);
    }
}

/// The themes one theme's `index.theme` inherits, from the first
/// `Inherits=` line, split on the separators libXcursor accepts.
fn inherits(theme_directory: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(theme_directory.join("index.theme")) else {
        return Vec::new();
    };
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("Inherits=") {
            return rest
                .split([',', ';', ':', ' ', '\t'])
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect();
        }
    }

    Vec::new()
}

/// Drops entries identical in bitmap, size and hotspot.
///
/// A theme's cursors directory is full of aliases -- `left_ptr`, `arrow`,
/// `default` are usually the same file -- and every alias parsed is the same
/// entry. The first occurrence is kept, so the table's order follows the
/// walk's.
fn dedupe(entries: Vec<HotspotEntry>) -> Vec<HotspotEntry> {
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    for entry in entries {
        let shape = (
            entry.width,
            entry.height,
            entry.hotspot_x,
            entry.hotspot_y,
            entry.pixels.clone(),
        );
        if seen.insert(shape) {
            kept.push(entry);
        }
    }

    kept
}

/// Holds the table under [`TABLE_BUDGET`], keeping the smaller entries.
///
/// Returns what is kept, and how many entries were dropped.
fn trim_to_budget(entries: Vec<HotspotEntry>) -> (Vec<HotspotEntry>, usize) {
    let mut ordered = entries;
    ordered.sort_by_key(|entry| entry.width * entry.height);

    let mut kept = Vec::new();
    let mut size = 0;
    let mut dropped = 0;
    for entry in ordered {
        let entry_size = entry.wire_size();
        if size + entry_size > TABLE_BUDGET {
            dropped += 1;
            continue;
        }
        size += entry_size;
        kept.push(entry);
    }

    (kept, dropped)
}

/// Splits a table into chunks, each within `budget` bytes.
///
/// Entries are never split: a chunk is a part of a table, and a part of a
/// bitmap would be nothing the host could match against. One entry wider
/// than the budget on its own -- a cursor past 180 pixels -- becomes a
/// chunk of one rather than being dropped.
#[must_use]
pub fn chunk_entries_within(entries: &[HotspotEntry], budget: usize) -> Vec<Vec<HotspotEntry>> {
    let mut chunks = Vec::new();
    let mut chunk = Vec::new();
    let mut size = 0;
    for entry in entries {
        let entry_size = entry.wire_size();
        if !chunk.is_empty() && size + entry_size > budget {
            chunks.push(std::mem::take(&mut chunk));
            size = 0;
        }
        size += entry_size;
        chunk.push(entry.clone());
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }

    chunks
}

/// [`chunk_entries_within`] at the ordinary budget, for hops that have no
/// cap of their own to keep under.
#[must_use]
pub fn chunk_entries(entries: &[HotspotEntry]) -> Vec<Vec<HotspotEntry>> {
    chunk_entries_within(entries, CHUNK_BUDGET)
}

/// The ordinary budget, for callers that clamp it against a cap of their
/// own first.
#[must_use]
pub const fn chunk_budget() -> usize {
    CHUNK_BUDGET
}

/// What the session's desktop names its cursor theme, if that can be known.
///
/// `XCURSOR_THEME` wins, as it wins for every Xcursor reader. Without it,
/// the dconf database is read for the desktop's own setting; a database
/// that names nothing answers the default theme, and one that cannot be
/// read answers nothing at all -- `None`, with no table to send, rather
/// than a guess.
#[must_use]
pub fn active_theme() -> Option<String> {
    active_theme_from(
        std::env::var("XCURSOR_THEME").ok().as_deref(),
        &home_directory(),
    )
}

/// [`active_theme`] against given inputs, so that a test need not touch the
/// environment of the process it runs in.
#[must_use]
pub fn active_theme_from(env_value: Option<&str>, home: &Path) -> Option<String> {
    if let Some(theme) = env_value.map(str::trim).filter(|theme| !theme.is_empty()) {
        return Some(theme.to_owned());
    }

    let Ok(database) = std::fs::read(home.join(".config/dconf/user")) else {
        return None;
    };
    match dconf_string(&database, CURSOR_THEME_KEY) {
        Lookup::Value(theme) if !theme.is_empty() => Some(theme),
        Lookup::Value(_) => Some(DEFAULT_THEME.to_owned()),
        Lookup::Absent => Some(DEFAULT_THEME.to_owned()),
        Lookup::Unreadable => None,
    }
}

/// [`collect_table`] for the session this process runs in: the search path
/// is this machine's, from `$XCURSOR_PATH` or the defaults under this
/// user's home.
#[must_use]
pub fn theme_table(theme: &str) -> Collection {
    let home = home_directory();
    let dirs = search_dirs(std::env::var("XCURSOR_PATH").ok().as_deref(), &home);
    collect_table(theme, &dirs)
}

fn home_directory() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Where the dconf database says a GVariant database file starts.
const GVDB_SIGNATURE: &[u8; 8] = b"GVariant";
const GVDB_HASH_HEADER_LEN: usize = 8;
const GVDB_ITEM_LEN: usize = 24;
/// The bloom-word count is the low 27 bits of the field; the high five are
/// a shift the filter's evaluator needs and this reader does not.
const BLOOM_COUNT_MASK: u32 = (1 << 27) - 1;
const GVDB_ROOT_PARENT: u32 = u32::MAX;
/// The hash-table item whose value is a GVariant.
const GVDB_ITEM_VALUE: u8 = b'v';

/// Reads one string key out of a dconf GVDB database file.
///
/// A dconf value is stored wrapped in a GVariant of type `v`; GLib
/// serialises such a variant as the child's bytes, then a NUL, then the
/// child's type string. Only string children are read, which is all this
/// key ever holds. A key that is not there is an [`Lookup::Absent`], and a
/// database that does not parse is an [`Lookup::Unreadable`] -- answers
/// that disagree, because the first means the desktop's default theme and
/// the second means no theme is to be guessed at all.
fn dconf_string(database: &[u8], key: &str) -> Lookup {
    let Some(table) = hash_table(database) else {
        return Lookup::Unreadable;
    };
    let Some(item) = lookup_item(database, &table, key) else {
        return Lookup::Absent;
    };
    if item.type_byte != GVDB_ITEM_VALUE {
        return Lookup::Unreadable;
    }
    let Some(value) = database.get(item.value_start as usize..item.value_end as usize) else {
        return Lookup::Unreadable;
    };

    // The last NUL in a serialised variant separates the child from the
    // child's type string, which runs to the very end.
    let Some(split) = value.iter().rposition(|byte| *byte == 0) else {
        return Lookup::Unreadable;
    };
    let (child, type_string) = (&value[..split], &value[split + 1..]);
    if type_string != b"s" {
        return Lookup::Unreadable;
    }
    let Some(text) = child.strip_suffix(&[0]) else {
        return Lookup::Unreadable;
    };
    match String::from_utf8(text.to_vec()) {
        Ok(text) => Lookup::Value(text),
        Err(_) => Lookup::Unreadable,
    }
}

/// What a dconf database said about one key.
#[derive(Debug, PartialEq, Eq)]
enum Lookup {
    /// The key names this string.
    Value(String),
    /// The database parses and names no such key.
    Absent,
    /// The database does not parse, or the key's value is not a string.
    Unreadable,
}

/// The root hash table of a GVDB database, as offsets into it.
struct HashTable {
    /// Where the table itself starts, absolute in the database.
    table_at: usize,
    buckets_at: usize,
    bucket_count: usize,
    items_at: usize,
    item_count: usize,
}

/// One 24-byte hash-table item.
struct Item {
    hash: u32,
    parent: u32,
    key_start: u32,
    key_size: u16,
    type_byte: u8,
    value_start: u32,
    value_end: u32,
}

/// Reads the root hash table out of a GVDB database.
///
/// The 24-byte header is a signature spelling `GVariant`, a version, an
/// options word and a pointer to the root table; the table itself is a
/// bloom-word count (whose high bits carry a shift this reader has no use
/// for), a bucket count, the bloom words, the buckets, and the items. The
/// bloom filter is skipped rather than evaluated -- it can only ever rule
/// a key out, and a database this small does not need the speed-up -- but
/// its bytes still have to be stepped around to reach the buckets.
fn hash_table(database: &[u8]) -> Option<HashTable> {
    if database.get(..8)? != GVDB_SIGNATURE.as_slice() {
        return None;
    }
    if read_u32(database, 8)? != 0 {
        return None;
    }
    let table_at = read_u32(database, 16)? as usize;
    let table_end = read_u32(database, 20)? as usize;
    let table = database.get(table_at..table_end)?;

    let bloom_word_count = (read_u32(table, 0)? & BLOOM_COUNT_MASK) as usize;
    let bucket_count = read_u32(table, 4)? as usize;
    let buckets_at = GVDB_HASH_HEADER_LEN + bloom_word_count.checked_mul(4)?;
    let items_at = buckets_at + bucket_count.checked_mul(4)?;
    if table.len() < items_at {
        return None;
    }
    let item_count = (table.len() - items_at) / GVDB_ITEM_LEN;

    Some(HashTable {
        table_at,
        buckets_at,
        bucket_count,
        items_at,
        item_count,
    })
}

/// The item index a bucket chain starts at.
fn bucket_start(database: &[u8], table: &HashTable, bucket: usize) -> Option<usize> {
    let at = table.table_at + table.buckets_at + bucket.checked_mul(4)?;
    read_u32(database, at).map(|first| first as usize)
}

/// Reads one hash-table item by its index.
fn item_at(database: &[u8], table: &HashTable, index: usize) -> Option<Item> {
    let at = table
        .table_at
        .checked_add(table.items_at)?
        .checked_add(index.checked_mul(GVDB_ITEM_LEN)?)?;
    Some(Item {
        hash: read_u32(database, at)?,
        parent: read_u32(database, at + 4)?,
        key_start: read_u32(database, at + 8)?,
        key_size: u16::from_le_bytes([*database.get(at + 12)?, *database.get(at + 13)?]),
        type_byte: *database.get(at + 14)?,
        value_start: read_u32(database, at + 16)?,
        value_end: read_u32(database, at + 20)?,
    })
}

/// Whether `item`'s key -- its own piece plus its parents' pieces before it
/// -- is `key`. dconf stores whole paths as one item with no parent; other
/// GVDB writers split them, and both spellings are read.
fn key_matches(database: &[u8], table: &HashTable, item: &Item, key: &str, depth: usize) -> bool {
    if depth > MAX_THEME_DEPTH {
        return false;
    }
    let Some(piece) =
        database.get(item.key_start as usize..item.key_start as usize + usize::from(item.key_size))
    else {
        return false;
    };
    let Ok(piece) = std::str::from_utf8(piece) else {
        return false;
    };
    if !key.ends_with(piece) {
        return false;
    }
    if item.parent == GVDB_ROOT_PARENT {
        return key.len() == piece.len();
    }
    let Some(parent) = item_at(database, table, item.parent as usize) else {
        return false;
    };

    key_matches(
        database,
        table,
        &parent,
        &key[..key.len() - piece.len()],
        depth + 1,
    )
}

/// Finds the item whose full key path is `key`.
///
/// A GVDB bucket chains its items together, so the chain is walked from
/// the bucket's first item to the next bucket's; an item is found when its
/// hash agrees and its whole key path does.
fn lookup_item(database: &[u8], table: &HashTable, key: &str) -> Option<Item> {
    if table.bucket_count == 0 || table.item_count == 0 {
        return None;
    }

    let hash = djb_hash(key);
    let bucket = (hash as usize) % table.bucket_count;
    let first = bucket_start(database, table, bucket)?;
    let last = if bucket + 1 < table.bucket_count {
        bucket_start(database, table, bucket + 1)?
    } else {
        table.item_count
    };

    for index in first..last.min(table.item_count) {
        let item = item_at(database, table, index)?;
        if item.hash == hash && key_matches(database, table, &item, key, 0) {
            return Some(item);
        }
    }

    None
}

/// djb2, the hash GVDB files are built with.
fn djb_hash(key: &str) -> u32 {
    let mut hash: u32 = 5381;
    for byte in key.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(u32::from(byte));
    }

    hash
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        CHUNK_BUDGET, DEFAULT_THEME, HotspotEntry, Lookup, MAX_DIMENSION, TABLE_BUDGET,
        active_theme_from, chunk_entries, collect_table, dconf_string, images_from_bytes,
        search_dirs,
    };

    /// The dconf key, restated so a test's fixtures name what they hold.
    const KEY: &str = "/org/gnome/desktop/interface/cursor-theme";

    /// Builds an Xcursor file around the images named by
    /// `(nominal size, width, height, hotspot_x, hotspot_y)`, one image
    /// chunk each, with the pixels filled so that two files of one shape
    /// can be told apart by their bytes.
    fn xcursor_file(shade: u8, images: &[(u32, u32, u32, u32, u32)]) -> Vec<u8> {
        let mut words: Vec<u32> = vec![
            0x7275_6358, // "Xcur"
            16,          // header length
            1,           // version
            images.len() as u32,
        ];
        // TOC positions, filled in once the chunk layout is known.
        let mut position = 16 + images.len() * 12;
        let mut positions = Vec::new();
        for &(_, width, height, _, _) in images {
            positions.push(position as u32);
            position += 36 + (width * height * 4) as usize;
        }
        for (index, position) in positions.into_iter().enumerate() {
            words.push(0xfffd_0002); // image chunk type
            words.push(images[index].0); // nominal size, as the subtype
            words.push(position);
        }
        for (index, &(_, width, height, hotspot_x, hotspot_y)) in images.iter().enumerate() {
            words.push(36); // chunk length
            words.push(0xfffd_0002);
            words.push(images[index].0);
            words.push(1); // chunk version
            words.push(width);
            words.push(height);
            words.push(hotspot_x);
            words.push(hotspot_y);
            words.push(30 + index as u32); // frame delay
            let count = width * height;
            words.extend((0..count).map(|pixel| u32::from(shade) + pixel));
        }

        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    /// The images of one Xcursor file, built the way a test names them.
    fn images_of(shade: u8, images: &[(u32, u32, u32, u32, u32)]) -> Vec<HotspotEntry> {
        images_from_bytes(&xcursor_file(shade, images))
    }

    /// A theme tree under a temporary root, laid out the way icon themes
    /// are: `<root>/<theme>/{cursors/*, index.theme}`.
    struct Themes {
        root: PathBuf,
    }

    impl Themes {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "vmlord-cursor-theme-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("a temporary theme root");

            Self { root }
        }

        /// Puts one Xcursor file at `<theme>/cursors/<name>`.
        fn cursor(&self, theme: &str, name: &str, shade: u8, images: &[(u32, u32, u32, u32, u32)]) {
            let directory = self.root.join(theme).join("cursors");
            std::fs::create_dir_all(&directory).expect("a cursors directory");
            std::fs::write(directory.join(name), xcursor_file(shade, images))
                .expect("a cursor file");
        }

        /// Puts an index.theme with the given inherits line.
        fn inherits(&self, theme: &str, inherited: &str) {
            let directory = self.root.join(theme);
            std::fs::create_dir_all(&directory).expect("a theme directory");
            std::fs::write(
                directory.join("index.theme"),
                format!("[Icon Theme]\nName={theme}\nInherits={inherited}\n"),
            )
            .expect("an index.theme");
        }

        fn dirs(&self) -> Vec<PathBuf> {
            vec![self.root.clone()]
        }
    }

    impl Drop for Themes {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A home whose `.config/dconf/user` is the given database, for the
    /// duration of the call.
    fn with_dconf_home(database: &[u8], look: impl FnOnce(&Path)) {
        let home = Themes::new("dconf");
        let directory = home.root.join(".config").join("dconf");
        std::fs::create_dir_all(&directory).expect("a dconf directory");
        std::fs::write(directory.join("user"), database).expect("a dconf database");

        look(&home.root);
    }

    fn one_image(width: u32, height: u32, hotspot_x: u32, hotspot_y: u32) -> HotspotEntry {
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for pixel in pixels.chunks_mut(4) {
            pixel[3] = 0xff;
        }

        HotspotEntry {
            pixels,
            width,
            height,
            hotspot_x,
            hotspot_y,
        }
    }

    #[test]
    fn an_xcursor_file_yields_its_image_with_its_hotspot() {
        let images = images_of(7, &[(24, 24, 24, 2, 3)]);

        assert_eq!(images.len(), 1);
        assert_eq!((images[0].width, images[0].height), (24, 24));
        assert_eq!((images[0].hotspot_x, images[0].hotspot_y), (2, 3));
        assert_eq!(images[0].pixels.len(), 24 * 24 * 4);
        assert!(images[0].is_sendable());
    }

    #[test]
    fn an_animated_file_is_one_entry_per_frame() {
        let images = images_of(
            3,
            &[(32, 32, 32, 1, 1), (32, 32, 32, 4, 6), (32, 32, 32, 0, 9)],
        );

        assert_eq!(images.len(), 3);
        assert_eq!((images[1].hotspot_x, images[1].hotspot_y), (4, 6));
        assert_eq!((images[2].hotspot_x, images[2].hotspot_y), (0, 9));
    }

    #[test]
    fn the_nominal_size_names_the_chunk_and_not_the_bitmap() {
        // The same bitmap under two nominal sizes: two chunks, and two
        // identical entries, because the table maps bitmaps to hotspots.
        let images = images_of(5, &[(24, 24, 24, 1, 1), (64, 24, 24, 1, 1)]);

        assert_eq!(images.len(), 2);
        assert_eq!(images[0], images[1]);
    }

    #[test]
    fn a_truncated_file_yields_what_is_readable_and_never_panics() {
        let bytes = xcursor_file(9, &[(16, 16, 16, 1, 1), (16, 16, 16, 2, 2)]);

        for cut in 0..bytes.len() {
            let images = images_from_bytes(&bytes[..cut]);
            assert!(
                images.len() <= 2,
                "a prefix of a file parses to a prefix of its images"
            );
        }
    }

    #[test]
    fn garbage_is_refused_without_panicking() {
        assert!(images_from_bytes(&[]).is_empty());
        assert!(images_from_bytes(vec![7u8; 64].as_slice()).is_empty());

        // A right magic with a mad table-of-contents count.
        let mut bytes = xcursor_file(1, &[(8, 8, 8, 0, 0)]);
        bytes[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            images_from_bytes(&bytes).len(),
            1,
            "an honest file is read in full"
        );

        // A bitmap claiming to be enormous: a well-formed header and TOC
        // whose chunk names far more pixels than the file could hold.
        let mut bytes = Vec::new();
        let word = |out: &mut Vec<u8>, value: u32| out.extend_from_slice(&value.to_le_bytes());
        word(&mut bytes, 0x7275_6358);
        word(&mut bytes, 16);
        word(&mut bytes, 1);
        word(&mut bytes, 1);
        word(&mut bytes, 0xfffd_0002); // TOC type
        word(&mut bytes, 8); // TOC nominal size
        word(&mut bytes, 28); // TOC position
        word(&mut bytes, 36); // chunk length
        word(&mut bytes, 0xfffd_0002);
        word(&mut bytes, 8); // chunk subtype
        word(&mut bytes, 1); // chunk version
        word(&mut bytes, 0x7fff); // width
        word(&mut bytes, 0x7fff); // height
        word(&mut bytes, 0); // hotspot
        word(&mut bytes, 0);
        word(&mut bytes, 0); // delay
        assert!(
            images_from_bytes(&bytes).is_empty(),
            "a bitmap the file does not carry is refused"
        );

        // A hotspot outside its bitmap. A hotspot on the far edge is in
        // the format's own rules -- and is what `is_sendable` refuses,
        // the codec being stricter than the file format is.
        let bytes = xcursor_file(1, &[(8, 16, 16, 17, 3)]);
        assert!(images_from_bytes(&bytes).is_empty());
        let bytes = xcursor_file(1, &[(8, 16, 16, 3, 17)]);
        assert!(images_from_bytes(&bytes).is_empty());
        assert!(images_from_bytes(&xcursor_file(1, &[(8, 16, 16, 16, 3)]))[0].hotspot_x == 16);
    }

    #[test]
    fn a_theme_is_read_from_its_cursors_directory_whole() {
        let themes = Themes::new("whole");
        themes.cursor("Adwaita", "left_ptr", 1, &[(24, 24, 24, 1, 2)]);
        themes.cursor("Adwaita", "text", 2, &[(24, 16, 16, 8, 8)]);

        let collection = collect_table("Adwaita", &themes.dirs());

        assert_eq!(collection.entries.len(), 2);
        assert_eq!(collection.unreadable, 0);
        assert_eq!(collection.dropped, 0);
    }

    #[test]
    fn the_inherits_chain_is_walked() {
        let themes = Themes::new("inherits");
        themes.cursor("fancy", "left_ptr", 1, &[(24, 24, 24, 1, 1)]);
        themes.inherits("fancy", "hand-drawn, ;: medium");
        themes.cursor("hand-drawn", "text", 2, &[(24, 16, 16, 4, 4)]);
        themes.cursor("medium", "help", 3, &[(24, 32, 32, 2, 2)]);

        let collection = collect_table("fancy", &themes.dirs());

        assert_eq!(
            collection.entries.len(),
            3,
            "the theme and both of its inherits"
        );
    }

    #[test]
    fn a_theme_that_inherits_itself_is_read_once() {
        let themes = Themes::new("self");
        themes.cursor("looped", "left_ptr", 1, &[(24, 24, 24, 1, 1)]);
        themes.inherits("looped", "looped");

        let collection = collect_table("looped", &themes.dirs());

        assert_eq!(collection.entries.len(), 1);
    }

    #[test]
    fn a_cycle_of_inherits_terminates() {
        let themes = Themes::new("cycle");
        themes.cursor("one", "left_ptr", 1, &[(24, 24, 24, 1, 1)]);
        themes.inherits("one", "two");
        themes.inherits("two", "three");
        themes.cursor("three", "text", 2, &[(24, 16, 16, 4, 4)]);
        themes.inherits("three", "one");

        let collection = collect_table("one", &themes.dirs());

        assert_eq!(collection.entries.len(), 2);
    }

    #[test]
    fn a_theme_found_nowhere_falls_back_to_default() {
        let themes = Themes::new("fallback");
        themes.cursor(DEFAULT_THEME, "left_ptr", 1, &[(24, 24, 24, 1, 1)]);

        let collection = collect_table("nonexistent", &themes.dirs());

        assert_eq!(collection.entries, images_of(1, &[(24, 24, 24, 1, 1)]));
    }

    #[test]
    fn the_first_directory_holding_the_theme_wins() {
        let themes = Themes::new("precedence");
        themes.cursor("shared", "left_ptr", 1, &[(24, 24, 24, 1, 1)]);
        themes.cursor("shared", "other", 2, &[(24, 24, 24, 2, 2)]);

        let later = themes.root.join("later");
        std::fs::create_dir_all(later.join("shared").join("cursors")).expect("a shadowing theme");
        std::fs::write(
            later.join("shared").join("cursors").join("left_ptr"),
            xcursor_file(9, &[(24, 24, 24, 5, 5)]),
        )
        .expect("a shadowing cursor");

        let dirs = vec![later, themes.root.clone()];
        let collection = collect_table("shared", &dirs);

        assert_eq!(
            (
                collection.entries[0].hotspot_x,
                collection.entries[0].hotspot_y
            ),
            (5, 5),
            "the theme in the earlier directory is the one read"
        );
        assert_eq!(
            collection.entries.len(),
            1,
            "and the shadowed theme is not merged in"
        );
    }

    #[test]
    fn a_search_path_set_by_the_environment_replaces_the_defaults() {
        let dirs = search_dirs(Some("/opt/themes:/home/u/.icons"), Path::new("/home/u"));

        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/opt/themes"),
                PathBuf::from("/home/u/.icons"),
            ]
        );
    }

    #[test]
    fn the_default_search_order_starts_in_the_home() {
        let dirs = search_dirs(None, Path::new("/home/u"));

        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/u/.local/share/icons"),
                PathBuf::from("/home/u/.icons"),
                PathBuf::from("/usr/share/icons"),
                PathBuf::from("/usr/share/pixmaps"),
            ]
        );
    }

    #[test]
    fn identical_entries_are_deduplicated() {
        let themes = Themes::new("dedupe");
        // Three names for the same bitmap, as a theme's aliases are, and
        // one shape of its own.
        themes.cursor("aliased", "left_ptr", 1, &[(24, 24, 24, 1, 1)]);
        themes.cursor("aliased", "arrow", 1, &[(24, 24, 24, 1, 1)]);
        themes.cursor("aliased", "default", 1, &[(24, 24, 24, 1, 1)]);
        themes.cursor("aliased", "text", 2, &[(24, 16, 16, 4, 4)]);

        let collection = collect_table("aliased", &themes.dirs());

        assert_eq!(collection.entries.len(), 2);
    }

    #[test]
    fn a_table_past_the_budget_loses_its_largest_entries() {
        // Twenty cursors of the largest size there is and ten small ones:
        // five megabytes of table in all.
        let mut entries: Vec<HotspotEntry> = (0..20)
            .map(|index| HotspotEntry {
                pixels: vec![index; MAX_DIMENSION as usize * MAX_DIMENSION as usize * 4],
                width: MAX_DIMENSION,
                height: MAX_DIMENSION,
                hotspot_x: 0,
                hotspot_y: 0,
            })
            .collect();
        entries.extend((0..10).map(|_| one_image(32, 32, 0, 0)));

        let (kept, dropped) = super::trim_to_budget(entries);

        assert_eq!(
            kept.iter().filter(|entry| entry.width == 32).count(),
            10,
            "the small entries are the ones worth anchoring, and all of them are kept"
        );
        assert_eq!(kept.len() + dropped, 30);
        let total: usize = kept.iter().map(HotspotEntry::wire_size).sum();
        assert!(total <= TABLE_BUDGET);
        assert_eq!(dropped, 5, "what the budget cannot hold is the largest");
    }

    #[test]
    fn chunks_hold_every_entry_in_order_and_stay_small() {
        let entries: Vec<_> = (0..40)
            .map(|index| one_image(64, 64, index % 8, index % 7))
            .collect();

        let chunks = chunk_entries(&entries);

        assert!(
            chunks.len() > 1,
            "a table of forty 64-pixel cursors does not fit one chunk"
        );
        assert_eq!(
            chunks.concat(),
            entries,
            "chunking loses nothing and reorders nothing"
        );
        for chunk in &chunks {
            let size: usize = chunk.iter().map(HotspotEntry::wire_size).sum();
            assert!(
                size <= CHUNK_BUDGET || chunk.len() == 1,
                "every chunk is within the budget, but for one oversized entry alone"
            );
        }
    }

    #[test]
    fn one_entry_bigger_than_the_budget_is_its_own_chunk_rather_than_dropped() {
        let big = one_image(MAX_DIMENSION, MAX_DIMENSION, 0, 0);
        let entries = vec![big, one_image(8, 8, 0, 0)];

        let chunks = chunk_entries(&entries);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 1);
        assert_eq!(chunks[1].len(), 1);
        assert_eq!(chunks[1][0].width, 8);
    }

    #[test]
    fn sendable_rejects_what_the_codec_would() {
        assert!(!one_image(0, 8, 0, 0).is_sendable());
        assert!(!one_image(MAX_DIMENSION + 1, 8, 0, 0).is_sendable());
        let mut edge = one_image(8, 8, 8, 4);
        assert!(
            !edge.is_sendable(),
            "a hotspot on the far edge is outside the codec's rule"
        );
        edge.hotspot_x = 7;
        assert!(edge.is_sendable());
    }

    /// Builds a dconf GVDB database with at most one key, as the fixture
    /// for the reader. The layout is the one GLib writes: a 24-byte header;
    /// a root table of bloom words, buckets and items; then, outside the
    /// root table, the key and the value -- a variant of child bytes, a
    /// NUL, and the child's type string, eight-byte aligned like every
    /// value pointer requires.
    fn dconf_database(key: &str, value: Option<&str>) -> Vec<u8> {
        // Real databases carry bloom words; a fixture with two proves the
        // reader steps around them.
        dconf_database_with_bloom(key, value, 2)
    }

    fn dconf_database_with_bloom(key: &str, value: Option<&str>, bloom_words: usize) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        fn word(out: &mut Vec<u8>, value: u32) {
            out.extend_from_slice(&value.to_le_bytes());
        }
        fn djb_hash_of(key: &str) -> u32 {
            super::djb_hash(key)
        }

        let key_bytes = key.as_bytes();
        let variant = value.map(|text| {
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0); // the string's own terminator
            bytes.push(0); // the NUL that ends the child of the variant
            bytes.push(b's'); // the child's type string
            bytes
        });
        let variant_len = variant.as_ref().map_or(0, Vec::len);

        let table_at = 24u32;
        let buckets_at = table_at + 8 + bloom_words as u32 * 4;
        let item_at = buckets_at + 4;
        let items_end = item_at + 24;
        let key_at = items_end;
        let key_end = key_at + key_bytes.len() as u32;
        // A value pointer is required to be eight-byte aligned.
        let value_at = key_end.div_ceil(8) * 8;

        // Header: signature, version, options, root pointer.
        out.extend_from_slice(b"GVariant");
        word(&mut out, 0);
        word(&mut out, 0);
        word(&mut out, table_at);
        word(&mut out, items_end);
        // The root table: the bloom-word count, the bucket count, the
        // bloom words with their bits honestly set, then one bucket
        // holding item 0. The bloom filter is the reader's to evaluate or
        // to skip.
        word(&mut out, bloom_words as u32);
        word(&mut out, 1);
        let hash = djb_hash_of(key);
        let bloom_index = (hash / 32) % bloom_words.max(1) as u32;
        for index in 0..bloom_words {
            let set = u32::from(index as u32 == bloom_index) << (hash & 31);
            word(&mut out, set);
        }
        word(&mut out, 0);
        // The item.
        word(&mut out, hash);
        word(&mut out, u32::MAX);
        word(&mut out, key_at);
        out.extend_from_slice(&(key_bytes.len() as u16).to_le_bytes());
        out.push(match value {
            Some(_) => b'v',
            None => b'H',
        });
        out.push(0);
        word(&mut out, value_at);
        word(&mut out, value_at + variant_len as u32);
        // The key, and then the value, aligned as its pointer promises.
        out.extend_from_slice(key_bytes);
        while out.len() < value_at as usize {
            out.push(0);
        }
        if let Some(variant) = variant {
            out.extend_from_slice(&variant);
        }

        out
    }

    #[test]
    fn bloom_words_are_stepped_around_rather_than_read_as_buckets() {
        let database = dconf_database(KEY, Some("Yaru"));

        assert_eq!(
            dconf_string(&database, KEY),
            Lookup::Value("Yaru".to_owned())
        );
        // And a database with none, at the other end of the field.
        let no_bloom = dconf_database_with_bloom(KEY, Some("Yaru"), 0);
        assert_eq!(
            dconf_string(&no_bloom, KEY),
            Lookup::Value("Yaru".to_owned())
        );
    }

    #[test]
    fn the_cursor_theme_is_read_out_of_a_dconf_database() {
        let database = dconf_database(KEY, Some("Yaru"));

        assert_eq!(
            dconf_string(&database, KEY),
            Lookup::Value("Yaru".to_owned())
        );
        with_dconf_home(&database, |home| {
            assert_eq!(active_theme_from(None, home), Some("Yaru".to_owned()));
        });
    }

    #[test]
    fn a_dconf_database_that_names_no_theme_answers_the_default() {
        let database = dconf_database("/org/gnome/desktop/other/key", Some("unrelated"));

        assert_eq!(dconf_string(&database, KEY), Lookup::Absent);
        with_dconf_home(&database, |home| {
            assert_eq!(
                active_theme_from(None, home),
                Some(DEFAULT_THEME.to_owned())
            );
        });
    }

    #[test]
    fn a_dconf_database_that_does_not_parse_answers_nothing() {
        assert_eq!(dconf_string(&[0u8; 64], KEY), Lookup::Unreadable);
        with_dconf_home(&[0u8; 64], |home| {
            assert_eq!(active_theme_from(None, home), None);
        });
    }

    #[test]
    fn a_missing_dconf_database_answers_nothing() {
        let empty = Themes::new("no-dconf");

        assert_eq!(active_theme_from(None, &empty.root), None);
    }

    #[test]
    fn the_environment_names_the_theme_before_dconf_is_asked() {
        let database = dconf_database(KEY, Some("Yaru"));

        with_dconf_home(&database, |home| {
            assert_eq!(
                active_theme_from(Some("Papirus"), home),
                Some("Papirus".to_owned())
            );
            assert_eq!(
                active_theme_from(Some("  "), home),
                Some("Yaru".to_owned()),
                "an empty variable is no theme at all"
            );
        });
    }

    #[test]
    fn a_dconf_value_that_is_not_a_string_is_refused() {
        let mut database = dconf_database(KEY, Some("Yaru"));
        // The variant's type string, at the very end, becomes `u`.
        let last = database.len() - 1;
        database[last] = b'u';

        assert_eq!(dconf_string(&database, KEY), Lookup::Unreadable);
    }
}
