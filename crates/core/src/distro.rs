//! Which distribution to fetch, where its releases live, and what the guest
//! inside them looks like.
//!
//! A profile is a table of data, not a trait with one implementation per
//! distribution. Ubuntu and Fedora differ by a URL template, a default user, an
//! admin group and the name of a checksum file -- those are fields, not
//! behaviour, and five structs differing only in constants are exactly what
//! AGENTS.md means by unnecessary abstractions.
//!
//! The fields own their strings rather than borrowing `'static` ones: profiles
//! are to be read from a JSON file, and a parsed file yields no `&'static str`
//! short of leaking it.

use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{SettingsStore, display::DesktopProfile};

/// The placeholder both templates carry.
const RELEASE_PLACEHOLDER: &str = "{release}";
/// The placeholder a keyboard file's template carries.
const LAYOUT_PLACEHOLDER: &str = "{layout}";
const BUNDLED_PROFILES_FILE_NAME: &str = ".bundled-profiles.json";

/// Copies installed profiles into the current user's catalogue without taking
/// ownership of profiles the user already created.
pub fn sync_bundled_profiles(
    bundle: &Path,
    store: &SettingsStore,
) -> Result<(), DistroCatalogError> {
    let directory = distro_directory(store)?;
    fs::create_dir_all(&directory).map_err(|source| DistroCatalogError::Io {
        operation: "create distribution profile directory",
        path: directory.clone(),
        source,
    })?;

    let ownership_path = directory.join(BUNDLED_PROFILES_FILE_NAME);
    let mut ownership = read_bundled_profile_hashes(&ownership_path)?;
    let entries = fs::read_dir(bundle).map_err(|source| DistroCatalogError::Io {
        operation: "read bundled distribution profile directory",
        path: bundle.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| DistroCatalogError::Io {
            operation: "read bundled distribution profile directory entry",
            path: bundle.to_path_buf(),
            source,
        })?;
        let profile_path = entry.path();
        if !entry
            .file_type()
            .map_err(|source| DistroCatalogError::Io {
                operation: "read bundled distribution profile type",
                path: profile_path.clone(),
                source,
            })?
            .is_file()
        {
            continue;
        }
        let name = validated_profile_file_name(&profile_path)?;
        let contents = fs::read(&profile_path).map_err(|source| DistroCatalogError::Io {
            operation: "read bundled distribution profile",
            path: profile_path.clone(),
            source,
        })?;
        let hash = Sha256::digest(&contents)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let destination = directory.join(&name);
        let owned_hash = ownership.get(&name);
        let managed = !destination.exists() || owned_hash.is_some();
        if managed
            && (!destination.exists() || owned_hash.is_some_and(|recorded| recorded != &hash))
        {
            write_atomically(&destination, &contents)?;
        }
        if managed {
            ownership.insert(name, hash);
        }
    }

    write_ownership_document(&ownership_path, &ownership)
}

fn distro_directory(store: &SettingsStore) -> Result<PathBuf, DistroCatalogError> {
    store
        .config_path()
        .parent()
        .map(|parent| parent.join("distros"))
        .ok_or_else(|| DistroCatalogError::MissingSettingsParent {
            path: store.config_path().to_path_buf(),
        })
}

fn read_bundled_profile_hashes(
    path: &Path,
) -> Result<BTreeMap<String, String>, DistroCatalogError> {
    match fs::read_to_string(path) {
        Ok(document) => {
            serde_json::from_str(&document).map_err(|source| DistroCatalogError::OwnershipParse {
                path: path.to_path_buf(),
                source,
            })
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(source) => Err(DistroCatalogError::Io {
            operation: "read bundled profile ownership",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn validated_profile_file_name(path: &Path) -> Result<String, DistroCatalogError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| DistroCatalogError::InvalidBundledProfileName {
            path: path.to_path_buf(),
        })?;
    if Path::new(name).components().count() != 1
        || Path::new(name)
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("json")
        || Path::new(name)
            .file_stem()
            .is_none_or(|stem| stem.is_empty())
    {
        return Err(DistroCatalogError::InvalidBundledProfileName {
            path: path.to_path_buf(),
        });
    }
    Ok(name.to_owned())
}

fn write_ownership_document(
    path: &Path,
    ownership: &BTreeMap<String, String>,
) -> Result<(), DistroCatalogError> {
    let document = serde_json::to_vec_pretty(ownership).expect("a string map always serializes");
    write_atomically(path, &document)
}

fn write_atomically(path: &Path, contents: &[u8]) -> Result<(), DistroCatalogError> {
    use std::io::Write;

    let directory = path.parent().ok_or_else(|| DistroCatalogError::Io {
        operation: "create temporary distribution profile",
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "profile path has no parent"),
    })?;
    let mut file = NamedTempFile::new_in(directory).map_err(|source| DistroCatalogError::Io {
        operation: "create temporary distribution profile",
        path: directory.to_path_buf(),
        source,
    })?;
    let temporary = file.path().to_path_buf();
    file.write_all(contents)
        .map_err(|source| DistroCatalogError::Io {
            operation: "write temporary distribution profile",
            path: temporary.clone(),
            source,
        })?;
    file.as_file()
        .sync_all()
        .map_err(|source| DistroCatalogError::Io {
            operation: "sync temporary distribution profile",
            path: temporary.clone(),
            source,
        })?;
    file.persist(path).map_err(|error| DistroCatalogError::Io {
        operation: "replace distribution profile",
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

/// Where a distribution publishes its cloud images, and what the guest inside
/// them looks like.
///
/// The URL is kept as two templates rather than one: the checksum file sits in
/// the same directory as the image, and a single template would have to have its
/// tail cut off to get at that directory.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DistroProfile {
    pub name: String,
    pub releases: Vec<String>,
    pub directory_template: String,
    pub file_name_template: String,
    pub checksum_file: String,
    /// The account cloud-init creates in the guest.
    pub default_user: String,
    /// The group that account must join to hold administrative rights.
    pub admin_group: String,
    /// How this distribution runs and configures its SSH daemon.
    pub ssh: SshDaemon,
    /// What installing a GNOME desktop on this distribution takes, when it is
    /// something VMLord knows how to install at all.
    ///
    /// `None` is a profile read from a file that says nothing about a
    /// desktop -- a VM built from it can only be headless, which is a fact
    /// about the profile rather than a failure to be reported later.
    pub desktop: Option<DesktopSetup>,
    /// What has to happen to the guest's packages before the desktop's are
    /// installed.
    ///
    /// Defaulted rather than required, because a profile written before this
    /// field existed described a distribution where refreshing the lists is
    /// the whole answer, which is what the default says.
    #[serde(default)]
    pub package_refresh: PackageRefresh,
    /// The files that tell this distribution's guest what keyboard layout it
    /// has.
    ///
    /// A list rather than one path: Debian keeps the whole answer in
    /// `/etc/default/keyboard`, while Arch configures the console through
    /// `/etc/vconsole.conf` and the graphical session through
    /// `/etc/X11/xorg.conf.d/00-keyboard.conf`. That is not one setting
    /// written twice but two files, in two syntaxes, that a guest needs both
    /// of.
    pub keyboard: Vec<KeyboardFile>,
}

/// What a distribution needs done to its packages before new ones are added.
///
/// The distinction is not a preference, it is what a distribution supports.
/// Debian and its family resolve a new package against refreshed lists and
/// leave everything installed where it is; Arch resolves against one moving
/// repository, so installing into an image a month old pulls in libraries
/// built for packages the guest has not upgraded to -- a partial upgrade, which
/// Arch documents as unsupported and does not test.
///
/// A field rather than a branch on the distribution's name, for the reason
/// [`SshDaemon`] is data: what the seed prints is one more cloud-init key, and
/// a generator that knew which distributions roll would have to be edited for
/// the next one.
///
/// The variant names are an on-disk format: they are what a profile's JSON
/// spells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum PackageRefresh {
    /// Refresh the package lists and install against them. cloud-init's
    /// `package_update`, which is what every profile written so far meant.
    #[default]
    Lists,
    /// Upgrade everything installed first, then add the new packages.
    /// cloud-init's `package_upgrade`, which on Arch is the `-u` that turns
    /// `pacman -Sy` into a full `-Syu`.
    FullUpgrade,
}

/// One file the seed writes to set the guest's keyboard layout.
///
/// The content is a template rather than keys this crate knows, for the reason
/// [`SshDaemon`] is data: the difference between distributions here is a path,
/// a syntax and a handful of lines, and whatever writes the seed has no
/// business knowing what `XKBMODEL` is.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct KeyboardFile {
    /// Where in the guest the file goes.
    pub path: String,
    /// The syntax the layout sits in, which is what decides how it is escaped.
    pub form: KeyboardForm,
    /// The whole file, with [`LAYOUT_PLACEHOLDER`] where the layout goes.
    pub template: String,
}

impl KeyboardFile {
    /// The file's content for `layout`, escaped for the form it is written in.
    #[must_use]
    pub fn content(&self, layout: &str) -> String {
        self.template
            .replace(LAYOUT_PLACEHOLDER, &self.form.escape(layout))
    }
}

/// The syntax a keyboard file is written in.
///
/// A form rather than a path pattern, because the escaping is what differs:
/// a value safe inside a shell assignment is not thereby safe inside an Xorg
/// option, and neither escaping stands in for the other.
///
/// The variant names are an on-disk format: they are what a profile's JSON
/// spells.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum KeyboardForm {
    /// `KEY="value"` lines. Debian's `/etc/default/keyboard` is read with
    /// `source`, where an unescaped `$` or backtick is code and a quote ends
    /// the assignment; systemd reads `/etc/vconsole.conf` itself and runs
    /// nothing, so escaping for the stricter of the two is safe for both.
    ShellAssignment,
    /// `Option "XkbLayout" "value"` inside an Xorg configuration section.
    ///
    /// Xorg's parser reads a quoted string up to the next quote and knows no
    /// escape sequences at all, so a quote cannot be written into one: it is
    /// dropped rather than passed through, since passing it through would end
    /// the string and leave the rest of the value as configuration.
    XorgString,
}

impl KeyboardForm {
    /// `layout`, safe to substitute into a file of this form.
    ///
    /// Neither escaping handles control characters, and neither has to:
    /// `Provisioning::validate` refuses them before a seed is ever built.
    fn escape(self, layout: &str) -> String {
        match self {
            Self::ShellAssignment => {
                let mut escaped = String::with_capacity(layout.len());
                for character in layout.chars() {
                    if matches!(character, '\\' | '"' | '`' | '$') {
                        escaped.push('\\');
                    }
                    escaped.push(character);
                }
                escaped
            }
            Self::XorgString => layout.replace('"', ""),
        }
    }
}

/// What installing a desktop into a guest of this distribution takes.
///
/// Data rather than knowledge inside whatever writes the cloud-init seed, for
/// the same reason [`SshDaemon`] is: the difference between distributions here
/// is a list of package names and the name of a unit.
///
/// Every package is one the distribution publishes in its own archives. VMLord
/// adds no repository, downloads no binary of its own and signs nothing: the
/// desktop a guest ends up with is the one its vendor ships, updated by the
/// guest's own updates.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DesktopSetup {
    /// The packages that bring in GNOME, GDM and their Wayland session.
    ///
    /// The whole of what a distribution declares about a desktop, and
    /// deliberately so: a package list is what cloud-init needs before there
    /// is a guest to ask anything of, and everything else about a desktop --
    /// which display manager owns the login screen, what the session on the
    /// screen calls itself -- the agent reads out of the guest at the moment
    /// it acts. A declared display manager was exactly that second copy, and
    /// it was deleted rather than kept in step with the truth beside it.
    pub packages: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DistroCatalog {
    profiles: BTreeMap<String, DistroProfile>,
}

impl DistroCatalog {
    pub fn load(settings: &SettingsStore) -> Result<Self, DistroCatalogError> {
        let directory = distro_directory(settings)?;
        let entries = fs::read_dir(&directory).map_err(|source| DistroCatalogError::Io {
            operation: "read distribution profile directory",
            path: directory.clone(),
            source,
        })?;
        let mut profiles = BTreeMap::new();
        for entry in entries {
            let entry = entry.map_err(|source| DistroCatalogError::Io {
                operation: "read distribution profile directory entry",
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            if path.file_name().and_then(|name| name.to_str()) == Some(BUNDLED_PROFILES_FILE_NAME) {
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("a JSON path returned by read_dir has a file stem")
                .to_owned();
            let document = fs::read_to_string(&path).map_err(|source| DistroCatalogError::Io {
                operation: "read distribution profile",
                path: path.clone(),
                source,
            })?;
            let profile =
                serde_json::from_str(&document).map_err(|source| DistroCatalogError::Parse {
                    path: path.clone(),
                    source,
                })?;
            profiles.insert(id, profile);
        }
        Ok(Self { profiles })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    pub fn select(&self, id: &str) -> Result<&DistroProfile, DistroCatalogError> {
        self.profiles
            .get(id)
            .ok_or_else(|| DistroCatalogError::ProfileNotFound { id: id.to_owned() })
    }

    pub fn options(&self) -> impl Iterator<Item = (&str, &str)> {
        self.profiles
            .iter()
            .map(|(id, profile)| (id.as_str(), profile.name.as_str()))
    }

    /// Every loaded profile with the identifier it was loaded under.
    pub fn profiles(&self) -> impl Iterator<Item = (&str, &DistroProfile)> {
        self.profiles
            .iter()
            .map(|(id, profile)| (id.as_str(), profile))
    }
}

#[derive(Debug)]
pub enum DistroCatalogError {
    MissingSettingsParent {
        path: PathBuf,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    OwnershipParse {
        path: PathBuf,
        source: serde_json::Error,
    },
    InvalidBundledProfileName {
        path: PathBuf,
    },
    ProfileNotFound {
        id: String,
    },
}

impl fmt::Display for DistroCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSettingsParent { path } => {
                write!(
                    formatter,
                    "settings path has no parent directory: {}",
                    path.display()
                )
            }
            Self::Io {
                operation,
                path,
                source,
            } => {
                write!(
                    formatter,
                    "failed to {operation} at {}: {source}",
                    path.display()
                )
            }
            Self::Parse { path, source } => {
                write!(
                    formatter,
                    "failed to parse distribution profile at {}: {source}",
                    path.display()
                )
            }
            Self::OwnershipParse { path, source } => {
                write!(
                    formatter,
                    "failed to parse bundled profile ownership at {}: {source}",
                    path.display()
                )
            }
            Self::InvalidBundledProfileName { path } => {
                write!(
                    formatter,
                    "bundled distribution profile has an invalid file name: {}",
                    path.display()
                )
            }
            Self::ProfileNotFound { id } => {
                write!(formatter, "distribution profile {id:?} was not found")
            }
        }
    }
}

impl std::error::Error for DistroCatalogError {}

/// How a distribution starts its SSH daemon, and where a setting of VMLord's
/// has to be written for the daemon to read it.
///
/// Data rather than knowledge inside the seed generator: the differences
/// between distributions here are file paths and unit names, and a generator
/// that branched on `"Ubuntu"` would have to be edited for every profile added
/// to a JSON file later.
///
/// Serializable because it is recorded per VM at creation: moving the port of
/// an installed guest later has to write the same files and poke the same
/// units the seed did, and by then the profile the VM was built from is gone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshDaemon {
    /// The systemd units that carry the daemon, and how they carry it.
    pub units: SshUnits,
    /// The drop-in file that overrides the daemon's own configuration.
    ///
    /// A file of VMLord's own rather than an edit of `sshd_config`: a drop-in
    /// is written whole, so nothing has to be found, matched or replaced inside
    /// a file the distribution owns and may change between releases.
    pub config_drop_in: String,
}

/// Which units a distribution's SSH daemon is made of.
///
/// The two shapes are different enough that one flat list of unit names could
/// not describe either honestly: where a socket owns the listening port, the
/// port lives in the socket's drop-in and the service must never be started by
/// hand beside it; where the daemon opens its own port, there is no socket at
/// all and `sshd_config` is the whole story. Spelling that as a choice keeps
/// the impossible combinations -- a socket drop-in with no socket unit, a
/// profile naming no units whatsoever -- out of the type.
///
/// The variant and field names are an on-disk format, like [`SshDaemon`]'s:
/// renaming one changes what a stored VM reads back as.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshUnits {
    /// The daemon opens its own port. Fedora and SUSE name this unit `sshd`.
    Service { unit: String },
    /// The socket owns the port and activates the daemon on demand, which is
    /// how Debian-family systems have run it since Ubuntu 22.10.
    SocketActivated {
        socket: String,
        /// The drop-in that moves the socket's listener.
        ///
        /// A socket-activated `sshd` is handed a descriptor that is already
        /// bound, so `sshd_config`'s `Port` is read and then ignored: this file
        /// is what actually decides where the guest answers.
        socket_drop_in: String,
        service: String,
    },
}

impl SshUnits {
    /// Every unit that has to be switched off for the guest to run no daemon,
    /// the socket first: it is the one holding the port open.
    #[must_use]
    pub fn all(&self) -> Vec<&str> {
        match self {
            Self::Service { unit } => vec![unit],
            Self::SocketActivated {
                socket, service, ..
            } => vec![socket, service],
        }
    }
}

/// Ubuntu fixture shared by tests in workspace crates.
///
/// The feature is dev-only: release binaries must load this document from the
/// installed `distros` directory rather than embed a second copy.
#[cfg(any(test, feature = "test-profile"))]
#[must_use]
pub fn ubuntu() -> DistroProfile {
    serde_json::from_str(include_str!("../../../distros/ubuntu.json"))
        .expect("the workspace Ubuntu test profile must be valid")
}

impl DistroProfile {
    /// What installing `profile` on this distribution takes, or `None` when
    /// there is nothing to install -- either because no desktop was asked for
    /// or because this distribution has no description of one.
    #[must_use]
    pub fn desktop_for(&self, profile: DesktopProfile) -> Option<&DesktopSetup> {
        profile
            .wants_desktop()
            .then_some(self.desktop.as_ref())
            .flatten()
    }

    /// The URL of the image itself.
    #[must_use]
    pub fn image_url(&self, release: &str) -> String {
        format!("{}{}", self.directory(release), self.file_name(release))
    }

    /// The URL of the checksum file published beside it.
    #[must_use]
    pub fn checksums_url(&self, release: &str) -> String {
        format!("{}{}", self.directory(release), self.checksum_file)
    }

    /// The name the image carries inside the checksum file.
    #[must_use]
    pub fn file_name(&self, release: &str) -> String {
        self.file_name_template
            .replace(RELEASE_PLACEHOLDER, release)
    }

    fn directory(&self, release: &str) -> String {
        let directory = self
            .directory_template
            .replace(RELEASE_PLACEHOLDER, release);
        if directory.ends_with('/') {
            directory
        } else {
            format!("{directory}/")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        DesktopProfile, DistroCatalog, DistroProfile, KeyboardFile, KeyboardForm, PackageRefresh,
        SshUnits, sync_bundled_profiles, ubuntu,
    };
    use crate::SettingsStore;

    /// A counter beside the clock, because the clock is not enough on its own.
    /// Windows advances `SystemTime` in steps of about fifteen milliseconds,
    /// so two of these tests starting together read the same nanosecond and
    /// would share a directory -- one of them then deleting the other's.
    static FIXTURES: AtomicUsize = AtomicUsize::new(0);

    fn temporary_directory() -> std::path::PathBuf {
        let unique_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = FIXTURES.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("vmlord-distro-test-{unique_id}-{sequence}"))
    }

    fn profile_document(name: &str, user: &str) -> String {
        format!(
            r#"{{
                "name": "{name}",
                "releases": ["26.04", "24.04"],
                "directory_template": "https://images.example/{name}/{{release}}/",
                "file_name_template": "{name}-{{release}}.img",
                "checksum_file": "SHA256SUMS",
                "default_user": "{user}",
                "admin_group": "wheel",
                "ssh": {{
                    "units": {{ "Service": {{ "unit": "sshd.service" }} }},
                    "config_drop_in": "/etc/ssh/sshd_config.d/10-vmlord.conf"
                }},
                "desktop": null,
                "keyboard": [
                    {{
                        "path": "/etc/vconsole.conf",
                        "form": "ShellAssignment",
                        "template": "KEYMAP=\"{{layout}}\"\n"
                    }}
                ]
            }}"#
        )
    }

    struct ProfileFixture {
        directory: PathBuf,
        bundle: PathBuf,
        user: PathBuf,
        store: SettingsStore,
    }

    impl ProfileFixture {
        fn new() -> Self {
            let directory = temporary_directory();
            let bundle = directory.join("bundle");
            let user = directory.join("user").join("distros");
            fs::create_dir_all(&bundle).unwrap();
            fs::create_dir_all(&user).unwrap();
            Self {
                store: SettingsStore::new(directory.join("user").join("settings.toml")),
                directory,
                bundle,
                user,
            }
        }

        fn write_bundle(&self, name: &str, contents: &str) {
            fs::write(self.bundle.join(name), contents).unwrap();
        }

        fn write_user(&self, name: &str, contents: &str) {
            fs::write(self.user.join(name), contents).unwrap();
        }

        fn read_user(&self, name: &str) -> String {
            fs::read_to_string(self.user.join(name)).unwrap()
        }
    }

    impl Drop for ProfileFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.directory).unwrap();
        }
    }

    #[test]
    fn a_missing_bundled_profile_is_copied_to_the_users_catalog() {
        let fixture = ProfileFixture::new();
        fixture.write_bundle("ubuntu.json", "bundle copy");

        sync_bundled_profiles(&fixture.bundle, &fixture.store).unwrap();

        assert_eq!(fixture.read_user("ubuntu.json"), "bundle copy");
    }

    #[test]
    fn a_changed_bundled_profile_replaces_its_recorded_copy() {
        let fixture = ProfileFixture::new();
        fixture.write_bundle("ubuntu.json", "old bundle");
        sync_bundled_profiles(&fixture.bundle, &fixture.store).unwrap();
        fixture.write_bundle("ubuntu.json", "new bundle");

        sync_bundled_profiles(&fixture.bundle, &fixture.store).unwrap();

        assert_eq!(fixture.read_user("ubuntu.json"), "new bundle");
    }

    #[test]
    fn synchronizing_an_unchanged_bundle_twice_keeps_its_ownership_record() {
        let fixture = ProfileFixture::new();
        fixture.write_bundle("ubuntu.json", "bundle copy");
        sync_bundled_profiles(&fixture.bundle, &fixture.store).unwrap();

        sync_bundled_profiles(&fixture.bundle, &fixture.store).unwrap();

        assert_eq!(fixture.read_user("ubuntu.json"), "bundle copy");
    }

    #[test]
    fn a_user_profile_is_never_replaced_by_a_bundled_profile() {
        let fixture = ProfileFixture::new();
        fixture.write_bundle("ubuntu.json", "new bundle");
        fixture.write_user("ubuntu.json", "user copy");

        sync_bundled_profiles(&fixture.bundle, &fixture.store).unwrap();

        assert_eq!(fixture.read_user("ubuntu.json"), "user copy");
    }

    #[test]
    fn a_catalog_loads_every_json_profile_and_selects_the_configured_one() {
        let directory = temporary_directory();
        let distros = directory.join("distros");
        fs::create_dir_all(&distros).unwrap();
        fs::write(
            distros.join("ubuntu.json"),
            profile_document("Ubuntu", "ubuntu"),
        )
        .unwrap();
        fs::write(
            distros.join("fedora.json"),
            profile_document("Fedora", "fedora"),
        )
        .unwrap();
        fs::write(distros.join("README.txt"), "not a profile").unwrap();
        let store = SettingsStore::new(directory.join("settings.toml"));

        let catalog = DistroCatalog::load(&store).unwrap();

        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog.select("fedora").unwrap().default_user, "fedora");
        assert_eq!(
            catalog.options().collect::<Vec<_>>(),
            [("fedora", "Fedora"), ("ubuntu", "Ubuntu")]
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_missing_profile_directory_reports_the_path_that_was_read() {
        let directory = temporary_directory();
        fs::create_dir_all(&directory).unwrap();
        let store = SettingsStore::new(directory.join("settings.toml"));

        let error = DistroCatalog::load(&store).unwrap_err().to_string();

        assert!(
            error.contains("read distribution profile directory"),
            "{error}"
        );
        assert!(
            error.contains(&directory.join("distros").display().to_string()),
            "{error}"
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_json_reports_the_profile_file_and_parse_failure() {
        let directory = temporary_directory();
        let distros = directory.join("distros");
        fs::create_dir_all(&distros).unwrap();
        let profile_path = distros.join("broken.json");
        fs::write(&profile_path, "{ definitely not JSON }").unwrap();
        let store = SettingsStore::new(directory.join("settings.toml"));

        let error = DistroCatalog::load(&store).unwrap_err().to_string();

        assert!(error.contains("parse distribution profile"), "{error}");
        assert!(
            error.contains(&profile_path.display().to_string()),
            "{error}"
        );
        assert!(error.contains("line 1 column"), "{error}");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn selecting_an_unknown_configured_profile_names_its_identifier() {
        let directory = temporary_directory();
        let distros = directory.join("distros");
        fs::create_dir_all(&distros).unwrap();
        fs::write(
            distros.join("ubuntu.json"),
            profile_document("Ubuntu", "ubuntu"),
        )
        .unwrap();
        let store = SettingsStore::new(directory.join("settings.toml"));
        let catalog = DistroCatalog::load(&store).unwrap();

        let error = catalog.select("arch").unwrap_err().to_string();

        assert!(error.contains("arch"), "{error}");
        assert!(error.contains("not found"), "{error}");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_shipped_ubuntu_profile_preserves_the_existing_resolver_contract() {
        let profile: DistroProfile =
            serde_json::from_str(include_str!("../../../distros/ubuntu.json")).unwrap();

        assert_eq!(profile.name, "Ubuntu");
        assert_eq!(profile.releases, ["26.04", "24.04", "22.04"]);
        assert_eq!(profile.default_user, "ubuntu");
        assert_eq!(profile.admin_group, "sudo");
        assert_eq!(
            profile.image_url("24.04"),
            "https://cloud-images.ubuntu.com/releases/24.04/release/\
             ubuntu-24.04-server-cloudimg-amd64.img"
        );
        assert_eq!(
            profile.checksums_url("24.04"),
            "https://cloud-images.ubuntu.com/releases/24.04/release/SHA256SUMS"
        );
        assert_eq!(profile.ssh.units.all(), ["ssh.socket", "ssh.service"]);
        assert_eq!(
            profile.desktop.unwrap().packages,
            ["ubuntu-desktop-minimal"]
        );
    }

    /// Every field here was read off the distribution rather than assumed.
    /// The image and its `.SHA256` sit in one directory that carries no
    /// release, which is why both templates spell no `{release}`; Arch's
    /// `openssh` prepends `Include /etc/ssh/sshd_config.d/*.conf` to
    /// `sshd_config` and ships no `sshd.socket`, and `arch-boxes` enables
    /// `sshd` -- so the port is a plain drop-in read by one service.
    #[test]
    fn the_shipped_arch_profile_matches_what_the_distribution_publishes() {
        let profile: DistroProfile =
            serde_json::from_str(include_str!("../../../distros/arch.json")).unwrap();

        assert_eq!(profile.name, "Arch Linux");
        assert_eq!(profile.releases, ["rolling"]);
        assert_eq!(profile.default_user, "arch");
        assert_eq!(profile.admin_group, "wheel");
        assert_eq!(
            profile.image_url("rolling"),
            "https://geo.mirror.pkgbuild.com/images/latest/Arch-Linux-x86_64-cloudimg.qcow2"
        );
        assert_eq!(
            profile.checksums_url("rolling"),
            "https://geo.mirror.pkgbuild.com/images/latest/\
             Arch-Linux-x86_64-cloudimg.qcow2.SHA256"
        );
        assert_eq!(profile.ssh.units.all(), ["sshd.service"]);
        assert_eq!(
            profile.ssh.config_drop_in,
            "/etc/ssh/sshd_config.d/10-vmlord.conf"
        );
        assert_eq!(profile.desktop.unwrap().packages[0], "gnome-shell");
    }

    /// A directory that names no release still has to answer the same two
    /// URLs for every release the profile offers, since the resolver asks for
    /// them by release whatever the templates do with it.
    #[test]
    fn a_release_that_is_not_in_the_url_leaves_the_arch_templates_alone() {
        let profile: DistroProfile =
            serde_json::from_str(include_str!("../../../distros/arch.json")).unwrap();

        assert_eq!(profile.image_url("rolling"), profile.image_url("24.04"));
        assert_eq!(
            profile.file_name("rolling"),
            "Arch-Linux-x86_64-cloudimg.qcow2"
        );
    }

    /// Arch resolves a new package against one moving repository, so a
    /// month-old image has to be upgraded rather than added to; Ubuntu does
    /// not, and saying so in the profile is what keeps the seed from branching
    /// on a distribution's name.
    #[test]
    fn the_shipped_profiles_state_what_installing_into_them_takes() {
        let arch: DistroProfile =
            serde_json::from_str(include_str!("../../../distros/arch.json")).unwrap();
        let ubuntu: DistroProfile =
            serde_json::from_str(include_str!("../../../distros/ubuntu.json")).unwrap();

        assert_eq!(arch.package_refresh, PackageRefresh::FullUpgrade);
        assert_eq!(ubuntu.package_refresh, PackageRefresh::Lists);
    }

    /// Arch splits the console keymap from the graphical layout, and the two
    /// files are read by different parsers -- which is what the forms are for.
    #[test]
    fn the_shipped_arch_profile_writes_both_keyboard_files() {
        let profile: DistroProfile =
            serde_json::from_str(include_str!("../../../distros/arch.json")).unwrap();

        let [console, graphical] = profile.keyboard.as_slice() else {
            panic!("Arch names two keyboard files");
        };
        assert_eq!(console.path, "/etc/vconsole.conf");
        assert_eq!(console.form, KeyboardForm::ShellAssignment);
        assert_eq!(console.content("ru"), "KEYMAP=\"ru\"\n");

        assert_eq!(graphical.path, "/etc/X11/xorg.conf.d/00-keyboard.conf");
        assert_eq!(graphical.form, KeyboardForm::XorgString);
        assert!(
            graphical
                .content("ru")
                .contains("Option \"XkbLayout\" \"ru\""),
            "{}",
            graphical.content("ru")
        );
    }

    /// A profile written before the field existed still loads, and reads as
    /// the distribution it was written for: one where refreshing the lists is
    /// the whole answer.
    #[test]
    fn a_profile_that_says_nothing_about_upgrading_refreshes_the_lists() {
        let profile: DistroProfile =
            serde_json::from_str(&profile_document("legacy", "legacy")).unwrap();

        assert_eq!(profile.package_refresh, PackageRefresh::Lists);
    }

    /// The file the shipped profile names is the one the seed used to carry as
    /// a constant, spelled the same way: an Ubuntu guest's layout has to end up
    /// where `console-setup` reads it, not merely somewhere.
    #[test]
    fn the_shipped_ubuntu_profile_writes_the_debian_keyboard_file() {
        let profile: DistroProfile =
            serde_json::from_str(include_str!("../../../distros/ubuntu.json")).unwrap();

        let [keyboard] = profile.keyboard.as_slice() else {
            panic!("Ubuntu names one keyboard file");
        };
        assert_eq!(keyboard.path, "/etc/default/keyboard");
        assert_eq!(keyboard.form, KeyboardForm::ShellAssignment);
        assert_eq!(
            keyboard.content("ru"),
            "XKBMODEL=\"pc105\"\nXKBLAYOUT=\"ru\"\nXKBVARIANT=\"\"\n\
             XKBOPTIONS=\"\"\nBACKSPACE=\"guess\"\n"
        );
    }

    /// A file read with `source` treats an unescaped `$` or backtick as code
    /// and a quote as the end of the assignment.
    #[test]
    fn a_shell_assignment_cannot_run_a_command_or_end_itself() {
        let file = KeyboardFile {
            path: "/etc/vconsole.conf".into(),
            form: KeyboardForm::ShellAssignment,
            template: "KEYMAP=\"{layout}\"\n".into(),
        };

        assert_eq!(file.content("us"), "KEYMAP=\"us\"\n");
        assert_eq!(file.content("us$(id)"), "KEYMAP=\"us\\$(id)\"\n");
        assert_eq!(file.content("us`id`"), "KEYMAP=\"us\\`id\\`\"\n");
        assert_eq!(
            file.content("us\"; reboot #"),
            "KEYMAP=\"us\\\"; reboot #\"\n"
        );
        assert_eq!(file.content("us\\"), "KEYMAP=\"us\\\\\"\n");
    }

    /// Xorg knows no escape sequences inside a quoted string, so the same
    /// backslash that saves the shell file would be a literal character here
    /// and the quote would still end the string. It goes instead.
    #[test]
    fn an_xorg_string_keeps_a_quote_out_rather_than_escaping_it() {
        let file = KeyboardFile {
            path: "/etc/X11/xorg.conf.d/00-keyboard.conf".into(),
            form: KeyboardForm::XorgString,
            template: "    Option \"XkbLayout\" \"{layout}\"\n".into(),
        };

        assert_eq!(file.content("us"), "    Option \"XkbLayout\" \"us\"\n");
        assert_eq!(
            file.content("us\"\n    Option \"XkbOptions\" \"terminate"),
            "    Option \"XkbLayout\" \"us\n    Option XkbOptions terminate\"\n"
        );
    }

    #[test]
    fn a_profile_builds_the_image_url_and_the_checksums_url_in_one_directory() {
        assert_eq!(
            ubuntu().image_url("24.04"),
            "https://cloud-images.ubuntu.com/releases/24.04/release/\
             ubuntu-24.04-server-cloudimg-amd64.img"
        );
        assert_eq!(
            ubuntu().checksums_url("24.04"),
            "https://cloud-images.ubuntu.com/releases/24.04/release/SHA256SUMS"
        );
        assert_eq!(
            ubuntu().file_name("22.04"),
            "ubuntu-22.04-server-cloudimg-amd64.img"
        );
    }

    #[test]
    fn a_desktop_is_offered_only_to_a_profile_that_asks_for_one() {
        // The seed's package list is filled from what was *asked for* and from
        // nothing else: cloud-init writes it before there is a guest to ask
        // what it has, so the found desktop has no say here and never will.
        let ubuntu = ubuntu();
        let desktop = ubuntu
            .desktop_for(DesktopProfile::Gnome)
            .expect("Ubuntu installs GNOME");
        assert_eq!(desktop.packages, ["ubuntu-desktop-minimal"]);
        assert_eq!(ubuntu.desktop_for(DesktopProfile::Headless), None);
    }

    #[test]
    fn a_profile_that_describes_no_desktop_offers_none() {
        let profile = DistroProfile {
            desktop: None,
            ..ubuntu()
        };
        assert_eq!(profile.desktop_for(DesktopProfile::Gnome), None);
    }

    #[test]
    fn a_profile_names_the_units_that_carry_its_ssh_daemon() {
        assert_eq!(ubuntu().ssh.units.all(), ["ssh.socket", "ssh.service"]);
    }

    /// Ubuntu listens through `ssh.socket`, so a port stated only in
    /// `sshd_config` would be read and then ignored.
    #[test]
    fn a_socket_activated_profile_names_both_places_a_port_has_to_be_written() {
        let ssh = ubuntu().ssh;

        assert_eq!(ssh.config_drop_in, "/etc/ssh/sshd_config.d/10-vmlord.conf");
        assert_eq!(
            ssh.units,
            SshUnits::SocketActivated {
                socket: "ssh.socket".into(),
                socket_drop_in: "/etc/systemd/system/ssh.socket.d/10-vmlord.conf".into(),
                service: "ssh.service".into(),
            }
        );
    }

    /// A daemon that opens its own port has one unit and nothing to override
    /// beside it.
    #[test]
    fn a_profile_without_socket_activation_names_only_its_service() {
        let units = SshUnits::Service {
            unit: "sshd.service".into(),
        };

        assert_eq!(units.all(), ["sshd.service"]);
    }

    #[test]
    fn a_directory_template_without_a_trailing_slash_still_joins_cleanly() {
        let profile = DistroProfile {
            directory_template: "http://127.0.0.1:9/{release}".into(),
            ..ubuntu()
        };

        assert_eq!(
            profile.checksums_url("24.04"),
            "http://127.0.0.1:9/24.04/SHA256SUMS",
            "a profile written by hand must not silently produce a glued-together URL"
        );
    }
}
