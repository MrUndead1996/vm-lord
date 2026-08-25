//! The ISO9660 volume the two documents travel to the guest on.
//!
//! Written by hand rather than taken from a crate: the volume label is the only
//! thing cloud-init has to find the seed by, and a dependency whose control over
//! it is undocumented would have to be re-verified on every update. The format
//! used here is the small, old core of ECMA-119 -- no Joliet, no Rock Ridge, no
//! El Torito -- and the output is checked byte by byte.

/// The logical block size, and the alignment of everything this writer places.
const SECTOR: usize = 2048;

/// The first block of the root directory. Everything before it is fixed.
const ROOT_LBA: u32 = 20;

/// The blocks nobody installs a boot loader in.
const SYSTEM_AREA_SECTORS: u32 = 16;
const LITTLE_PATH_TABLE_LBA: u32 = 18;
const BIG_PATH_TABLE_LBA: u32 = 19;

/// A tree of nothing but a root is a single 10-byte path table record.
const PATH_TABLE_SIZE: u32 = 10;

/// ISO9660 Level 2 caps a file identifier at 30 bytes.
const MAX_IDENTIFIER: usize = 30;

/// Whoever asks the volume who made it.
const PUBLISHER: &str = "VMLORD";

/// Packs `entries` into an ISO9660 image labelled `volume_id`.
///
/// Infallible: this writer does no I/O, and the only caller passes constants,
/// so there is no input to refuse. A name longer than Level 2 allows is a bug in
/// that caller rather than a user's mistake, which is what `debug_assert` is
/// for.
pub(crate) fn build(volume_id: &str, entries: &[(&str, &[u8])]) -> Vec<u8> {
    debug_assert!(volume_id.len() <= 32, "a volume identifier is 32 bytes");
    debug_assert!(
        entries
            .iter()
            .all(|(name, _)| !name.is_empty() && name.len() <= MAX_IDENTIFIER),
        "a file identifier is 1 to {MAX_IDENTIFIER} bytes"
    );

    // The root is laid out first: a record's length depends on its name alone,
    // so the size of the root -- and with it the address of the first file --
    // is known before a single address is.
    let identifiers: Vec<&[u8]> = std::iter::once(&[0x00u8][..])
        .chain(std::iter::once(&[0x01u8][..]))
        .chain(entries.iter().map(|(name, _)| name.as_bytes()))
        .collect();
    let root_sectors = sectors_for_records(&identifiers);
    let root_length = root_sectors * SECTOR as u32;

    let mut lba = ROOT_LBA + root_sectors;
    let addresses: Vec<u32> = entries
        .iter()
        .map(|(_, content)| {
            let address = lba;
            lba += sectors_for(content.len());
            address
        })
        .collect();
    let total_sectors = lba;

    tracing::debug!(
        "packing an ISO9660 image: volume \"{volume_id}\", {} files, {total_sectors} sectors",
        entries.len()
    );

    let root_record = directory_record(&[0x00], ROOT_LBA, root_length, true);
    let mut records = vec![
        root_record.clone(),
        directory_record(&[0x01], ROOT_LBA, root_length, true),
    ];
    for ((name, content), address) in entries.iter().zip(&addresses) {
        let length = u32::try_from(content.len()).expect("a file is under 4 GiB");
        records.push(directory_record(name.as_bytes(), *address, length, false));
    }

    let mut image = vec![0u8; SYSTEM_AREA_SECTORS as usize * SECTOR];
    image.extend_from_slice(&primary_volume_descriptor(
        volume_id,
        total_sectors,
        &root_record,
    ));
    image.extend_from_slice(&terminator());
    image.extend_from_slice(&sector(&path_table(ROOT_LBA, false)));
    image.extend_from_slice(&sector(&path_table(ROOT_LBA, true)));
    image.extend_from_slice(&pack_records(&records, root_length as usize));
    for (_, content) in entries {
        image.extend_from_slice(content);
        image.resize(image.len().next_multiple_of(SECTOR), 0);
    }

    debug_assert_eq!(image.len(), total_sectors as usize * SECTOR);
    image
}

/// How many sectors a run of bytes occupies.
fn sectors_for(length: usize) -> u32 {
    u32::try_from(length.div_ceil(SECTOR)).expect("a file is under 4 GiB")
}

/// How many sectors the directory records need.
///
/// A record may not straddle a sector boundary, so one that does not fit leaves
/// the rest of its sector as padding and starts the next.
fn sectors_for_records(identifiers: &[&[u8]]) -> u32 {
    let mut sectors = 1;
    let mut used = 0;
    for identifier in identifiers {
        let length = record_length(identifier.len());
        if used + length > SECTOR {
            sectors += 1;
            used = 0;
        }
        used += length;
    }
    sectors
}

/// Lays the records out by the same rule their size was measured with.
fn pack_records(records: &[Vec<u8>], length: usize) -> Vec<u8> {
    let mut extent = vec![0u8; length];
    let mut cursor = 0;
    for record in records {
        if cursor % SECTOR + record.len() > SECTOR {
            cursor = (cursor / SECTOR + 1) * SECTOR;
        }
        extent[cursor..cursor + record.len()].copy_from_slice(record);
        cursor += record.len();
    }
    extent
}

/// Puts a short run of bytes alone in a sector.
fn sector(content: &[u8]) -> Vec<u8> {
    let mut block = vec![0u8; SECTOR];
    block[..content.len()].copy_from_slice(content);
    block
}

/// The descriptor a reader mounts the volume by.
fn primary_volume_descriptor(volume_id: &str, total_sectors: u32, root: &[u8]) -> Vec<u8> {
    let mut descriptor = vec![0u8; SECTOR];
    descriptor[0] = 1;
    descriptor[1..6].copy_from_slice(b"CD001");
    descriptor[6] = 1;
    text(&mut descriptor[8..40], "");
    text(&mut descriptor[40..72], volume_id);
    descriptor[80..88].copy_from_slice(&both_endian_u32(total_sectors));
    descriptor[120..124].copy_from_slice(&both_endian_u16(1));
    descriptor[124..128].copy_from_slice(&both_endian_u16(1));
    descriptor[128..132].copy_from_slice(&both_endian_u16(
        u16::try_from(SECTOR).expect("the sector size fits in a u16"),
    ));
    descriptor[132..140].copy_from_slice(&both_endian_u32(PATH_TABLE_SIZE));
    // The path table addresses are the one pair of fields ECMA-119 stores as
    // two separate single-order values rather than as one both-endian field.
    descriptor[140..144].copy_from_slice(&LITTLE_PATH_TABLE_LBA.to_le_bytes());
    descriptor[148..152].copy_from_slice(&BIG_PATH_TABLE_LBA.to_be_bytes());
    descriptor[156..156 + root.len()].copy_from_slice(root);
    text(&mut descriptor[190..318], "");
    text(&mut descriptor[318..446], PUBLISHER);
    text(&mut descriptor[446..574], PUBLISHER);
    text(&mut descriptor[574..702], "");
    text(&mut descriptor[702..739], "");
    text(&mut descriptor[739..776], "");
    text(&mut descriptor[776..813], "");
    for field in [813..830, 830..847, 847..864, 864..881] {
        undated(&mut descriptor[field]);
    }
    descriptor[881] = 1;
    descriptor
}

/// The descriptor that ends the set.
fn terminator() -> Vec<u8> {
    let mut descriptor = vec![0u8; SECTOR];
    descriptor[0] = 0xFF;
    descriptor[1..6].copy_from_slice(b"CD001");
    descriptor[6] = 1;
    descriptor
}

/// Writes a text field, space-padded the way ECMA-119 pads them.
fn text(field: &mut [u8], value: &str) {
    field.fill(b' ');
    field[..value.len()].copy_from_slice(value.as_bytes());
}

/// Writes the date form that means "not specified": sixteen digit zeros and a
/// zero offset from Greenwich.
fn undated(field: &mut [u8]) {
    field[..16].fill(b'0');
    field[16] = 0;
}

/// Records a number the way ECMA-119 wants it: little-endian, then big-endian.
fn both_endian_u32(value: u32) -> [u8; 8] {
    let mut field = [0u8; 8];
    field[..4].copy_from_slice(&value.to_le_bytes());
    field[4..].copy_from_slice(&value.to_be_bytes());
    field
}

/// The same, for the fields that are two bytes wide.
fn both_endian_u16(value: u16) -> [u8; 4] {
    let mut field = [0u8; 4];
    field[..2].copy_from_slice(&value.to_le_bytes());
    field[2..].copy_from_slice(&value.to_be_bytes());
    field
}

/// The length a directory record takes, identifier and padding included.
///
/// Needed on its own because the root has to be laid out before any address in
/// it is known, and a record's length depends on nothing but its name.
fn record_length(identifier_len: usize) -> usize {
    let length = 33 + identifier_len;
    length + length % 2
}

/// Builds one entry of a directory.
///
/// The seven date bytes stay zero, and so do file unit size and interleave gap:
/// this volume is neither dated nor interleaved.
fn directory_record(identifier: &[u8], lba: u32, length: u32, directory: bool) -> Vec<u8> {
    let mut record = vec![0u8; record_length(identifier.len())];
    record[0] = u8::try_from(record.len()).expect("a directory record is at most 63 bytes");
    record[2..10].copy_from_slice(&both_endian_u32(lba));
    record[10..18].copy_from_slice(&both_endian_u32(length));
    record[25] = u8::from(directory) << 1;
    record[28..32].copy_from_slice(&both_endian_u16(1));
    record[32] = u8::try_from(identifier.len()).expect("an identifier is at most 30 bytes");
    record[33..33 + identifier.len()].copy_from_slice(identifier);
    record
}

/// Builds the one path table record a root-only tree needs.
///
/// The root's identifier is a single zero byte and its parent is itself, which
/// is what makes the record the same ten bytes on every volume this writer
/// produces -- only the address and the byte order change.
fn path_table(root_lba: u32, big_endian: bool) -> [u8; PATH_TABLE_SIZE as usize] {
    let mut record = [0u8; PATH_TABLE_SIZE as usize];
    record[0] = 1;
    if big_endian {
        record[2..6].copy_from_slice(&root_lba.to_be_bytes());
        record[6..8].copy_from_slice(&1u16.to_be_bytes());
    } else {
        record[2..6].copy_from_slice(&root_lba.to_le_bytes());
        record[6..8].copy_from_slice(&1u16.to_le_bytes());
    }
    record
}

#[cfg(test)]
mod tests {
    use super::{
        PATH_TABLE_SIZE, SECTOR, both_endian_u16, both_endian_u32, build, directory_record,
        path_table, record_length,
    };

    /// A reader for the writer's own output.
    ///
    /// Deliberately independent of the code that produced it: it walks the
    /// image the way a driver does -- descriptor, root address, records -- so a
    /// test says what the bytes mean rather than what they were meant to be.
    struct Volume<'a> {
        bytes: &'a [u8],
    }

    struct Entry {
        identifier: Vec<u8>,
        lba: u32,
        length: u32,
        directory: bool,
        /// Offset inside the root's extent, kept to prove no record straddles a
        /// sector boundary.
        offset: usize,
    }

    fn le_u32(field: &[u8]) -> u32 {
        u32::from_le_bytes(field[..4].try_into().unwrap())
    }

    fn be_u32(field: &[u8]) -> u32 {
        u32::from_be_bytes(field[..4].try_into().unwrap())
    }

    impl<'a> Volume<'a> {
        fn descriptor(&self) -> &'a [u8] {
            &self.bytes[16 * SECTOR..17 * SECTOR]
        }

        /// Reads a both-endian field and insists the two halves agree.
        fn number(&self, field: &[u8]) -> u32 {
            assert_eq!(le_u32(&field[..4]), be_u32(&field[4..8]), "halves disagree");
            le_u32(&field[..4])
        }

        fn root(&self) -> (u32, u32) {
            let record = &self.descriptor()[156..190];
            (self.number(&record[2..10]), self.number(&record[10..18]))
        }

        fn entries(&self) -> Vec<Entry> {
            let (lba, length) = self.root();
            let start = lba as usize * SECTOR;
            let root = &self.bytes[start..start + length as usize];
            let mut entries = Vec::new();
            let mut cursor = 0;
            while cursor < root.len() {
                let size = usize::from(root[cursor]);
                if size == 0 {
                    // Zero length means the rest of this sector is padding.
                    cursor = (cursor / SECTOR + 1) * SECTOR;
                    continue;
                }
                let record = &root[cursor..cursor + size];
                let identifier_len = usize::from(record[32]);
                entries.push(Entry {
                    identifier: record[33..33 + identifier_len].to_vec(),
                    lba: le_u32(&record[2..6]),
                    length: le_u32(&record[10..14]),
                    directory: record[25] & 0x02 != 0,
                    offset: cursor,
                });
                cursor += size;
            }
            entries
        }

        fn file(&self, name: &str) -> &'a [u8] {
            let entry = self
                .entries()
                .into_iter()
                .find(|entry| entry.identifier == name.as_bytes())
                .unwrap_or_else(|| panic!("the root should list \"{name}\""));
            let start = entry.lba as usize * SECTOR;
            &self.bytes[start..start + entry.length as usize]
        }
    }

    /// ECMA-119 stores a number twice, in both orders, and a reader picks the
    /// half it likes. Both halves must mean the same thing.
    #[test]
    fn a_number_is_recorded_in_both_orders() {
        assert_eq!(both_endian_u32(0x0102_0304), [4, 3, 2, 1, 1, 2, 3, 4]);
        assert_eq!(both_endian_u16(0x0102), [2, 1, 1, 2]);
    }

    /// A directory record must have an even length, so an odd identifier is
    /// followed by a padding byte.
    #[test]
    fn a_record_is_padded_to_an_even_length() {
        assert_eq!(record_length(1), 34);
        assert_eq!(record_length(9), 42);
        assert_eq!(record_length(10), 44);
    }

    #[test]
    fn a_file_record_carries_its_name_address_and_size() {
        let record = directory_record(b"user-data", 21, 4096, false);

        assert_eq!(record.len(), 42);
        assert_eq!(usize::from(record[0]), record.len());
        assert_eq!(record[2..10], both_endian_u32(21));
        assert_eq!(record[10..18], both_endian_u32(4096));
        assert_eq!(record[25], 0x00);
        assert_eq!(record[28..32], both_endian_u16(1));
        assert_eq!(usize::from(record[32]), "user-data".len());
        assert_eq!(&record[33..42], b"user-data");
    }

    /// Bit 1 of the flags is what tells a reader this entry is a directory.
    #[test]
    fn a_directory_record_is_flagged_as_one() {
        let record = directory_record(&[0x00], 20, 2048, true);

        assert_eq!(record.len(), 34);
        assert_eq!(record[25], 0x02);
        assert_eq!(record[33], 0x00);
    }

    /// The date fields are left unset on purpose: nothing reads them, and zeros
    /// keep the same seed producing the same bytes.
    #[test]
    fn a_record_states_no_date() {
        let record = directory_record(b"meta-data", 22, 64, false);

        assert_eq!(record[18..25], [0; 7]);
    }

    #[test]
    fn the_path_table_places_the_root_in_either_order() {
        let little = path_table(20, false);
        let big = path_table(20, true);

        assert_eq!(u32::try_from(little.len()).unwrap(), PATH_TABLE_SIZE);
        assert_eq!(little[0], 1, "the root identifier is one zero byte");
        assert_eq!(little[2..6], 20u32.to_le_bytes());
        assert_eq!(
            little[6..8],
            1u16.to_le_bytes(),
            "the root is its own parent"
        );
        assert_eq!(big[2..6], 20u32.to_be_bytes());
        assert_eq!(big[6..8], 1u16.to_be_bytes());
        assert_eq!(little[8..10], [0, 0], "identifier and pad byte");
    }

    #[test]
    fn an_image_announces_itself_as_iso9660() {
        let bytes = build("CIDATA", &[("user-data", b"#cloud-config\n")]);
        let volume = Volume { bytes: &bytes };

        assert_eq!(&bytes[..16 * SECTOR], &vec![0u8; 16 * SECTOR][..]);
        assert_eq!(volume.descriptor()[0], 1, "a primary volume descriptor");
        assert_eq!(&volume.descriptor()[1..6], b"CD001");
        assert_eq!(volume.descriptor()[6], 1, "descriptor version");
        assert_eq!(volume.descriptor()[881], 1, "file structure version");
        assert_eq!(bytes[17 * SECTOR], 0xFF, "the descriptor set terminator");
        assert_eq!(&bytes[17 * SECTOR + 1..17 * SECTOR + 6], b"CD001");
    }

    /// The label is the whole point: it is the only thing cloud-init has to
    /// find the seed by, and it is padded with spaces to the full field.
    #[test]
    fn the_volume_is_labelled_for_cloud_init() {
        let bytes = build("CIDATA", &[("user-data", b"#cloud-config\n")]);
        let volume = Volume { bytes: &bytes };

        assert_eq!(
            &volume.descriptor()[40..72],
            b"CIDATA                          "
        );
    }

    #[test]
    fn the_recorded_size_matches_the_image() {
        let bytes = build("CIDATA", &[("user-data", b"#cloud-config\n")]);
        let volume = Volume { bytes: &bytes };

        let sectors = volume.number(&volume.descriptor()[80..88]);
        assert_eq!(sectors as usize * SECTOR, bytes.len());
        assert_eq!(
            u16::from_le_bytes(volume.descriptor()[128..130].try_into().unwrap()),
            2048,
            "the logical block size"
        );
    }

    #[test]
    fn both_path_tables_describe_the_same_root() {
        let bytes = build("CIDATA", &[("user-data", b"#cloud-config\n")]);
        let volume = Volume { bytes: &bytes };
        let (root_lba, _) = volume.root();

        assert_eq!(volume.number(&volume.descriptor()[132..140]), 10);
        assert_eq!(le_u32(&volume.descriptor()[140..144]), 18);
        assert_eq!(be_u32(&volume.descriptor()[148..152]), 19);
        let little = &bytes[18 * SECTOR..18 * SECTOR + 10];
        let big = &bytes[19 * SECTOR..19 * SECTOR + 10];
        assert_eq!(le_u32(&little[2..6]), root_lba);
        assert_eq!(be_u32(&big[2..6]), root_lba);
    }

    #[test]
    fn the_root_navigates_to_itself_and_lists_the_files() {
        let bytes = build("CIDATA", &[("user-data", b"one"), ("meta-data", b"two")]);
        let volume = Volume { bytes: &bytes };
        let entries = volume.entries();

        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].identifier, [0x00], "the record for \".\"");
        assert!(entries[0].directory);
        assert_eq!(entries[1].identifier, [0x01], "the record for \"..\"");
        assert!(entries[1].directory);
        assert_eq!(entries[2].identifier, b"user-data");
        assert!(!entries[2].directory);
        assert_eq!(entries[3].identifier, b"meta-data");
        assert_eq!(volume.file("user-data"), b"one");
        assert_eq!(volume.file("meta-data"), b"two");
    }

    /// The one deliberate deviation from ECMA-119, and the reason for it: the
    /// Linux driver passes these bytes through untouched, so what is written is
    /// what cloud-init opens.
    #[test]
    fn a_name_is_written_literally_without_a_version_suffix() {
        let bytes = build("CIDATA", &[("user-data", b"one")]);
        let volume = Volume { bytes: &bytes };
        let entries = volume.entries();
        let entry = &entries[2];

        assert_eq!(entry.identifier, b"user-data");
        assert!(!entry.identifier.contains(&b';'));
        assert!(!entry.identifier.iter().any(u8::is_ascii_uppercase));
    }

    #[test]
    fn every_file_starts_on_a_sector_boundary() {
        let bytes = build("CIDATA", &[("user-data", b"one"), ("meta-data", b"two")]);
        let volume = Volume { bytes: &bytes };

        for entry in volume
            .entries()
            .into_iter()
            .filter(|entry| !entry.directory)
        {
            assert!(entry.lba >= 21, "a file cannot sit in the metadata");
            assert!((entry.lba as usize + 1) * SECTOR <= bytes.len());
        }
    }

    #[test]
    fn a_file_longer_than_a_sector_survives_intact() {
        let long: Vec<u8> = (0..5000u32).map(|byte| byte as u8).collect();
        let bytes = build(
            "CIDATA",
            &[("user-data", long.as_slice()), ("meta-data", b"two")],
        );
        let volume = Volume { bytes: &bytes };

        assert_eq!(volume.file("user-data"), &long[..]);
        assert_eq!(volume.file("meta-data"), b"two");
    }

    /// The agent this transport will one day carry makes the root grow. A
    /// record may not straddle a sector, so the sector is padded and the next
    /// record starts the following one.
    #[test]
    fn a_root_that_outgrows_one_sector_keeps_every_record() {
        let names: Vec<String> = (0..64).map(|index| format!("file-{index:0>24}")).collect();
        let entries: Vec<(&str, &[u8])> = names
            .iter()
            .map(|name| (name.as_str(), name.as_bytes()))
            .collect();
        let bytes = build("CIDATA", &entries);
        let volume = Volume { bytes: &bytes };

        assert!(volume.root().1 as usize > SECTOR, "the root spans sectors");
        assert_eq!(volume.entries().len(), 66);
        for entry in volume.entries() {
            let size = 33 + entry.identifier.len();
            assert!(
                entry.offset % SECTOR + size <= SECTOR,
                "a record straddles a sector"
            );
        }
        for name in &names {
            assert_eq!(volume.file(name), name.as_bytes());
        }
    }

    /// No timestamps anywhere, which is what makes the output reproducible.
    #[test]
    fn the_same_seed_gives_the_same_image() {
        let first = build("CIDATA", &[("user-data", b"one"), ("meta-data", b"two")]);
        let second = build("CIDATA", &[("user-data", b"one"), ("meta-data", b"two")]);

        assert_eq!(first, second);
        let volume = Volume { bytes: &first };
        assert_eq!(&volume.descriptor()[813..829], b"0000000000000000");
        assert_eq!(volume.descriptor()[829], 0);
    }
}
