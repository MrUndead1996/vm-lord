//! The renderer a compositor built on aquamarine draws through.
//!
//! Aquamarine builds its DRM renderer only through `EGL_PLATFORM_DEVICE_EXT`,
//! matching an EGL device against the DRM node it was handed. `vmlord_drm` is
//! KMS-only and `/dev/dxg` is not a DRM device, so the one EGL device Mesa
//! enumerates is the software one and it carries no DRM file to match: no
//! renderer is built, and the output is modeset and never painted. The patch
//! beside this module calls the gbm-platform overload aquamarine already has
//! and never calls; what this module does is get that patch into the guest.
//!
//! Built here rather than shipped, because what it has to be built against is
//! whatever aquamarine the guest's own distribution installed, and that is not
//! knowable when a payload is packed. A `.so` built against another version is
//! not merely wrong, it is invisible: at a soname bump the loader passes it
//! over without a word and the guest goes black again. So the version is read
//! out of the guest on every run and the stamp beside the staged library says
//! what the staged one was built from.
//!
//! Nothing here is Hyprland: the guest is asked what its compositor loaded,
//! and a compositor with no aquamarine in it never reaches this module.

use std::{fs, path::Path, time::Duration};

use crate::command::{self, Outcome};

/// Where the built library is staged.
///
/// Beside the payload's Mesa rather than in `/usr/lib`, for the reason that
/// tree is there too: a file this agent wrote into a directory the package
/// manager owns is a file the next upgrade overwrites or, worse, keeps.
pub const PREFIX: &str = "/opt/vmlord/aquamarine";

/// The file beside the staged library that says what it was built from.
const STAMP: &str = "built-from";

/// Where the source comes from.
const SOURCE: &str = "https://github.com/hyprwm/aquamarine.git";

/// The patch, carried in the binary so that a guest needs nothing but a
/// checkout to apply it.
const PATCH: &str = include_str!(
    "../../../payloads/aquamarine/patches/0001-drm-fall-back-to-the-gbm-platform.patch"
);

/// How long the whole build may take.
///
/// Generous against a slow guest rather than tuned: measured at 18 seconds on
/// eight cores, and what this bound is for is a clone behind a broken NAT, not
/// a build that is merely slower than the one that was measured.
const BUDGET: Duration = Duration::from_secs(900);

/// The version a library file name carries: `libaquamarine.so.0.15.0` is
/// `0.15.0`.
///
/// The full version and not the soname, because the tag to check out is the
/// full version and two builds that share a soname are not the same source.
#[must_use]
pub fn version_of(library: &Path) -> Option<String> {
    let name = library.file_name()?.to_str()?;
    let rest = name.strip_prefix("libaquamarine.so.")?;
    // A soname link -- `libaquamarine.so.14` -- names no version, and neither
    // does the bare `libaquamarine.so` the linker uses.
    rest.contains('.').then(|| rest.to_owned())
}

/// The aquamarine the guest's distribution has installed, whatever the
/// compositor happens to be running.
///
/// Asked of the library directory rather than of the compositor, because the
/// compositor may already be running the library an earlier run staged -- and
/// then its own version is the answer to the wrong question. What decides
/// whether a build is needed is what the guest would load without us.
#[must_use]
pub fn packaged_version(library_directory: &Path) -> Option<String> {
    let entries = fs::read_dir(library_directory).ok()?;
    let mut found: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            // The real file, not the soname link beside it: a link resolves to
            // the same version and a directory listing gives no order worth
            // relying on.
            fs::symlink_metadata(entry.path()).is_ok_and(|data| data.is_file())
        })
        .filter_map(|entry| version_of(&entry.path()))
        .collect();
    found.sort();
    found.pop()
}

/// What is staged under `prefix`, where anything is.
#[must_use]
pub fn staged_version(prefix: &Path) -> Option<String> {
    let stamp = fs::read_to_string(prefix.join(STAMP)).ok()?;
    let version = stamp.trim();
    (!version.is_empty()).then(|| version.to_owned())
}

/// Builds aquamarine `version` with the patch and stages it under `prefix`.
///
/// `work` is a directory this may fill and is expected to remove: a checkout
/// and a build tree, neither of which outlives the call.
pub fn stage(version: &str, work: &Path, prefix: &Path) -> Result<(), String> {
    let checkout = work.join("aquamarine");
    let _ = fs::remove_dir_all(&checkout);
    fs::create_dir_all(work)
        .map_err(|error| format!("{} could not be made: {error}", work.display()))?;

    let tag = format!("v{version}");
    let checkout_path = checkout.to_string_lossy().into_owned();
    ran(
        "git",
        &[
            "clone",
            "--depth",
            "1",
            "--branch",
            &tag,
            SOURCE,
            &checkout_path,
        ],
    )?;

    let patch = work.join("gbm-fallback.patch");
    fs::write(&patch, PATCH)
        .map_err(|error| format!("{} could not be written: {error}", patch.display()))?;
    ran(
        "git",
        &["-C", &checkout_path, "apply", &patch.to_string_lossy()],
    )?;

    let build = checkout.join("build");
    let build_path = build.to_string_lossy().into_owned();
    ran(
        "cmake",
        &[
            "-S",
            &checkout_path,
            "-B",
            &build_path,
            "-G",
            "Ninja",
            "-DCMAKE_BUILD_TYPE=Release",
        ],
    )?;
    ran("ninja", &["-C", &build_path])?;

    install(&build, prefix, version)?;
    let _ = fs::remove_dir_all(&checkout);
    Ok(())
}

/// Copies what the build produced into `prefix`, stamp last.
///
/// Every `libaquamarine.so*` rather than a named one, because the soname is
/// the build's to decide and a copy that leaves the link behind is a directory
/// the loader looks straight past.
///
/// The stamp is written after the libraries and removed before them, so that
/// an interrupted run leaves a directory that says it holds nothing rather
/// than one that claims a version it does not have.
fn install(build: &Path, prefix: &Path, version: &str) -> Result<(), String> {
    let stamp = prefix.join(STAMP);
    let _ = fs::remove_file(&stamp);
    fs::create_dir_all(prefix)
        .map_err(|error| format!("{} could not be made: {error}", prefix.display()))?;

    let entries = fs::read_dir(build)
        .map_err(|error| format!("{} could not be read: {error}", build.display()))?;
    let mut copied = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("libaquamarine.so") {
            continue;
        }
        let destination = prefix.join(name);
        let _ = fs::remove_file(&destination);
        match fs::read_link(entry.path()) {
            Ok(target) => std::os::unix::fs::symlink(&target, &destination).map_err(|error| {
                format!("{} could not be linked: {error}", destination.display())
            })?,
            Err(_) => {
                fs::copy(entry.path(), &destination).map_err(|error| {
                    format!("{} could not be copied: {error}", destination.display())
                })?;
            }
        }
        copied += 1;
    }

    if copied == 0 {
        return Err(format!(
            "{} holds no libaquamarine.so after a build that said it succeeded",
            build.display()
        ));
    }

    fs::write(&stamp, format!("{version}\n"))
        .map_err(|error| format!("{} could not be written: {error}", stamp.display()))
}

/// Runs one program of the build, turning every way it can fail into one.
fn ran(program: &str, arguments: &[&str]) -> Result<(), String> {
    let outcome: Outcome = command::run(program, arguments, &[], BUDGET);
    if outcome.succeeded() {
        return Ok(());
    }
    Err(format!(
        "{program} {} failed: {}\n{}",
        arguments.join(" "),
        match outcome.ending {
            command::Ending::Exited(code) => format!("exit {code}"),
            command::Ending::TimedOut => "timed out".to_owned(),
            command::Ending::NotStarted => "not installed".to_owned(),
        },
        outcome.output
    ))
}

#[cfg(test)]
mod tests {
    use super::{packaged_version, staged_version, version_of};
    use std::path::{Path, PathBuf};

    fn temporary(label: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("vmlord-aquamarine-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn the_version_is_what_the_file_name_carries() {
        assert_eq!(
            version_of(Path::new("/usr/lib/libaquamarine.so.0.15.0")),
            Some("0.15.0".to_owned())
        );
    }

    #[test]
    fn a_soname_link_names_no_version() {
        assert_eq!(
            version_of(Path::new("/usr/lib/libaquamarine.so.14")),
            None,
            "the tag to check out is the full version, and 14 is not one"
        );
        assert_eq!(version_of(Path::new("/usr/lib/libaquamarine.so")), None);
        assert_eq!(version_of(Path::new("/usr/lib/libmutter-16.so.0")), None);
    }

    #[test]
    fn the_packaged_version_is_read_past_the_links_beside_it() {
        let directory = temporary("packaged");
        std::fs::write(directory.join("libaquamarine.so.0.15.0"), "").unwrap();
        std::os::unix::fs::symlink(
            "libaquamarine.so.0.15.0",
            directory.join("libaquamarine.so.14"),
        )
        .unwrap();
        std::os::unix::fs::symlink("libaquamarine.so.14", directory.join("libaquamarine.so"))
            .unwrap();
        std::fs::write(directory.join("libwayland-server.so.0.23.0"), "").unwrap();

        assert_eq!(packaged_version(&directory), Some("0.15.0".to_owned()));
    }

    #[test]
    fn a_guest_without_aquamarine_installed_has_no_packaged_version() {
        let directory = temporary("packaged-none");
        std::fs::write(directory.join("libmutter-16.so.0"), "").unwrap();

        assert_eq!(packaged_version(&directory), None);
        assert_eq!(
            packaged_version(&directory.join("not-a-directory")),
            None,
            "a directory that is not there is a guest without one, not a failure"
        );
    }

    #[test]
    fn what_is_staged_is_what_the_stamp_says() {
        let prefix = temporary("stamp");
        assert_eq!(
            staged_version(&prefix),
            None,
            "nothing staged is nothing to compare against"
        );

        std::fs::write(prefix.join("built-from"), "0.15.0\n").unwrap();
        assert_eq!(staged_version(&prefix), Some("0.15.0".to_owned()));

        std::fs::write(prefix.join("built-from"), "\n").unwrap();
        assert_eq!(
            staged_version(&prefix),
            None,
            "an empty stamp is a build that did not finish, so it claims nothing"
        );
    }
}
