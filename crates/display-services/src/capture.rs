//! A framebuffer the broker exported, mapped so the encoder can read it.
//!
//! The mapping is `PROT_READ` over a descriptor the broker exported without
//! `DRM_RDWR`, so this process could not write the desktop even if it tried.

use std::{
    io::{self, ErrorKind},
    os::fd::{AsRawFd, BorrowedFd},
    ptr, slice,
    sync::atomic::{AtomicBool, Ordering},
};

use vmlord_display_codec::{PixelFormat, Rect};

use crate::drm::{
    sync_buffer,
    uapi::{DMA_BUF_SYNC_END, DMA_BUF_SYNC_READ, DMA_BUF_SYNC_START},
};

/// A read-only mapping over an exported buffer.
///
/// The mapping lives as long as the value; `Drop` unmaps it. The descriptor is
/// not kept: `mmap` holds its own reference to the object, so the caller may
/// close it.
pub struct MappedBuffer {
    /// The mapping's base address. Never null: `map` fails instead.
    address: ptr::NonNull<u8>,
    /// Its length in bytes, which is what `read` hands out.
    length: usize,
    /// The descriptor, kept only for the coherency calls.
    descriptor: libc::c_int,
    /// Whether a failed sync has already been reported. A buffer that does not
    /// implement the call fails on every frame, and a warning per frame is a
    /// log nobody reads.
    sync_reported: AtomicBool,
}

// SAFETY: the mapping is read-only for the life of the value and the only
// interior mutability is an atomic flag, so a shared reference may cross
// threads.
unsafe impl Send for MappedBuffer {}
// SAFETY: as above.
unsafe impl Sync for MappedBuffer {}

impl MappedBuffer {
    /// Maps `length` bytes of `descriptor` for reading.
    ///
    /// The descriptor's own size is checked first: a mapping that runs past the
    /// end of its object is one whose pages fault on access, and a SIGBUS
    /// inside the encoder is not a failure anyone can diagnose.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the descriptor cannot be measured, is shorter than
    /// `length`, or the mapping fails.
    pub fn map(descriptor: BorrowedFd<'_>, length: usize) -> io::Result<Self> {
        if length == 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "a framebuffer of no bytes is not something to map",
            ));
        }

        let raw = descriptor.as_raw_fd();
        let mut status: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `status` is a live, correctly shaped value and `raw` is a
        // descriptor the caller owns for the duration of the call.
        if unsafe { libc::fstat(raw, &raw mut status) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let size = u64::try_from(status.st_size).unwrap_or(0);
        if size < length as u64 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("a buffer of {size} bytes cannot back a mapping of {length}"),
            ));
        }

        // SAFETY: a null hint asks the kernel to choose the address, and the
        // length is one the object has just been shown to cover.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                raw,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let Some(address) = ptr::NonNull::new(address.cast::<u8>()) else {
            return Err(io::Error::other("mmap returned a null mapping"));
        };

        Ok(Self {
            address,
            length,
            descriptor: raw,
            sync_reported: AtomicBool::new(false),
        })
    }

    /// The mapping's length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.length
    }

    /// Whether the mapping covers no bytes, which [`MappedBuffer::map`] refuses
    /// to produce.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Runs `body` over the mapped bytes, with the buffer's caches made
    /// coherent around it.
    ///
    /// A descriptor that does not implement the coherency call -- anything that
    /// is not a dma-buf, which is what the tests hand it -- is read anyway: the
    /// call is an optimisation the kernel offers, not a precondition, and
    /// dropping the desktop over it would be worse than a stale cache line.
    pub fn read<T>(&self, body: impl FnOnce(&[u8]) -> T) -> T {
        self.sync(DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ);
        // SAFETY: the mapping is live for the life of `self`, covers `length`
        // bytes by construction, and is read-only, so no other reference can
        // write it while this slice exists.
        let bytes = unsafe { slice::from_raw_parts(self.address.as_ptr(), self.length) };
        let value = body(bytes);
        self.sync(DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);

        value
    }

    /// One half of the coherency bracket, reported at most once.
    fn sync(&self, flags: u64) {
        if sync_buffer(self.descriptor, flags).is_err()
            && !self.sync_reported.swap(true, Ordering::Relaxed)
        {
            eprintln!(
                "vmlord-display: buffer does not implement DMA_BUF_IOCTL_SYNC; reading it uncached"
            );
        }
    }
}

impl Drop for MappedBuffer {
    fn drop(&mut self) {
        // SAFETY: the address and length are the ones `mmap` returned and this
        // is the only owner of the mapping.
        unsafe {
            libc::munmap(self.address.as_ptr().cast::<libc::c_void>(), self.length);
        }
    }
}

/// Where a captured frame's pixels live.
///
/// An enum rather than a bare field, because this is also where a backing that
/// is handed on without ever being mapped goes.
pub enum Backing {
    /// A mapping over a dma-buf the broker exported. What a guest actually
    /// captures.
    Cpu(MappedBuffer),
    /// Pixels this process owns. What tests and, one day, a software fallback
    /// hand to the pipeline; the encoder cannot tell the difference.
    Owned(Vec<u8>),
}

/// One frame, as capture produced it.
pub struct CapturedFrame {
    /// The vblank sequence this frame was read at.
    pub sequence: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per row, which is not promised to be `width * 4`.
    pub stride: u32,
    /// How the pixels are laid out.
    pub format: PixelFormat,
    /// What changed since the last frame, when the source can say.
    ///
    /// Always `None` here: this module has no damage source, and the encoder
    /// treats `None` as "compare the tiles". A later task fills it from the
    /// module's own damage reporting.
    pub damage: Option<Vec<Rect>>,
    /// The pixels themselves.
    pub backing: Backing,
}

impl CapturedFrame {
    /// A frame whose pixels this process owns.
    ///
    /// The pipeline's tests are its consumer, and so is a software fallback
    /// that never touches DRM. It is not a test-only constructor: a variant
    /// behind `#[cfg(test)]` would mean the tested path is not the shipped one.
    #[must_use]
    pub const fn from_pixels(
        sequence: u64,
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
        pixels: Vec<u8>,
    ) -> Self {
        Self {
            sequence,
            width,
            height,
            stride,
            format,
            damage: None,
            backing: Backing::Owned(pixels),
        }
    }

    /// Runs `body` over the frame's pixels.
    ///
    /// A mapped frame is bracketed with the buffer's coherency calls; an owned
    /// one needs none, and the caller cannot tell which it got.
    pub fn read<T>(&self, body: impl FnOnce(&[u8]) -> T) -> T {
        match &self.backing {
            Backing::Cpu(mapped) => mapped.read(body),
            Backing::Owned(pixels) => body(pixels),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use super::MappedBuffer;
    use crate::unix::memfd;

    #[test]
    fn a_mapped_buffer_reads_what_the_descriptor_holds() {
        let fd = memfd("frame", &[0xab; 4096]).unwrap();
        let mapped = MappedBuffer::map(fd.as_fd(), 4096).unwrap();

        mapped.read(|bytes| {
            assert_eq!(bytes.len(), 4096);
            assert!(bytes.iter().all(|byte| *byte == 0xab));
        });
    }

    #[test]
    fn a_descriptor_that_cannot_sync_is_still_readable() {
        // A memfd is not a dma-buf and answers DMA_BUF_IOCTL_SYNC with ENOTTY.
        // A cache-coherency call that a buffer does not implement is not a
        // reason to drop the desktop, and this is the case that proves the
        // read still happens.
        let fd = memfd("frame", &[1; 64]).unwrap();
        let mapped = MappedBuffer::map(fd.as_fd(), 64).unwrap();

        assert_eq!(mapped.read(|bytes| bytes[0]), 1);
    }

    #[test]
    fn mapping_past_the_end_of_a_descriptor_fails_rather_than_faulting() {
        let fd = memfd("small", &[0; 8]).unwrap();
        // A length beyond the file is a mapping whose pages fault on access,
        // which must be refused here rather than discovered as a SIGBUS in the
        // encoder.
        assert!(MappedBuffer::map(fd.as_fd(), 1 << 20).is_err());
    }
}
