//! Vetting a qcow2 header before a parser is let anywhere near the file.
//!
//! The image is downloaded from the internet, and the parser behind it -- the
//! `qcow` crate -- last saw a release in October 2021. Two kinds of trouble
//! follow from that, and both are headed off here rather than downstream.
//!
//! The first is features. A qcow2 file states what a reader must understand in
//! order to read it, and the spec requires refusing an image with an
//! incompatible bit one does not know. The crate's `IncompatibleFeatures`
//! discards the unknown bits while parsing, so asking it is not enough: the
//! field is read here as the raw 64 bits it is, and anything but the one bit we
//! support is a refusal.
//!
//! The second is arithmetic. Every count in the header is a promise the parser
//! believes: `l1_size` becomes the length of a `Vec`, a header extension's
//! length becomes the size of an allocation. A file claiming four billion L1
//! entries is a few bytes to write and gigabytes to open. So the counts are
//! bounded here, while the file is still nothing but bytes.

use std::io::{self, Read, Seek, SeekFrom};

use crate::error::Qcow2Error;

/// The magic every qcow file starts with: `QFI\xfb`.
const MAGIC: [u8; 4] = *b"QFI\xfb";

/// Bytes of header common to versions 2 and 3.
const V2_HEADER_LENGTH: u64 = 72;

/// Bytes of header a version 3 image must have at least: the v2 fields plus the
/// feature bitmaps, `refcount_order` and `header_length` itself.
const V3_MIN_HEADER_LENGTH: u32 = 104;

/// The largest header a version 3 image may declare. The spec lets the header
/// grow, and a reader is to skip what it does not know; the ceiling only keeps
/// the number in the range where it can be seeked to.
const MAX_HEADER_LENGTH: u32 = 4096;

/// The one incompatible feature bit this reader supports: bit 3, which says the
/// `compression_type` field is present and names something other than zlib.
/// zstd is the only other value, and the parser decodes it.
const INCOMPATIBLE_COMPRESSION_TYPE: u64 = 1 << 3;

/// Smallest cluster the format allows: 512 bytes.
const MIN_CLUSTER_BITS: u32 = 9;

/// Largest cluster qemu will write or open: 2 MiB. Ours matches so that a
/// cluster always fits comfortably in a buffer.
const MAX_CLUSTER_BITS: u32 = 21;

/// Largest active L1 table this reader will hold: qemu's own 32 MiB limit,
/// which at eight bytes an entry is four million entries.
const MAX_L1_ENTRIES: u64 = 4 * 1024 * 1024;

/// Header extensions are a handful of small records; a file with more than this,
/// or with one larger than a cluster, is not a file qemu wrote.
const MAX_HEADER_EXTENSIONS: usize = 32;
const MAX_HEADER_EXTENSION_LENGTH: u32 = 64 * 1024;

/// The compression type an image's clusters use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Compression {
    Zlib,
    Zstd,
}

/// What the header says about the image, once it has been found acceptable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeaderFacts {
    pub(crate) version: u32,
    /// Size of the disk inside the image, in bytes.
    pub(crate) virtual_size: u64,
    pub(crate) cluster_size: u64,
    pub(crate) l1_size: u32,
    pub(crate) l1_table_offset: u64,
    pub(crate) compression: Compression,
}

/// Reads the header of `reader` and either describes the image or refuses it.
///
/// `file_length` is the size of the file on disk, which bounds every offset the
/// header hands out. `capacity` is the disk the image is destined for: an image
/// larger than that is refused here, before a byte of content is read, because
/// the alternative is discovering it half way through writing a VHDX.
pub(crate) fn inspect(
    reader: &mut (impl Read + Seek),
    file_length: u64,
    capacity: u64,
) -> Result<HeaderFacts, Qcow2Error> {
    let mut header = [0u8; V2_HEADER_LENGTH as usize];
    reader
        .seek(SeekFrom::Start(0))
        .and_then(|_| reader.read_exact(&mut header))
        .map_err(|source| match source.kind() {
            io::ErrorKind::UnexpectedEof => {
                Qcow2Error::Malformed("the file is shorter than a qcow2 header".to_owned())
            }
            _ => Qcow2Error::Malformed(format!("the header could not be read: {source}")),
        })?;

    if header[..4] != MAGIC {
        // The caller turns this into `NotQcow2`, which needs the path to read
        // sensibly, and the path is not this function's business.
        return Err(Qcow2Error::Malformed("bad magic".to_owned()));
    }

    let version = be_u32(&header, 4);
    if version != 2 && version != 3 {
        return Err(Qcow2Error::UnsupportedVersion { version });
    }

    if be_u64(&header, 8) != 0 || be_u32(&header, 16) != 0 {
        return Err(Qcow2Error::BackingFile);
    }

    let cluster_bits = be_u32(&header, 20);
    if !(MIN_CLUSTER_BITS..=MAX_CLUSTER_BITS).contains(&cluster_bits) {
        return Err(Qcow2Error::UnsupportedClusterSize { cluster_bits });
    }
    let cluster_size = 1u64 << cluster_bits;

    let virtual_size = be_u64(&header, 24);
    if virtual_size == 0 {
        return Err(Qcow2Error::Malformed(
            "the image holds a disk of zero bytes".to_owned(),
        ));
    }
    if virtual_size > capacity {
        return Err(Qcow2Error::TooLarge {
            virtual_size,
            capacity,
        });
    }

    let crypt_method = be_u32(&header, 32);
    if crypt_method != 0 {
        return Err(Qcow2Error::Encrypted { crypt_method });
    }

    let l1_size = be_u32(&header, 36);
    let l1_table_offset = be_u64(&header, 40);
    check_l1_table(
        l1_size,
        l1_table_offset,
        virtual_size,
        cluster_size,
        file_length,
    )?;

    let snapshots = be_u32(&header, 60);
    if snapshots != 0 {
        return Err(Qcow2Error::Snapshots { count: snapshots });
    }

    let (compression, extensions_offset) = if version == 3 {
        check_version3(reader)?
    } else {
        (Compression::Zlib, V2_HEADER_LENGTH)
    };

    check_extensions(reader, extensions_offset, file_length)?;

    log::debug!(
        "the image is qcow v{version}: a {virtual_size}-byte disk in {cluster_size}-byte \
         {compression:?} clusters, {l1_size} L1 entries at {l1_table_offset}"
    );
    Ok(HeaderFacts {
        version,
        virtual_size,
        cluster_size,
        l1_size,
        l1_table_offset,
        compression,
    })
}

/// Checks that the active L1 table covers the disk and can be held in memory.
///
/// A table too small is a contradiction: the image would have no way to say
/// where the end of its own disk lives. A table larger than the disk needs is
/// merely odd -- an image that was shrunk, say -- so it earns a warning and is
/// read anyway, up to the point where the length stops being an address and
/// becomes an allocation.
fn check_l1_table(
    l1_size: u32,
    l1_table_offset: u64,
    virtual_size: u64,
    cluster_size: u64,
    file_length: u64,
) -> Result<(), Qcow2Error> {
    let entries_per_l2 = cluster_size / 8;
    let clusters = virtual_size.div_ceil(cluster_size);
    let needed = clusters.div_ceil(entries_per_l2);
    let l1_size = u64::from(l1_size);

    if l1_size < needed {
        return Err(Qcow2Error::Malformed(format!(
            "the L1 table has {l1_size} entries, too few to describe the {clusters} clusters of \
             the disk"
        )));
    }
    if l1_size > MAX_L1_ENTRIES {
        return Err(Qcow2Error::Malformed(format!(
            "the L1 table claims {l1_size} entries, more than the {MAX_L1_ENTRIES} a qcow2 image \
             may have"
        )));
    }
    if l1_size > needed {
        log::warn!("the L1 table has {l1_size} entries where {needed} would do");
    }

    let table_bytes = l1_size * 8;
    if l1_table_offset == 0 || !l1_table_offset.is_multiple_of(cluster_size) {
        return Err(Qcow2Error::Malformed(format!(
            "the L1 table starts at {l1_table_offset}, which is not a cluster boundary"
        )));
    }
    if l1_table_offset.saturating_add(table_bytes) > file_length {
        return Err(Qcow2Error::Malformed(format!(
            "the L1 table runs from {l1_table_offset} past the end of a {file_length}-byte file"
        )));
    }

    Ok(())
}

/// Reads the version 3 part of the header: the feature bitmaps and the
/// compression type. Returns the compression in force and the offset the header
/// extensions start at.
fn check_version3(reader: &mut (impl Read + Seek)) -> Result<(Compression, u64), Qcow2Error> {
    let mut tail = [0u8; (V3_MIN_HEADER_LENGTH as usize) - (V2_HEADER_LENGTH as usize)];
    read_at(reader, V2_HEADER_LENGTH, &mut tail, "the version 3 header")?;

    let incompatible = be_u64(&tail, 0);
    let unsupported = incompatible & !INCOMPATIBLE_COMPRESSION_TYPE;
    if unsupported != 0 {
        return Err(Qcow2Error::UnsupportedFeatures { bits: unsupported });
    }

    // Compatible and auto-clear bits may be ignored by a reader, and this reader
    // never writes, so an unknown one of either is worth a line in the log and
    // nothing more.
    let compatible = be_u64(&tail, 8);
    let autoclear = be_u64(&tail, 16);
    if compatible != 0 || autoclear != 0 {
        log::debug!(
            "the image sets compatible features {compatible:#018x} and auto-clear features \
             {autoclear:#018x}, none of which affect reading it"
        );
    }

    let header_length = be_u32(&tail, 28);
    if !(V3_MIN_HEADER_LENGTH..=MAX_HEADER_LENGTH).contains(&header_length) {
        return Err(Qcow2Error::Malformed(format!(
            "the header claims to be {header_length} bytes long"
        )));
    }
    if !header_length.is_multiple_of(8) {
        return Err(Qcow2Error::Malformed(format!(
            "the header length {header_length} is not a multiple of eight"
        )));
    }

    let declares_compression = incompatible & INCOMPATIBLE_COMPRESSION_TYPE != 0;
    // The field sits just past the fields every version 3 image has, and is
    // present only in a header long enough to contain it. Reading it out of a
    // shorter header would be reading whatever follows the header instead.
    let compression_type = if header_length > V3_MIN_HEADER_LENGTH {
        let mut byte = [0u8; 1];
        read_at(
            reader,
            u64::from(V3_MIN_HEADER_LENGTH),
            &mut byte,
            "the compression type",
        )?;
        byte[0]
    } else {
        0
    };

    // The two must agree. A zstd image whose bit is clear would be read as
    // zlib -- every cluster failing to inflate -- and a bit set beside a zero
    // type is a file describing itself wrongly.
    let compression = match (declares_compression, compression_type) {
        (false, 0) => Compression::Zlib,
        (true, 1) => Compression::Zstd,
        (true, other) => {
            return Err(Qcow2Error::UnsupportedCompression {
                compression_type: other,
            });
        }
        (false, other) => {
            return Err(Qcow2Error::Malformed(format!(
                "the image names compression type {other} without setting the feature bit that \
                 must accompany it"
            )));
        }
    };

    Ok((compression, u64::from(header_length)))
}

/// Walks the header extensions, refusing any that would cost more to read than
/// qemu would ever spend to write.
///
/// Nothing in them is used: the extensions this reader would care about -- an
/// external data file name, a backing file format -- accompany features already
/// refused. The walk exists so that the parser behind us, which allocates an
/// extension's declared length before looking at it, is never handed a length
/// nobody checked.
fn check_extensions(
    reader: &mut (impl Read + Seek),
    mut offset: u64,
    file_length: u64,
) -> Result<(), Qcow2Error> {
    for _ in 0..MAX_HEADER_EXTENSIONS {
        let mut record = [0u8; 8];
        read_at(reader, offset, &mut record, "a header extension")?;

        let kind = be_u32(&record, 0);
        let length = be_u32(&record, 4);
        if kind == 0 {
            return Ok(());
        }
        if length > MAX_HEADER_EXTENSION_LENGTH {
            return Err(Qcow2Error::Malformed(format!(
                "a header extension of type {kind:#010x} claims {length} bytes"
            )));
        }

        // Each record is padded to a multiple of eight, and the next one starts
        // after the padding.
        let padded = u64::from(length).div_ceil(8) * 8;
        offset = offset
            .checked_add(8 + padded)
            .filter(|end| *end <= file_length)
            .ok_or_else(|| {
                Qcow2Error::Malformed(format!(
                    "a header extension of type {kind:#010x} runs past the end of the file"
                ))
            })?;
        log::debug!("skipping the {length}-byte header extension of type {kind:#010x}");
    }

    Err(Qcow2Error::Malformed(format!(
        "the header carries more than {MAX_HEADER_EXTENSIONS} extensions"
    )))
}

/// Reads exactly `buffer.len()` bytes from `offset`, saying what was being read
/// if the file ends first.
fn read_at(
    reader: &mut (impl Read + Seek),
    offset: u64,
    buffer: &mut [u8],
    what: &'static str,
) -> Result<(), Qcow2Error> {
    reader
        .seek(SeekFrom::Start(offset))
        .and_then(|_| reader.read_exact(buffer))
        .map_err(|source| {
            Qcow2Error::Malformed(format!("{what} could not be read at {offset}: {source}"))
        })
}

fn be_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn be_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{Compression, inspect};
    use crate::error::Qcow2Error;

    const CLUSTER: usize = 4096;
    /// Three clusters: the header, the L1 table, and one cluster of contents.
    const FILE_LENGTH: usize = 3 * CLUSTER;
    /// Eight clusters of guest disk, which one L1 entry is enough to describe.
    const VIRTUAL_SIZE: u64 = 8 * CLUSTER as u64;
    const CAPACITY: u64 = 1024 * 1024;

    /// A version 3 header this module accepts, as the bytes of a whole small
    /// file. Every test that refuses an image starts from this and changes one
    /// field, so what the test is about is the line that changes.
    fn valid_image() -> Vec<u8> {
        let mut bytes = vec![0u8; FILE_LENGTH];
        bytes[..4].copy_from_slice(b"QFI\xfb");
        put_u32(&mut bytes, 4, 3); // version
        put_u32(&mut bytes, 20, 12); // cluster_bits: 4096-byte clusters
        put_u64(&mut bytes, 24, VIRTUAL_SIZE);
        put_u32(&mut bytes, 36, 1); // l1_size
        put_u64(&mut bytes, 40, CLUSTER as u64); // l1_table_offset
        put_u64(&mut bytes, 48, 2 * CLUSTER as u64); // refcount_table_offset
        put_u32(&mut bytes, 56, 1); // refcount_table_clusters
        put_u32(&mut bytes, 96, 4); // refcount_order
        put_u32(&mut bytes, 100, 112); // header_length
        bytes
    }

    fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
        bytes[at..at + 8].copy_from_slice(&value.to_be_bytes());
    }

    /// Runs the check over a whole file's bytes, as `Qcow2Image::open` does.
    fn check(bytes: &[u8]) -> Result<super::HeaderFacts, Qcow2Error> {
        check_against(bytes, CAPACITY)
    }

    fn check_against(bytes: &[u8], capacity: u64) -> Result<super::HeaderFacts, Qcow2Error> {
        inspect(&mut Cursor::new(bytes), bytes.len() as u64, capacity)
    }

    #[test]
    fn a_header_qemu_would_write_is_described_rather_than_refused() {
        let facts = check(&valid_image()).expect("this is the header qemu writes");

        assert_eq!(facts.version, 3);
        assert_eq!(facts.virtual_size, VIRTUAL_SIZE);
        assert_eq!(facts.cluster_size, CLUSTER as u64);
        assert_eq!(facts.compression, Compression::Zlib);
    }

    #[test]
    fn a_version_2_image_needs_no_feature_bitmaps_at_all() {
        let mut bytes = valid_image();
        put_u32(&mut bytes, 4, 2);
        // Where a version 3 image keeps its feature bitmaps, a version 2 image
        // keeps its header extensions, so the end marker moves up to offset 72.
        put_u64(&mut bytes, 72, 0);

        let facts = check(&bytes).expect("compat=0.10 images are still published");
        assert_eq!(facts.version, 2);
        assert_eq!(facts.compression, Compression::Zlib);
    }

    #[test]
    fn the_legacy_version_is_refused_by_number() {
        let mut bytes = valid_image();
        put_u32(&mut bytes, 4, 1);

        assert!(matches!(
            check(&bytes),
            Err(Qcow2Error::UnsupportedVersion { version: 1 })
        ));
    }

    #[test]
    fn a_version_from_the_future_is_refused_rather_than_guessed_at() {
        let mut bytes = valid_image();
        put_u32(&mut bytes, 4, 4);

        assert!(matches!(
            check(&bytes),
            Err(Qcow2Error::UnsupportedVersion { version: 4 })
        ));
    }

    #[test]
    fn a_file_that_is_not_a_qcow_is_refused_on_its_first_four_bytes() {
        let mut bytes = valid_image();
        bytes[..4].copy_from_slice(b"\x1f\x8b\x08\x00");

        assert!(matches!(check(&bytes), Err(Qcow2Error::Malformed(_))));
    }

    #[test]
    fn a_file_too_short_to_hold_a_header_is_refused_before_it_is_indexed() {
        let bytes = valid_image()[..40].to_vec();

        let error = check(&bytes).expect_err("forty bytes is not a header");
        assert!(
            matches!(&error, Qcow2Error::Malformed(message) if message.contains("shorter")),
            "got {error}"
        );
    }

    /// The bit that says "the compression type field means something", which is
    /// the one incompatible feature this reader implements.
    #[test]
    fn the_zstd_compression_bit_is_the_one_incompatible_feature_that_is_accepted() {
        let mut bytes = valid_image();
        put_u64(&mut bytes, 72, 1 << 3);
        bytes[104] = 1;

        assert_eq!(
            check(&bytes)
                .expect("zstd clusters are decoded")
                .compression,
            Compression::Zstd
        );
    }

    #[test]
    fn each_incompatible_feature_this_reader_does_not_implement_is_refused() {
        // Bit 0 dirty, 1 corrupt, 2 external data file, 4 extended L2 entries,
        // and a bit from the reserved range that no version of qemu has used.
        for bit in [0u32, 1, 2, 4, 40] {
            let mut bytes = valid_image();
            put_u64(&mut bytes, 72, 1 << bit);

            let error = check(&bytes).expect_err("this reader implements none of these");
            assert!(
                matches!(error, Qcow2Error::UnsupportedFeatures { bits } if bits == 1 << bit),
                "bit {bit} gave {error}"
            );
        }
    }

    #[test]
    fn a_compatible_or_autoclear_bit_does_not_stand_in_the_way_of_reading() {
        let mut bytes = valid_image();
        put_u64(&mut bytes, 80, 1); // lazy refcounts
        put_u64(&mut bytes, 88, 1); // bitmaps extension

        assert!(
            check(&bytes).is_ok(),
            "the spec lets a reader ignore both, and this reader never writes"
        );
    }

    #[test]
    fn a_compression_type_and_its_feature_bit_must_agree() {
        let mut bytes = valid_image();
        bytes[104] = 1;

        let error =
            check(&bytes).expect_err("zstd without the bit set is a file lying about itself");
        assert!(matches!(error, Qcow2Error::Malformed(_)), "got {error}");
    }

    #[test]
    fn a_codec_with_no_decoder_behind_it_is_refused_by_number() {
        let mut bytes = valid_image();
        put_u64(&mut bytes, 72, 1 << 3);
        bytes[104] = 7;

        assert!(matches!(
            check(&bytes),
            Err(Qcow2Error::UnsupportedCompression {
                compression_type: 7
            })
        ));
    }

    #[test]
    fn a_header_of_the_minimum_version_3_length_carries_no_compression_type() {
        let mut bytes = valid_image();
        // Shortening the header moves the extensions up to 104, which is where
        // the compression type would have been. A reader that read the field
        // anyway would be reading the first byte of a header extension.
        put_u32(&mut bytes, 100, 104);

        assert_eq!(
            check(&bytes)
                .expect("104 is a complete version 3 header")
                .compression,
            Compression::Zlib
        );
    }

    #[test]
    fn an_encrypted_image_is_refused_with_the_method_it_names() {
        for method in [1u32, 2] {
            let mut bytes = valid_image();
            put_u32(&mut bytes, 32, method);

            assert!(matches!(
                check(&bytes),
                Err(Qcow2Error::Encrypted { crypt_method }) if crypt_method == method
            ));
        }
    }

    #[test]
    fn an_overlay_is_refused_whether_it_names_its_parent_or_only_points_at_it() {
        let mut named = valid_image();
        put_u64(&mut named, 8, 512);
        put_u32(&mut named, 16, 20);
        assert!(matches!(check(&named), Err(Qcow2Error::BackingFile)));

        let mut length_only = valid_image();
        put_u32(&mut length_only, 16, 20);
        assert!(
            matches!(check(&length_only), Err(Qcow2Error::BackingFile)),
            "a length without an offset is still an image describing a parent"
        );
    }

    #[test]
    fn internal_snapshots_are_refused_with_their_count() {
        let mut bytes = valid_image();
        put_u32(&mut bytes, 60, 3);

        assert!(matches!(
            check(&bytes),
            Err(Qcow2Error::Snapshots { count: 3 })
        ));
    }

    #[test]
    fn a_cluster_size_outside_what_the_format_allows_is_refused() {
        for cluster_bits in [0u32, 8, 22, 63] {
            let mut bytes = valid_image();
            put_u32(&mut bytes, 20, cluster_bits);

            let error = check(&bytes).expect_err("none of these is a cluster size");
            assert!(
                matches!(error, Qcow2Error::UnsupportedClusterSize { cluster_bits: bits }
                    if bits == cluster_bits),
                "2^{cluster_bits} gave {error}"
            );
        }
    }

    #[test]
    fn a_disk_larger_than_the_one_it_is_headed_for_is_refused_before_any_content_is_read() {
        let bytes = valid_image();

        let error = check_against(&bytes, VIRTUAL_SIZE - 1)
            .expect_err("the last byte would have nowhere to go");
        assert!(
            matches!(error, Qcow2Error::TooLarge { virtual_size, capacity }
                if virtual_size == VIRTUAL_SIZE && capacity == VIRTUAL_SIZE - 1),
            "got {error}"
        );
        assert!(
            check_against(&bytes, VIRTUAL_SIZE).is_ok(),
            "a disk that fits exactly fits"
        );
    }

    #[test]
    fn an_l1_table_too_small_to_describe_the_disk_is_a_contradiction() {
        let mut bytes = valid_image();
        put_u32(&mut bytes, 36, 0);

        let error = check(&bytes).expect_err("no L1 entries means no addressable disk");
        assert!(
            matches!(&error, Qcow2Error::Malformed(message) if message.contains("too few")),
            "got {error}"
        );
    }

    #[test]
    fn an_l1_table_of_billions_of_entries_is_refused_rather_than_allocated() {
        let mut bytes = valid_image();
        put_u32(&mut bytes, 36, u32::MAX);

        let error = check(&bytes).expect_err("this is four bytes to write and 64 GiB to open");
        assert!(matches!(error, Qcow2Error::Malformed(_)), "got {error}");
    }

    #[test]
    fn an_l1_table_the_file_does_not_contain_is_refused() {
        let mut bytes = valid_image();
        put_u64(&mut bytes, 40, 64 * CLUSTER as u64);

        let error = check(&bytes).expect_err("the table lies past the end of the file");
        assert!(
            matches!(&error, Qcow2Error::Malformed(message) if message.contains("past the end")),
            "got {error}"
        );
    }

    #[test]
    fn an_l1_table_off_a_cluster_boundary_is_refused() {
        let mut bytes = valid_image();
        put_u64(&mut bytes, 40, CLUSTER as u64 + 8);

        let error = check(&bytes).expect_err("the format requires the alignment");
        assert!(
            matches!(&error, Qcow2Error::Malformed(message) if message.contains("boundary")),
            "got {error}"
        );
    }

    #[test]
    fn a_header_extension_is_skipped_over_and_its_length_bounded() {
        let mut bytes = valid_image();
        put_u32(&mut bytes, 112, 0xe2792aca); // backing file format
        put_u32(&mut bytes, 116, 5);
        bytes[120..125].copy_from_slice(b"qcow2");
        assert!(
            check(&bytes).is_ok(),
            "an extension this reader has no use for is padding to step over"
        );

        put_u32(&mut bytes, 116, 1024 * 1024);
        let error = check(&bytes).expect_err("a megabyte is not a header extension");
        assert!(matches!(error, Qcow2Error::Malformed(_)), "got {error}");
    }

    #[test]
    fn extensions_that_never_end_are_refused_rather_than_walked_forever() {
        let mut bytes = valid_image();
        for offset in (112..FILE_LENGTH - 8).step_by(8) {
            put_u32(&mut bytes, offset, 0xdeadbeef);
            put_u32(&mut bytes, offset + 4, 0);
        }

        let error = check(&bytes).expect_err("no end marker is ever reached");
        assert!(matches!(error, Qcow2Error::Malformed(_)), "got {error}");
    }
}
