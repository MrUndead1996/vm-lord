//! The disk inside a qcow2 image, as a plain stream of bytes.
//!
//! `Qcow2Image` is `Read + Seek` over the guest's disk: offset zero is the
//! guest's sector zero, the stream ends at the disk's virtual size, and holes
//! read as the zeros the guest would see. The importer that writes a VHDX wants
//! nothing more than that.
//!
//! Parsing the format is the `qcow` crate's work -- header, L1 and L2 tables,
//! zlib and zstd clusters. Mapping a guest offset onto those tables is ours, and
//! that division is deliberate. The crate's own reader opens the backing file
//! named in the header (any path in the image, opened on the host), zero-fills a
//! failed cluster read rather than reporting it, and panics on a handful of
//! malformed inputs. None of that is acceptable on a file fetched over HTTP, and
//! all of it lives in the fifty lines of lookup we would have written anyway.
//!
//! The header is vetted before the parser sees the file -- see [`header`].

mod header;

use std::{
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use qcow::{
    CompressionType, DynamicQcow, Qcow2,
    levels::{ClusterDescriptor, L1Entry, L2Entry},
};

use crate::{
    error::{Qcow2Error, qcow2_io_error},
    qcow2::header::{Compression, HeaderFacts},
};

/// The disk inside a qcow2 image, readable as one contiguous stream.
///
/// Reads are served a cluster at a time, so a caller copying the disk out wants
/// a buffer of at least a cluster to avoid re-reading one; `cluster_size` says
/// how big that is.
pub struct Qcow2Image {
    path: PathBuf,
    file: BufReader<File>,
    file_length: u64,
    qcow: Qcow2,
    virtual_size: u64,
    cluster_size: u64,
    /// L2 entries per L2 table: one cluster's worth of eight-byte entries.
    entries_per_l2: u64,
    compression: CompressionType,
    /// Position within the guest's disk, which is what `Seek` moves.
    position: u64,
    /// The last cluster read, and which one it was. A caller reading the disk in
    /// order touches each cluster once and finds it here for the rest of it.
    cluster: Box<[u8]>,
    cluster_index: Option<u64>,
    /// The last L2 table read, and the L1 entry it came from.
    l2_table: Vec<L2Entry>,
    l1_index: Option<u64>,
}

impl Qcow2Image {
    /// Opens `path` as the disk it contains, refusing it if the disk is larger
    /// than `capacity` bytes or if the image asks for anything this reader does
    /// not implement.
    ///
    /// `capacity` is the size of the disk the image is destined for -- the VM's
    /// `disk_gb` in bytes. Checking it here rather than at the first write is
    /// the difference between a refusal and a half-written VHDX.
    pub fn open(path: &Path, capacity: u64) -> Result<Self, Qcow2Error> {
        let file = File::open(path).map_err(qcow2_io_error("open the image", path))?;
        let file_length = file
            .metadata()
            .map_err(qcow2_io_error("measure the image", path))?
            .len();
        let mut file = BufReader::new(file);

        let facts = header::inspect(&mut file, file_length, capacity).map_err(|error| {
            // A file that is not a qcow2 at all is the one refusal worth naming
            // the file in: the others are about an image, this one about a
            // download that is not one.
            match error {
                Qcow2Error::Malformed(message) if message == "bad magic" => Qcow2Error::NotQcow2 {
                    path: path.to_path_buf(),
                },
                other => other,
            }
        })?;

        let qcow = load(&mut file, path)?;
        check_agreement(&qcow, &facts)?;

        tracing::info!(
            "{} holds a {}-byte disk within a {capacity}-byte capacity",
            path.display(),
            facts.virtual_size
        );
        Ok(Self {
            path: path.to_path_buf(),
            file,
            file_length,
            qcow,
            virtual_size: facts.virtual_size,
            cluster_size: facts.cluster_size,
            entries_per_l2: facts.cluster_size / 8,
            compression: match facts.compression {
                Compression::Zlib => CompressionType::Zlib,
                Compression::Zstd => CompressionType::Zstd,
            },
            position: 0,
            cluster: vec![0; facts.cluster_size as usize].into_boxed_slice(),
            cluster_index: None,
            l2_table: Vec::new(),
            l1_index: None,
        })
    }

    /// The size of the disk inside the image, in bytes. The stream ends here.
    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    /// The image's cluster size: the granularity every read is served at.
    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    /// Loads the cluster holding a guest offset into `self.cluster`.
    ///
    /// Every path that cannot produce cluster contents leaves zeros there: an
    /// unallocated L2 table, an unallocated cluster, or one flagged as reading
    /// all zeroes. Those are holes, and a hole is data -- the guest sees zeros
    /// where nothing was ever written.
    fn load_cluster(&mut self, index: u64) -> Result<(), Qcow2Error> {
        if self.cluster_index == Some(index) {
            return Ok(());
        }

        let l1_index = index / self.entries_per_l2;
        let l2_index = index % self.entries_per_l2;
        let l2_offset = match self.qcow.l1_table.get(l1_index as usize) {
            Some(entry) => entry.l2_offset,
            None => {
                // The header check proved the L1 table covers the disk, and
                // reads are clamped to the disk, so this is unreachable rather
                // than a bad image.
                return Err(Qcow2Error::Malformed(format!(
                    "cluster {index} needs L1 entry {l1_index} of a {}-entry table",
                    self.qcow.l1_table.len()
                )));
            }
        };

        // The buffer stops being the cluster it was named after the first byte
        // is written into it, and a failure below leaves it half filled. Saying
        // so before the writing starts is what keeps a later read of the cluster
        // that used to be cached from being served zeros.
        self.cluster_index = None;
        self.cluster.fill(0);
        if l2_offset == 0 {
            self.cluster_index = Some(index);
            return Ok(());
        }

        self.load_l2_table(l1_index, l2_offset)?;
        let entry = self.l2_table[l2_index as usize].clone();
        self.read_cluster_contents(index, &entry)?;

        self.cluster_index = Some(index);
        Ok(())
    }

    /// Reads the L2 table at `l2_offset` unless it is the one already held.
    fn load_l2_table(&mut self, l1_index: u64, l2_offset: u64) -> Result<(), Qcow2Error> {
        if self.l1_index == Some(l1_index) {
            return Ok(());
        }

        self.check_span(l2_offset, self.cluster_size, "an L2 table")?;
        if !l2_offset.is_multiple_of(self.cluster_size) {
            return Err(Qcow2Error::Malformed(format!(
                "the L2 table at {l2_offset} does not start on a cluster boundary"
            )));
        }

        // `L1Entry`'s fields are public and `is_used` says only whether the
        // table's refcount is one, which a reader has no use for. Rebuilding the
        // entry here keeps the borrow of the parsed image from colliding with the
        // borrow of the file it is read from.
        let entry = L1Entry {
            l2_offset,
            is_used: false,
        };
        self.l2_table = entry
            .read_l2(&mut self.file, self.cluster_size.trailing_zeros())
            .ok_or_else(|| {
                Qcow2Error::Malformed(format!("the L2 table at {l2_offset} could not be read"))
            })?;
        self.l1_index = Some(l1_index);
        tracing::debug!("read the L2 table at {l2_offset} for L1 entry {l1_index}");
        Ok(())
    }

    /// Fills `self.cluster` from one L2 entry, which the caller has already
    /// zeroed for the benefit of the holes.
    fn read_cluster_contents(&mut self, index: u64, entry: &L2Entry) -> Result<(), Qcow2Error> {
        match &entry.cluster_descriptor {
            ClusterDescriptor::Standard(cluster) => {
                if cluster.all_zeroes || cluster.host_cluster_offset == 0 {
                    return Ok(());
                }
                self.check_span(cluster.host_cluster_offset, self.cluster_size, "a cluster")?;
            }
            ClusterDescriptor::Compressed(cluster) => {
                // A compressed cluster is not cluster-aligned and its length is
                // whatever the codec needs, so only its start can be checked
                // here; the decoder stops when it has produced a cluster.
                self.check_span(cluster.host_cluster_offset, 1, "a compressed cluster")?;
            }
        }

        entry
            .read_contents(&mut self.file, &mut self.cluster, self.compression)
            .map_err(|source| {
                Qcow2Error::Malformed(format!(
                    "the contents of cluster {index} could not be read: {source}"
                ))
            })
    }

    /// Refuses an offset the file does not contain. The offsets come out of
    /// tables in the image, so this is where a truncated or hostile file stops
    /// being read as if it were whole.
    fn check_span(&self, offset: u64, length: u64, what: &str) -> Result<(), Qcow2Error> {
        if offset.saturating_add(length) > self.file_length {
            return Err(Qcow2Error::Malformed(format!(
                "{what} at {offset} lies past the end of the {}-byte file",
                self.file_length
            )));
        }
        Ok(())
    }
}

impl Read for Qcow2Image {
    /// Serves bytes out of one cluster per call, however large the buffer is, so
    /// that a read never spans two lookups. `read_exact`, `read_to_end` and
    /// `io::copy` all cope with the short reads that follow.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.virtual_size || buffer.is_empty() {
            return Ok(0);
        }

        let index = self.position / self.cluster_size;
        let offset_in_cluster = (self.position % self.cluster_size) as usize;
        let wanted = buffer
            .len()
            .min(self.cluster.len() - offset_in_cluster)
            .min((self.virtual_size - self.position) as usize);

        self.load_cluster(index).map_err(io::Error::other)?;
        buffer[..wanted]
            .copy_from_slice(&self.cluster[offset_in_cluster..offset_in_cluster + wanted]);
        self.position += wanted as u64;
        Ok(wanted)
    }
}

impl Seek for Qcow2Image {
    /// Seeking past the end of the disk is allowed, as it is for a file; a read
    /// there returns nothing. Seeking before the start is an error.
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let (base, offset) = match from {
            SeekFrom::Start(position) => {
                self.position = position;
                return Ok(position);
            }
            SeekFrom::Current(offset) => (self.position, offset),
            SeekFrom::End(offset) => (self.virtual_size, offset),
        };

        self.position = base.checked_add_signed(offset).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("seeking {offset} from {base} leaves the disk"),
            )
        })?;
        Ok(self.position)
    }
}

impl std::fmt::Debug for Qcow2Image {
    /// Written by hand because the parsed image behind it prints its whole L1
    /// table, which for a real cloud image is pages of hex.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Qcow2Image")
            .field("path", &self.path)
            .field("virtual_size", &self.virtual_size)
            .field("cluster_size", &self.cluster_size)
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

/// Hands the file to the parser, which reads the header a second time along with
/// the L1 table and the snapshot list.
fn load(file: &mut BufReader<File>, path: &Path) -> Result<Qcow2, Qcow2Error> {
    file.rewind()
        .map_err(qcow2_io_error("rewind the image", path))?;
    match qcow::load(file) {
        Ok(DynamicQcow::Qcow2(qcow)) => Ok(qcow),
        // Unreachable: the header check has already refused every version but
        // 2 and 3, and both of those parse as `Qcow2`.
        Ok(DynamicQcow::Qcow1(qcow)) => Err(Qcow2Error::UnsupportedVersion {
            version: qcow.header.version,
        }),
        Err(source) => Err(Qcow2Error::Malformed(format!(
            "the image could not be parsed: {source}"
        ))),
    }
}

/// Checks the parser's reading of the header against ours.
///
/// The two parse the same bytes independently, so a disagreement means one of
/// them is wrong about the file -- and a check that was passed against one
/// reading of the header protects nothing if the reads are served against
/// another.
fn check_agreement(qcow: &Qcow2, facts: &HeaderFacts) -> Result<(), Qcow2Error> {
    let parsed = (
        qcow.header.version,
        qcow.header.size,
        qcow.header.cluster_size(),
        qcow.header.l1_size,
        qcow.header.l1_table_offset,
        qcow.header.backing_file.is_some(),
        qcow.snapshots.len(),
    );
    let checked = (
        facts.version,
        facts.virtual_size,
        facts.cluster_size,
        facts.l1_size,
        facts.l1_table_offset,
        false,
        0,
    );
    if parsed != checked {
        return Err(Qcow2Error::Malformed(format!(
            "the header reads differently to the parser: {parsed:?} against {checked:?}"
        )));
    }

    if qcow.l1_table.len() != facts.l1_size as usize {
        return Err(Qcow2Error::Malformed(format!(
            "the L1 table came back with {} of its {} entries",
            qcow.l1_table.len(),
            facts.l1_size
        )));
    }
    Ok(())
}
