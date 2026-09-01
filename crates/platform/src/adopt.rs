//! What adopting a disk is asked for on a command line.
//!
//! Here rather than in the composition root because it is not wiring: which
//! account holds VMLord's key, on what port the guest's daemon answers and how
//! big its disk is are the facts an adoption is made of, and they are checked
//! rather than trusted.

use std::path::PathBuf;

use vmlord_core::{
    DesktopProfile, GpuMode, NetworkMode, Provisioning, SshAccess, SshDaemon, SshPort,
    VmCreateRequest, VmSource,
};

use crate::host_guest_defaults;

/// What an adopted VM gets when the command is not told otherwise.
const DEFAULT_RAM_MB: u32 = 4096;
const DEFAULT_CPU_CORES: u32 = 2;

pub const ADOPT_USAGE: &str = "usage: vmlord adopt-disk --name <name> --disk <path> \
     --username <user> --disk-gb <size> [--ram-mb <size>] [--cpu-cores <count>] \
     [--ssh-port <port>]";

/// One adoption, as a command line describes it.
#[derive(Debug)]
pub struct AdoptArguments {
    name: String,
    disk: PathBuf,
    username: String,
    /// The disk's virtual size, which the operator reads off the source VM's
    /// own configuration. Required rather than defaulted: it is recorded as
    /// the VM's disk size and a later resize is checked against it, so a
    /// number nobody chose would be a number that misreports the disk.
    disk_gb: u32,
    ram_mb: u32,
    cpu_cores: u32,
    /// Absent leaves the guest's daemon on the port it already answers on.
    ssh_port: Option<u16>,
}

impl AdoptArguments {
    /// Reads a command line, refusing anything it cannot name a value for.
    ///
    /// # Errors
    ///
    /// The usage, when a required argument is missing, an argument has no
    /// value, or a number is not one.
    pub fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut name = None;
        let mut disk = None;
        let mut username = None;
        let mut disk_gb = None;
        let mut ram_mb = DEFAULT_RAM_MB;
        let mut cpu_cores = DEFAULT_CPU_CORES;
        let mut ssh_port = None;
        let mut arguments = arguments;
        while let Some(argument) = arguments.next() {
            let mut value = |argument: &str| {
                arguments
                    .next()
                    .ok_or_else(|| format!("{argument} needs a value\n{ADOPT_USAGE}"))
            };
            match argument.as_str() {
                "--name" => name = Some(value("--name")?),
                "--disk" => disk = Some(PathBuf::from(value("--disk")?)),
                "--username" => username = Some(value("--username")?),
                "--disk-gb" => {
                    disk_gb = Some(number(&value("--disk-gb")?, "--disk-gb")?);
                }
                "--ram-mb" => ram_mb = number(&value("--ram-mb")?, "--ram-mb")?,
                "--cpu-cores" => cpu_cores = number(&value("--cpu-cores")?, "--cpu-cores")?,
                "--ssh-port" => ssh_port = Some(number(&value("--ssh-port")?, "--ssh-port")?),
                other => return Err(format!("unknown argument `{other}`\n{ADOPT_USAGE}")),
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| format!("--name is required\n{ADOPT_USAGE}"))?,
            disk: disk.ok_or_else(|| format!("--disk is required\n{ADOPT_USAGE}"))?,
            username: username.ok_or_else(|| format!("--username is required\n{ADOPT_USAGE}"))?,
            disk_gb: disk_gb.ok_or_else(|| format!("--disk-gb is required\n{ADOPT_USAGE}"))?,
            ram_mb,
            cpu_cores,
            ssh_port,
        })
    }

    /// The request the creation pipeline adopts the disk through.
    ///
    /// The guest's locale, keyboard and timezone are the ones already inside
    /// it -- the source application set them from this same host when it built
    /// the VM -- so the request repeats the host's, and nothing of VMLord's
    /// writes them into the guest again.
    ///
    /// `ssh_daemon` comes from the installed distribution profile rather than
    /// from here: how a release carries its SSH daemon is what decides which
    /// two drop-ins move its port, and a copy of that here would be a copy
    /// that falls behind the profiles VMLord ships.
    #[must_use]
    pub fn request(&self, ssh_daemon: SshDaemon) -> VmCreateRequest {
        let host = host_guest_defaults();
        VmCreateRequest {
            name: self.name.clone(),
            source: VmSource::ExistingDisk {
                path: self.disk.to_string_lossy().into_owned(),
                provisioning: Provisioning {
                    username: self.username.clone(),
                    // The guest keeps the password it already has: VMLord holds
                    // no hash for it and has no business setting one.
                    password: None,
                    ssh: SshAccess::Enabled {
                        deploy_key: true,
                        port: self
                            .ssh_port
                            .and_then(|port| SshPort::new(port).ok())
                            .unwrap_or(SshPort::DEFAULT),
                    },
                    locale: host.locale,
                    keyboard: host.keyboard,
                    timezone: host.timezone,
                    // Whatever desktop the guest has, it has: nothing of
                    // VMLord's installs one into an adopted disk.
                    desktop: DesktopProfile::Headless,
                },
                ssh_daemon,
            },
            ram_mb: self.ram_mb,
            disk_gb: self.disk_gb,
            cpu_cores: self.cpu_cores,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::Nat,
        }
    }
}

fn number<T: std::str::FromStr>(value: &str, argument: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("{argument} is not a number\n{ADOPT_USAGE}"))
}

#[cfg(test)]
mod tests {
    use super::{AdoptArguments, DEFAULT_CPU_CORES, DEFAULT_RAM_MB};
    use vmlord_core::{SshAccess, SshPort, VmSource};

    fn parse(arguments: &[&str]) -> Result<AdoptArguments, String> {
        AdoptArguments::parse(arguments.iter().copied().map(ToOwned::to_owned))
    }

    #[test]
    fn every_value_the_adoption_needs_is_parsed() {
        let arguments = parse(&[
            "--name",
            "imported",
            "--disk",
            "D:\\vms\\copied.vhdx",
            "--username",
            "agromov",
            "--disk-gb",
            "200",
            "--ram-mb",
            "32768",
            "--cpu-cores",
            "24",
            "--ssh-port",
            "22",
        ])
        .expect("parsed");

        let request = arguments.request(vmlord_core::ubuntu().ssh);
        assert_eq!(request.name, "imported");
        assert_eq!(request.disk_gb, 200);
        assert_eq!(request.ram_mb, 32768);
        assert_eq!(request.cpu_cores, 24);
        let VmSource::ExistingDisk { provisioning, .. } = &request.source else {
            panic!("an adoption is an existing disk");
        };
        assert_eq!(provisioning.username, "agromov");
        assert!(
            provisioning.password.is_none(),
            "a guest keeps its password"
        );
    }

    #[test]
    fn a_port_left_out_is_the_one_the_guest_already_answers_on() {
        let arguments = parse(&[
            "--name",
            "i",
            "--disk",
            "d.vhdx",
            "--username",
            "a",
            "--disk-gb",
            "64",
        ])
        .expect("parsed");

        assert_eq!(arguments.ram_mb, DEFAULT_RAM_MB);
        assert_eq!(arguments.cpu_cores, DEFAULT_CPU_CORES);
        let request = arguments.request(vmlord_core::ubuntu().ssh);
        let VmSource::ExistingDisk { provisioning, .. } = &request.source else {
            panic!("an adoption is an existing disk");
        };
        assert_eq!(
            provisioning.ssh,
            SshAccess::Enabled {
                deploy_key: true,
                port: SshPort::DEFAULT,
            }
        );
    }

    #[test]
    fn each_required_argument_is_named_when_it_is_missing() {
        for (missing, line) in [
            (
                "--name",
                vec!["--disk", "d.vhdx", "--username", "a", "--disk-gb", "64"],
            ),
            (
                "--disk",
                vec!["--name", "i", "--username", "a", "--disk-gb", "64"],
            ),
            (
                "--username",
                vec!["--name", "i", "--disk", "d.vhdx", "--disk-gb", "64"],
            ),
            (
                "--disk-gb",
                vec!["--name", "i", "--disk", "d.vhdx", "--username", "a"],
            ),
        ] {
            let error = parse(&line).expect_err("refused");
            assert!(error.contains(missing), "{missing}: {error}");
        }
    }

    #[test]
    fn an_argument_without_its_value_is_refused_with_the_usage() {
        let error = parse(&["--disk"]).expect_err("refused");
        assert!(error.contains("--disk needs a value"), "{error}");
    }

    #[test]
    fn a_size_that_is_not_a_number_is_refused() {
        let error = parse(&[
            "--name",
            "i",
            "--disk",
            "d.vhdx",
            "--username",
            "a",
            "--disk-gb",
            "big",
        ])
        .expect_err("refused");
        assert!(error.contains("--disk-gb is not a number"), "{error}");
    }
}
