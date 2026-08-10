//! The ISO9660 volume the two documents travel to the guest on.
//!
//! Written by hand rather than taken from a crate: the volume label is the only
//! thing cloud-init has to find the seed by, and a dependency whose control over
//! it is undocumented would have to be re-verified on every update. The format
//! used here is the small, old core of ECMA-119 -- no Joliet, no Rock Ridge, no
//! El Torito -- and the output is checked byte by byte.

/// A tree of nothing but a root is a single 10-byte path table record.
const PATH_TABLE_SIZE: u32 = 10;

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
        PATH_TABLE_SIZE, both_endian_u16, both_endian_u32, directory_record, path_table,
        record_length,
    };

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
}
