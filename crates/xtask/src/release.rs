//! The `release-manifest` task: the JSON a release publishes beside its
//! installer, derived from the installer's own bytes.
//!
//! Nothing here trusts an input for a fact it can measure. The size and the
//! digest come from reading the file, never from a workflow variable, and the
//! version comes from the workspace manifest rather than from the tag -- the
//! tag only has to agree with it.

use std::{fs, path::PathBuf};

use semver::Version;
use sha2::{Digest, Sha256};
use vmlord_core::{InstallerAsset, RELEASE_DOWNLOAD_PREFIX, ReleaseManifest};

/// The workspace version this build was compiled from.
///
/// Read at compile time rather than by parsing `Cargo.toml`: every crate in
/// the workspace inherits `version.workspace = true`, so xtask's own version
/// is the workspace's, and it cannot drift from what was built.
const WORKSPACE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Writes the release manifest for a finished installer.
pub fn run(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let request = parse(arguments)?;
    let version = tag_version(&request.tag, WORKSPACE_VERSION)?;

    let bytes = fs::read(&request.installer)
        .map_err(|error| format!("cannot read {}: {error}", request.installer.display()))?;
    let asset_name = request
        .installer
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "the installer path has no usable file name: {}",
                request.installer.display()
            )
        })?;

    let asset = installer_asset(&version, asset_name, &bytes)?;
    let json = manifest_json(&version, asset)?;
    if let Some(parent) = request.output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    fs::write(&request.output, &json)
        .map_err(|error| format!("cannot write {}: {error}", request.output.display()))?;

    println!("release-manifest: {}", request.output.display());
    Ok(())
}

/// What `cargo release-manifest` was asked to do.
struct Request {
    tag: String,
    installer: PathBuf,
    output: PathBuf,
}

fn parse(arguments: impl Iterator<Item = String>) -> Result<Request, String> {
    let mut tag = None;
    let mut installer = None;
    let mut output = None;
    let mut arguments = arguments;

    while let Some(argument) = arguments.next() {
        let mut take = |slot: &mut Option<String>| match arguments.next() {
            Some(value) => {
                *slot = Some(value);
                Ok(())
            }
            None => Err(format!("{argument} needs a value")),
        };
        match argument.as_str() {
            "--tag" => take(&mut tag)?,
            "--installer" => take(&mut installer)?,
            "--output" => take(&mut output)?,
            other => return Err(format!("unknown argument `{other}`")),
        }
    }

    Ok(Request {
        tag: tag.ok_or("--tag <vX.Y.Z> is required")?,
        installer: installer.ok_or("--installer <path> is required")?.into(),
        output: output.ok_or("--output <path> is required")?.into(),
    })
}

/// The version a release tag names, refused unless it is the version this
/// workspace was built as.
///
/// A tag that disagrees is the mistake that ships one binary under another
/// version's number, and the manifest would then send every installation to a
/// download whose contents do not match what it says it is.
fn tag_version(tag: &str, workspace_version: &str) -> Result<String, String> {
    let version = tag
        .strip_prefix('v')
        .ok_or_else(|| format!("release tag `{tag}` does not start with `v`"))?;
    if version != workspace_version {
        return Err(format!(
            "release tag `{tag}` does not match the workspace version `{workspace_version}`"
        ));
    }
    Ok(version.to_owned())
}

/// The installer as the manifest describes it: where it will be downloaded
/// from, how large it is, and what it hashes to.
///
/// The size and the digest are measured here rather than passed in, because a
/// manifest is only worth anything if it describes the bytes that were built.
fn installer_asset(
    version: &str,
    asset_name: &str,
    bytes: &[u8],
) -> Result<InstallerAsset, String> {
    if bytes.is_empty() {
        return Err(format!("the installer `{asset_name}` is empty"));
    }

    let asset = InstallerAsset {
        url: format!("{RELEASE_DOWNLOAD_PREFIX}v{version}/{asset_name}"),
        size: bytes.len() as u64,
        sha256: Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    };
    // Validated here rather than only at the end, so an unusable asset name is
    // reported against the name and not against the finished manifest.
    validate(version, asset.clone())?;
    Ok(asset)
}

/// The manifest file's exact contents, newline included.
///
/// Pretty-printed on purpose: it is read by people during a release, and the
/// field order is the struct's, so the same inputs give the same bytes.
fn manifest_json(version: &str, asset: InstallerAsset) -> Result<String, String> {
    let manifest = validate(version, asset)?;
    let mut json = serde_json::to_string_pretty(&manifest)
        .map_err(|error| format!("cannot serialize the release manifest: {error}"))?;
    json.push('\n');
    Ok(json)
}

/// Builds the manifest and puts it through the application's own validation.
///
/// The generator and the application therefore agree by construction: anything
/// VMLord would refuse to install fails the release instead of reaching users.
fn validate(version: &str, installer: InstallerAsset) -> Result<ReleaseManifest, String> {
    let version = Version::parse(version)
        .map_err(|error| format!("`{version}` is not a semantic version: {error}"))?;
    let manifest = ReleaseManifest {
        schema: 1,
        version,
        installer,
    };
    // Against 0.0.0, because the question here is whether the manifest is
    // well-formed, not whether it is newer than any particular installation.
    manifest
        .validate(&Version::new(0, 0, 0))
        .map_err(|error| format!("the generated release manifest is invalid: {error}"))?
        .ok_or_else(|| "the generated release manifest names no newer version".to_owned())?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::{installer_asset, manifest_json, tag_version};

    #[test]
    fn a_release_tag_must_equal_the_workspace_version() {
        assert_eq!(tag_version("v0.1.0", "0.1.0").unwrap(), "0.1.0");
        assert!(tag_version("v0.2.0", "0.1.0").is_err());
    }

    /// A tag without the `v` is not the tag this project publishes, and
    /// guessing that it meant one would put the wrong string in every URL.
    #[test]
    fn a_tag_without_the_v_prefix_is_refused() {
        assert!(tag_version("0.1.0", "0.1.0").is_err());
    }

    /// The size and digest describe the bytes, so they are taken from them.
    #[test]
    fn the_installer_is_measured_from_its_own_bytes() {
        let asset = installer_asset(
            "0.1.0",
            "VMLord-0.1.0-x86_64-setup.exe",
            b"the installer bytes",
        )
        .unwrap();

        assert_eq!(asset.size, 19);
        assert_eq!(
            asset.sha256,
            "aa7fc234c2dd31617ea5698d39075836fd805988fb554ed913a0b45b4451bfca"
        );
        assert_eq!(
            asset.url,
            "https://github.com/MrUndead1996/vm-lord/releases/download/v0.1.0/\
             VMLord-0.1.0-x86_64-setup.exe"
        );
    }

    /// An empty file is not an installer, and a manifest naming one would send
    /// every VMLord on the internet to fetch zero bytes.
    #[test]
    fn an_empty_installer_is_refused() {
        assert!(installer_asset("0.1.0", "VMLord-0.1.0-x86_64-setup.exe", b"").is_err());
    }

    /// The same inputs give the same file, byte for byte: the manifest is
    /// compared against re-generated copies during a release.
    #[test]
    fn the_manifest_is_written_deterministically() {
        let asset = installer_asset(
            "0.1.0",
            "VMLord-0.1.0-x86_64-setup.exe",
            b"the installer bytes",
        )
        .unwrap();

        let json = manifest_json("0.1.0", asset.clone()).unwrap();

        assert_eq!(json, manifest_json("0.1.0", asset).unwrap());
        assert!(json.ends_with('\n'), "{json}");
        assert!(json.contains("\"schema\": 1"), "{json}");
    }

    /// The manifest is only worth publishing if the application would accept
    /// it, so generating one runs it through the same validation.
    #[test]
    fn a_manifest_the_application_would_refuse_is_never_written() {
        let asset = installer_asset("0.1.0", "setup?evil=1", b"the installer bytes");

        assert!(asset.is_err());
    }
}
