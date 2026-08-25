//! Turning a distribution release into a URL and the checksum to expect.

use vmlord_core::DistroProfile;

use crate::{
    checksums::parse_sha256sums, distro::validated_release, error::ResolveError, http::build_agent,
};

/// The largest checksum file that will be read into memory.
///
/// Without a ceiling a server is free to answer this request with a gigabyte,
/// straight into the worker thread's memory. The real file is under eight
/// kilobytes.
const MAX_CHECKSUMS_BYTES: u64 = 1024 * 1024;

/// Where to get a release's image, what it must hash to, and what the guest
/// inside it looks like.
///
/// `sha256` is lowercase hex, which is what `ImageDownloadRequest` expects, so
/// the caller feeds one into the other without a converter in between.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedImage {
    pub url: String,
    pub sha256: String,
    pub default_user: String,
    pub admin_group: String,
}

/// Works out which image a release means, by reading the checksum file the
/// distribution publishes beside it.
///
/// One request of a few kilobytes, so there is no progress reporting and no
/// cancellation flag here -- both belong to the download that follows. The list
/// is deliberately not cached: it is the thing that says what is current, and a
/// month-old copy would point at a build that has been withdrawn.
pub fn resolve_image(
    profile: &DistroProfile,
    release: &str,
) -> Result<ResolvedImage, ResolveError> {
    let release = validated_release(release).inspect_err(|error| tracing::error!("{error}"))?;
    let file_name = profile.file_name(release);
    let checksums_url = profile.checksums_url(release);

    tracing::debug!("looking up {file_name} in {checksums_url}");
    let published = fetch_text(&checksums_url).inspect_err(|error| tracing::error!("{error}"))?;
    let sha256 = parse_sha256sums(&published, &file_name, &checksums_url)
        .inspect_err(|error| tracing::error!("{error}"))?;

    let url = profile.image_url(release);
    tracing::info!("{} {release} resolves to {url} ({sha256})", profile.name);
    Ok(ResolvedImage {
        url,
        sha256,
        default_user: profile.default_user.clone(),
        admin_group: profile.admin_group.clone(),
    })
}

/// Fetches a small text file, refusing a body too large to be one.
fn fetch_text(url: &str) -> Result<String, ResolveError> {
    let agent = build_agent();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|source| ResolveError::Http(format!("requesting {url} failed: {source}")))?;

    let status = response.status().as_u16();
    if status != 200 {
        return Err(ResolveError::UnexpectedStatus { status });
    }

    response
        .body_mut()
        .with_config()
        .limit(MAX_CHECKSUMS_BYTES)
        .read_to_string()
        .map_err(|source| ResolveError::Http(format!("reading {url} failed: {source}")))
}
