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

/// The placeholder both templates carry.
const RELEASE_PLACEHOLDER: &str = "{release}";

/// Where a distribution publishes its cloud images, and what the guest inside
/// them looks like.
///
/// The URL is kept as two templates rather than one: the checksum file sits in
/// the same directory as the image, and a single template would have to have its
/// tail cut off to get at that directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DistroProfile {
    pub name: String,
    pub directory_template: String,
    pub file_name_template: String,
    pub checksum_file: String,
    /// The account cloud-init creates in the guest.
    pub default_user: String,
    /// The group that account must join to hold administrative rights.
    pub admin_group: String,
    /// How this distribution runs and configures its SSH daemon.
    pub ssh: SshDaemon,
}

/// How a distribution starts its SSH daemon, and where a setting of VMLord's
/// has to be written for the daemon to read it.
///
/// Data rather than knowledge inside the seed generator: the differences
/// between distributions here are file paths and unit names, and a generator
/// that branched on `"Ubuntu"` would have to be edited for every profile added
/// to a JSON file later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshDaemon {
    /// The systemd units that carry the daemon.
    ///
    /// Debian-family systems socket-activate `ssh.socket` and keep
    /// `ssh.service` beside it, while Fedora and SUSE name both `sshd`. A VM
    /// created with SSH turned off has these disabled on the first boot, and
    /// one created with a port has them restarted after the drop-ins below are
    /// written.
    pub units: Vec<String>,
    /// The drop-in file that overrides the daemon's own configuration.
    ///
    /// A file of VMLord's own rather than an edit of `sshd_config`: a drop-in
    /// is written whole, so nothing has to be found, matched or replaced inside
    /// a file the distribution owns and may change between releases.
    pub config_drop_in: String,
    /// The drop-in that overrides the listening socket, on distributions that
    /// socket-activate the daemon.
    ///
    /// `None` where the daemon opens its own port, in which case
    /// [`Self::config_drop_in`] is the whole story. Where a socket does the
    /// listening, `sshd_config`'s `Port` is ignored -- the daemon is handed a
    /// file descriptor that is already bound -- so both files have to agree.
    pub socket_drop_in: Option<String>,
}

/// Ubuntu's official cloud images.
///
/// The directory is addressed by version number even though the server stores
/// it under the codename: `/releases/24.04/` answers 302 to `/releases/noble/`,
/// so a table of codenames would buy nothing and would need a line added for
/// every future release. The file name, in contrast, does carry the version
/// number rather than the codename -- verified on 24.04 and 22.04.
///
/// The architecture is baked into the template. Hyper-V here is x86_64, and a
/// field with one possible value is no better than an enum with one variant.
#[must_use]
pub fn ubuntu() -> DistroProfile {
    DistroProfile {
        name: "Ubuntu".into(),
        directory_template: "https://cloud-images.ubuntu.com/releases/{release}/release/".into(),
        file_name_template: "ubuntu-{release}-server-cloudimg-amd64.img".into(),
        checksum_file: "SHA256SUMS".into(),
        default_user: "ubuntu".into(),
        admin_group: "sudo".into(),
        ssh: SshDaemon {
            units: vec!["ssh.socket".into(), "ssh.service".into()],
            // `sshd_config` ends with `Include /etc/ssh/sshd_config.d/*.conf`
            // read in name order, and the *first* value of a keyword wins --
            // so the number decides who wins, and `10-` puts VMLord ahead of
            // cloud-init's own `50-cloud-init.conf`.
            config_drop_in: "/etc/ssh/sshd_config.d/10-vmlord.conf".into(),
            // Ubuntu socket-activates the daemon since 22.10, and the unit
            // lives under `/lib`, so the override goes under `/etc` where
            // systemd looks for it second.
            socket_drop_in: Some("/etc/systemd/system/ssh.socket.d/10-vmlord.conf".into()),
        },
    }
}

impl DistroProfile {
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
    use super::{DistroProfile, ubuntu};

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
    fn a_profile_names_the_units_that_carry_its_ssh_daemon() {
        assert_eq!(ubuntu().ssh.units, ["ssh.socket", "ssh.service"]);
    }

    /// Ubuntu listens through `ssh.socket`, so a port stated only in
    /// `sshd_config` would be read and then ignored.
    #[test]
    fn a_socket_activated_profile_names_both_places_a_port_has_to_be_written() {
        let ssh = ubuntu().ssh;

        assert_eq!(ssh.config_drop_in, "/etc/ssh/sshd_config.d/10-vmlord.conf");
        assert_eq!(
            ssh.socket_drop_in.as_deref(),
            Some("/etc/systemd/system/ssh.socket.d/10-vmlord.conf")
        );
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
