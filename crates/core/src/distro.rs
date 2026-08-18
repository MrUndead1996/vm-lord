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
#[derive(Clone, Debug, PartialEq, Eq)]
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
            units: SshUnits::SocketActivated {
                socket: "ssh.socket".into(),
                // Ubuntu socket-activates the daemon since 22.10, and the unit
                // lives under `/lib`, so the override goes under `/etc` where
                // systemd looks for it second.
                socket_drop_in: "/etc/systemd/system/ssh.socket.d/10-vmlord.conf".into(),
                service: "ssh.service".into(),
            },
            // `sshd_config` ends with `Include /etc/ssh/sshd_config.d/*.conf`
            // read in name order, and the *first* value of a keyword wins --
            // so the number decides who wins, and `10-` puts VMLord ahead of
            // cloud-init's own `50-cloud-init.conf`.
            config_drop_in: "/etc/ssh/sshd_config.d/10-vmlord.conf".into(),
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
    use super::{DistroProfile, SshUnits, ubuntu};

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
