//! What a payload preparation reports while it works.
//!
//! Every stage is local: bytes are hashed, expanded and staged from a file
//! this build ships beside its executable. Nothing here describes a transfer,
//! because there is no longer one to describe.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadProgress {
    Verifying { hashed: u64, total: u64 },
    Extracting { files: u64, total: u64 },
    Staging { files: u64, total: u64 },
    Ready,
}
