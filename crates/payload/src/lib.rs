//! What every VMLord payload is made of, whatever it carries.
//!
//! A payload is an archive a release ships, verified by digest, expanded under
//! limits, cached by content and staged into one VM's directory. None of that
//! knows whether the files inside are a GPU stack or a display one, so none of
//! it lives in a crate that does.
//!
//! Portable by construction: no Windows APIs, no Linux syscalls, no network,
//! and no catalog compiled in. What a payload *is* -- its target, its
//! provenance, what makes one applicable to a guest -- belongs to the crate
//! for that kind of payload, which meets this one at [`PayloadEntry`].

pub mod archive;
#[cfg(feature = "builder")]
pub mod builder;
mod cache;
pub mod catalog;
mod digest;
mod entry;
mod error;
mod marker;
mod prepared;
mod progress;
pub mod release;
mod staging;
#[cfg(test)]
mod test_kind;

pub use cache::{PrepareRequest, ReadyPayload, prepare, prepare_verified_archive};
pub use digest::{Sha256Digest, Sha256Hasher};
pub use entry::{PayloadEntry, PayloadFiles, PayloadSources};
pub use error::PayloadError;
pub use marker::{ReadyMarker, cache_provenance};
pub use prepared::{PreparedFile, validate_path};
pub use progress::PayloadProgress;
pub use staging::{StagedPayload, ensure_staging_root, publish_active, stage_payload};

#[cfg(test)]
mod tests {
    use super::{PayloadError, Sha256Digest};

    #[test]
    fn an_unsupported_target_is_named_by_whoever_had_one() {
        let error = PayloadError::UnsupportedTarget("ubuntu 24.04 amd64".into());

        assert!(error.to_string().contains("ubuntu 24.04 amd64"));
    }

    #[test]
    fn a_digest_round_trips_through_its_hex() {
        let digest = Sha256Digest::hash_reader(b"payload".as_slice()).unwrap();

        assert_eq!(
            digest.as_hex().parse::<Sha256Digest>().unwrap().as_hex(),
            digest.as_hex()
        );
    }
}
