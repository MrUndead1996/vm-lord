//! Splitting an image into chunks, and telling a hole from data.
//!
//! Nothing here touches Windows. It is the arithmetic the importer runs on
//! every megabyte of every image, and it is the part that can be tested without
//! a Hyper-V host.

use std::io::Read;

/// The unit every read, write and verification works in.
///
/// A multiple of every qcow2 cluster size in practice (64 KiB and below), so a
/// chunk never lands mid-cluster, and a multiple of [`SECTOR_ALIGNMENT`], so a
/// full chunk needs no padding to reach the disk.
pub(crate) const CHUNK_BYTES: usize = 1024 * 1024;

/// What `FILE_FLAG_NO_BUFFERING` demands of every offset, length and buffer
/// address.
///
/// The requirement is the volume's sector size, which is 512 or 4096 on the
/// disks Hyper-V presents. 4096 is a multiple of both, so rounding to it
/// satisfies either without asking the disk which one it is.
pub(crate) const SECTOR_ALIGNMENT: usize = 4096;

/// Fills `buffer` from `source`, returning how many bytes arrived.
///
/// A short read is not the end of the stream: [`Qcow2Image`] serves one cluster
/// per call however large the buffer is, so a single `read` would leave chunks
/// the size of a cluster and multiply the number of writes by sixteen. Only a
/// read of zero bytes ends the image.
///
/// [`Qcow2Image`]: https://docs.rs/vmlord-image
pub(crate) fn fill_chunk(source: &mut dyn Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match source.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

/// Whether a chunk is a hole: bytes the guest would read as zeros anyway.
///
/// Skipping these is what keeps the VHDX sparse. The reader hands out zeros for
/// every cluster the image never allocated, and writing them back would
/// allocate the whole disk -- a 600 MB image would land as a 64 GB file.
pub(crate) fn is_zero(chunk: &[u8]) -> bool {
    // Eight bytes at a time: this runs over every byte of every image, and a
    // byte-at-a-time `all()` over a megabyte is the slower half of a copy that
    // skips most of what it reads.
    let words = chunk.chunks_exact(8);
    let tail = words.remainder();
    words.into_iter().all(|word| word == [0; 8]) && tail.iter().all(|byte| *byte == 0)
}

/// Rounds a chunk's length up to what an unbuffered write will accept.
///
/// Only the last chunk of an image is ever short, and the bytes past its end
/// are ones the disk already reads as zeros, so writing a few hundred extra
/// zeros changes nothing the guest can see.
pub(crate) fn padded_length(used: usize, alignment: usize) -> usize {
    used.div_ceil(alignment) * alignment
}

/// A 64-bit FNV-1a hash of a chunk, kept so the chunk can be recognised when it
/// is read back off the disk without holding the whole image in memory.
///
/// It is a check against a write that did not land, not against an adversary:
/// the failure it exists for writes zeros, or writes nothing at all, and either
/// is caught by any hash at all.
pub(crate) fn digest(chunk: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in chunk {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};

    use super::{CHUNK_BYTES, SECTOR_ALIGNMENT, digest, fill_chunk, is_zero, padded_length};

    /// Serves at most `limit` bytes per call, the way the qcow2 reader serves at
    /// most one cluster.
    struct Dribble {
        data: Vec<u8>,
        position: usize,
        limit: usize,
    }

    impl Read for Dribble {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let wanted = buffer
                .len()
                .min(self.limit)
                .min(self.data.len() - self.position);
            buffer[..wanted].copy_from_slice(&self.data[self.position..self.position + wanted]);
            self.position += wanted;
            Ok(wanted)
        }
    }

    #[test]
    fn a_chunk_is_filled_from_as_many_short_reads_as_it_takes() {
        let mut source = Dribble {
            data: (0..=255u8).cycle().take(4096).collect(),
            position: 0,
            limit: 7,
        };
        let mut buffer = [0u8; 4096];

        let filled = fill_chunk(&mut source, &mut buffer).unwrap();

        assert_eq!(filled, 4096);
        assert_eq!(buffer.as_slice(), source.data.as_slice());
    }

    #[test]
    fn the_last_chunk_of_an_image_comes_back_short() {
        let mut source = Dribble {
            data: vec![1; 100],
            position: 0,
            limit: 64,
        };
        let mut buffer = [7u8; 4096];

        let filled = fill_chunk(&mut source, &mut buffer).unwrap();

        assert_eq!(filled, 100);
        assert!(buffer[..100].iter().all(|byte| *byte == 1));
        assert_eq!(buffer[100], 7, "the tail of the buffer is left untouched");
    }

    #[test]
    fn an_exhausted_source_fills_nothing() {
        let mut source = Dribble {
            data: Vec::new(),
            position: 0,
            limit: 64,
        };
        let mut buffer = [0u8; 16];

        assert_eq!(fill_chunk(&mut source, &mut buffer).unwrap(), 0);
    }

    #[test]
    fn a_read_error_is_not_mistaken_for_the_end_of_the_image() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("the image could not be read"))
            }
        }
        let mut buffer = [0u8; 16];

        let error = fill_chunk(&mut Broken, &mut buffer).unwrap_err();

        assert!(error.to_string().contains("could not be read"));
    }

    #[test]
    fn a_hole_is_recognised_at_every_alignment_of_the_buffer() {
        let zeros = vec![0u8; CHUNK_BYTES];

        // A word-at-a-time scan leaves a remainder whose size depends on where
        // the slice starts, and the remainder is the part that gets forgotten.
        for start in 0..8 {
            assert!(
                is_zero(&zeros[start..]),
                "offset {start} should read as a hole"
            );
        }
        assert!(is_zero(&[]));
    }

    #[test]
    fn a_single_set_byte_anywhere_makes_a_chunk_data() {
        for position in [0, 1, 7, 8, 4095, CHUNK_BYTES - 1] {
            let mut chunk = vec![0u8; CHUNK_BYTES];
            chunk[position] = 1;
            assert!(!is_zero(&chunk), "a byte set at {position} should be data");
            if position > 0 {
                assert!(
                    !is_zero(&chunk[1..]),
                    "a byte set at {position} should be data at an odd alignment too"
                );
            }
        }
    }

    #[test]
    fn a_short_chunk_is_padded_up_to_a_sector_and_a_full_one_is_left_alone() {
        assert_eq!(padded_length(0, SECTOR_ALIGNMENT), 0);
        assert_eq!(padded_length(1, SECTOR_ALIGNMENT), SECTOR_ALIGNMENT);
        assert_eq!(
            padded_length(SECTOR_ALIGNMENT, SECTOR_ALIGNMENT),
            SECTOR_ALIGNMENT
        );
        assert_eq!(
            padded_length(SECTOR_ALIGNMENT + 1, SECTOR_ALIGNMENT),
            SECTOR_ALIGNMENT * 2
        );
        assert_eq!(padded_length(CHUNK_BYTES, SECTOR_ALIGNMENT), CHUNK_BYTES);
    }

    #[test]
    fn a_chunk_that_came_back_as_zeros_does_not_match_its_digest() {
        let written = vec![0xa5u8; CHUNK_BYTES];
        let read_back = vec![0u8; CHUNK_BYTES];

        assert_ne!(digest(&written), digest(&read_back));
    }

    #[test]
    fn a_digest_notices_a_single_flipped_byte_and_a_truncated_read() {
        let written = vec![0xa5u8; 4096];
        let mut flipped = written.clone();
        flipped[2048] = 0xa4;

        assert_ne!(digest(&written), digest(&flipped));
        assert_ne!(digest(&written), digest(&written[..2048]));
        assert_eq!(digest(&written), digest(&written.clone()));
    }
}
