//! Turning a copied AppSandbox guest into a VMLord one, over SSH, resumably.
//!
//! The conversion runs during a boot nobody watches, on a guest that already
//! answers SSH as somebody else's machine, and it can be interrupted at any
//! point by a crash, a power cut or a person closing VMLord. So it is written
//! as a list of steps rather than as a procedure: each one is idempotent, each
//! one has a check that can be run on its own, and the journal records a step
//! only after the guest has confirmed it.
//!
//! What that buys is the rule a resumption needs. A confirmed step is not done
//! again -- reinstalling an agent that is already installed is wasted time at
//! best -- but every step's check *is* run again, on every pass, because the
//! journal records what VMLord did and only the guest knows what it still is.
//! A disk that was rolled back, a file somebody removed by hand, an install
//! that half-happened: all of those look like a confirmed step in the journal
//! and like a failed check on the guest, and the check is what decides.
//!
//! Nothing here builds a command out of a value from outside VMLord. The
//! remote commands are a fixed set of constants naming a fixed program that was
//! uploaded and hashed; everything that program needs to know about this
//! particular guest -- its user name, the key to install, which payload was
//! chosen -- is in the JSON document beside it. See [`super::bundle`].
//!
//! The source VM is never contacted. What opens the first session is the
//! AppSandbox key, read by `ssh.exe` from the path the source application keeps
//! it at and never copied into VMLord's storage; what it opens is the *copy*,
//! running in a VMLord compute system of its own.

use std::{fmt, path::Path, time::Duration};

use vmlord_core::{RepositoryError, SshEndpoint, Subsystem};
use zeroize::Zeroizing;

use super::{
    bundle::{
        BundleRequest, ConversionBundle, GUEST_BUNDLE_DIRECTORY, GUEST_PROGRAM_NAME,
        GUEST_STAGED_DIRECTORY,
    },
    journal::{ConversionStep, ImportJournal, JournalStage},
};
use crate::ssh::{self, SshCredential, SshInvocation};

/// How long a step waits for the guest to answer at all.
///
/// A connection, not a step: the work behind a step is the guest's and can take
/// as long as unpacking a payload takes, but a guest that has not answered the
/// TCP handshake in this long is a guest that is not there.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// What the guest is asked before anything has been uploaded to it.
///
/// A fixed string with nothing of VMLord's in it, and the only remote command
/// that does not run the uploaded program -- because at this point there is no
/// uploaded program. The three answers arrive in one session rather than three:
/// they describe one guest at one moment, and a payload chosen from facts read
/// a minute apart would be a payload chosen for a guest that never existed.
const OBSERVE_COMMAND: &str = "uname -m; uname -r; cat /etc/os-release";

/// A value that must not appear in a log, a journal or a command.
///
/// The base64 the agent secret travels as. `Debug` says only that there is one;
/// there is no `Display` at all, so the only way to reach the text is
/// [`SecretText::expose`], which is a word a reviewer can search for.
#[derive(Clone)]
pub(crate) struct SecretText(Zeroizing<String>);

impl SecretText {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self(Zeroizing::new(text.into()))
    }

    /// The text itself, for the one caller that has to write it into a file.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretText(<redacted>)")
    }
}

/// What the copied guest said it is, in its own words.
///
/// Read from the guest rather than taken from the source application's
/// configuration: the VM in `vms.cfg` records what someone asked for years ago,
/// and what boots off the copied disk is whatever it has been upgraded into
/// since. The payloads are chosen from this and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GuestIdentity {
    distribution: String,
    release: String,
    architecture: String,
    kernel_release: String,
    pretty_name: String,
}

impl GuestIdentity {
    /// The identity of a guest that answered `machine`, `kernel_release` and an
    /// `/etc/os-release` naming `distribution` and `release`.
    pub(crate) fn observed(
        distribution: &str,
        release: &str,
        machine: &str,
        kernel_release: &str,
        pretty_name: &str,
    ) -> Self {
        Self {
            distribution: distribution.to_ascii_lowercase(),
            release: release.to_owned(),
            architecture: package_architecture(machine),
            kernel_release: kernel_release.to_owned(),
            pretty_name: pretty_name.to_owned(),
        }
    }

    /// Reads what [`OBSERVE_COMMAND`] answered.
    ///
    /// Refused rather than guessed at when a fact is missing: the two payload
    /// catalogs select on the distribution, the release and the architecture,
    /// and a default for any of those would install a payload built for a guest
    /// this is not.
    pub(crate) fn parse(answer: &str) -> Result<Self, RepositoryError> {
        let mut lines = answer.lines();
        let machine = lines.next().unwrap_or_default().trim();
        let kernel_release = lines.next().unwrap_or_default().trim();
        let os_release: Vec<&str> = lines.collect();

        let distribution = os_release_value(&os_release, "ID");
        let release = os_release_value(&os_release, "VERSION_ID");
        let pretty_name = os_release_value(&os_release, "PRETTY_NAME");

        match (machine, kernel_release, distribution, release) {
            ("", _, _, _) | (_, "", _, _) | (_, _, None, _) | (_, _, _, None) => {
                Err(RepositoryError::new(format!(
                    "the copied guest did not say what it is: expected a machine, a kernel \
                     release and an /etc/os-release naming ID and VERSION_ID, and it answered \
                     {answer:?}"
                )))
            }
            (machine, kernel_release, Some(distribution), Some(release)) => Ok(Self::observed(
                &distribution,
                &release,
                machine,
                kernel_release,
                pretty_name.as_deref().unwrap_or_default(),
            )),
        }
    }

    pub(crate) fn distribution(&self) -> &str {
        &self.distribution
    }

    pub(crate) fn release(&self) -> &str {
        &self.release
    }

    pub(crate) fn architecture(&self) -> &str {
        &self.architecture
    }

    pub(crate) fn kernel_release(&self) -> &str {
        &self.kernel_release
    }

    pub(crate) fn pretty_name(&self) -> &str {
        &self.pretty_name
    }
}

/// One `NAME=value` of an `/etc/os-release`, unquoted.
///
/// The file's own quoting rules and nothing more: a value may be bare or in
/// single or double quotes, and both catalogs compare it as text.
fn os_release_value(lines: &[&str], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    lines
        .iter()
        .map(|line| line.trim())
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|value| value.trim_matches(['"', '\'']).to_owned())
        .filter(|value| !value.is_empty())
}

/// The architecture as a package -- and both payload catalogs -- spell it.
///
/// `uname -m` answers with the kernel's name for a machine and the catalogs are
/// published under Debian's, which are different words for the same hardware.
fn package_architecture(machine: &str) -> String {
    match machine.trim() {
        "x86_64" => "amd64".to_owned(),
        "aarch64" => "arm64".to_owned(),
        other => other.to_ascii_lowercase(),
    }
}

/// One labelled thing the conversion asked of the guest.
///
/// The label is what a report, a log and a resumption all name a step by, and
/// it is also the guest program's own name for that step: one word, decided
/// here, so that what a person reads and what the guest ran cannot drift.
#[derive(Debug)]
pub(crate) struct ConversionCommand {
    pub(crate) label: &'static str,
    pub(crate) invocation: SshInvocation,
}

/// What one run of the conversion did.
#[derive(Debug)]
pub(crate) struct ConversionReport {
    /// What the guest said it is, which is what the payloads were chosen for.
    pub(crate) identity: GuestIdentity,
    /// Every command this run issued, in order. A run that skipped a confirmed
    /// step has no command for it, which is how a report says what it resumed.
    pub(crate) commands: Vec<ConversionCommand>,
    pub(crate) last_confirmed_step: Option<ConversionStep>,
}

/// Runs one command in the guest and answers with what it printed.
///
/// A seam rather than a call, so that the order of the steps, the skipping of
/// confirmed ones and the advancing of the journal are all testable without a
/// guest, a network or an `ssh.exe` to run.
pub(crate) type RemoteExecution =
    Box<dyn Fn(&ConversionCommand) -> Result<String, RepositoryError> + Send + Sync>;

/// Everything one conversion is run against.
pub(crate) struct ConversionRequest<'a> {
    /// The copied guest, as the bootstrap boot made it reachable.
    pub(crate) endpoint: &'a SshEndpoint,
    /// The VM directory the copy was written into, which is where the host
    /// keys and the VM's own key pair live.
    pub(crate) vm_directory: &'a Path,
    pub(crate) ssh_client: &'a Path,
    pub(crate) scp_client: &'a Path,
    /// The AppSandbox private key, at the path the source application keeps it
    /// at. Named and never read: see [`SshCredential::AppSandboxBootstrapKey`].
    pub(crate) bootstrap_key: &'a Path,
    pub(crate) release_directory: &'a Path,
    pub(crate) staging_directory: &'a Path,
    pub(crate) agent_binary: &'a Path,
    pub(crate) vmlord_public_key: &'a str,
    pub(crate) agent_secret: &'a SecretText,
}

/// The label of the one command that is a file copy rather than a session.
const UPLOAD_LABEL: &str = "upload-bundle";

/// One step of the conversion, as the runner walks it.
struct Stage {
    step: ConversionStep,
    /// Whether this step begins by copying the bundle in. One step does, and
    /// it is the only command in the conversion that is not a session.
    uploads_bundle: bool,
    /// What is done only when the journal does not already confirm the step.
    actions: &'static [&'static str],
    /// What is asked on every pass, whatever the journal says. `None` only for
    /// the shutdown, which is the one step whose success is the connection
    /// going away.
    check: Option<&'static str>,
}

/// Every step after the observation, which the runner takes before this list
/// because its answer is what the bundle is built from.
const STAGES: [Stage; 9] = [
    Stage {
        step: ConversionStep::BundleUploaded,
        uploads_bundle: true,
        actions: &["install-bundle"],
        check: Some("verify-bundle"),
    },
    Stage {
        step: ConversionStep::VmlordSshKeyDeployed,
        uploads_bundle: false,
        actions: &["deploy-vmlord-key"],
        check: Some("verify-vmlord-key"),
    },
    Stage {
        step: ConversionStep::AgentInstalled,
        uploads_bundle: false,
        actions: &["install-agent"],
        check: Some("verify-agent-files"),
    },
    Stage {
        step: ConversionStep::DisplayPayloadInstalled,
        uploads_bundle: false,
        actions: &["install-display-payload"],
        check: Some("verify-display-payload"),
    },
    Stage {
        step: ConversionStep::GpuPayloadInstalled,
        uploads_bundle: false,
        actions: &["install-gpu-payload"],
        check: Some("verify-gpu-payload"),
    },
    Stage {
        step: ConversionStep::AppSandboxUnitsDisabled,
        uploads_bundle: false,
        actions: &["disable-appsandbox-units"],
        check: Some("verify-appsandbox-units-disabled"),
    },
    // Nothing to do and everything to prove: this is the gate the removal
    // waits behind, so it is a check and only a check.
    Stage {
        step: ConversionStep::ReplacementsValidated,
        uploads_bundle: false,
        actions: &[],
        check: Some("validate-replacements"),
    },
    Stage {
        step: ConversionStep::ObsoleteFilesRemoved,
        uploads_bundle: false,
        actions: &["remove-obsolete-files"],
        check: Some("verify-obsolete-files-removed"),
    },
    // The one step with no check: what would confirm it is the session ending,
    // and a guest that answered a question afterwards would not have taken it.
    Stage {
        step: ConversionStep::ShutdownRequested,
        uploads_bundle: false,
        actions: &["request-shutdown"],
        check: None,
    },
];

/// Walks the conversion's steps against one guest.
pub(crate) struct ConversionRunner<'a> {
    request: ConversionRequest<'a>,
    journal: &'a mut ImportJournal,
    execute: RemoteExecution,
    commands: Vec<ConversionCommand>,
}

impl<'a> ConversionRunner<'a> {
    pub(crate) fn new(
        request: ConversionRequest<'a>,
        journal: &'a mut ImportJournal,
        execute: RemoteExecution,
    ) -> Self {
        Self {
            request,
            journal,
            execute,
            commands: Vec::new(),
        }
    }

    /// Converts the guest, resuming from whatever the journal already confirms.
    ///
    /// The bundle is rebuilt on every pass, including a resumed one. It is
    /// cheap next to the copy that made the import, it is deterministic -- so a
    /// resumption's bundle is the one the guest is already holding -- and it is
    /// what the checks of every confirmed step are run out of.
    pub(crate) fn run(mut self) -> Result<ConversionReport, RepositoryError> {
        self.journal.set_stage(JournalStage::Converting);
        self.journal.save()?;

        let identity = self.observe()?;
        let bundle = ConversionBundle::build(&BundleRequest {
            release_directory: self.request.release_directory,
            staging_directory: self.request.staging_directory,
            agent_binary: self.request.agent_binary,
            guest: &identity,
            guest_username: &self.request.endpoint.username,
            vmlord_public_key: self.request.vmlord_public_key,
            agent_secret: self.request.agent_secret,
        })?;

        for stage in &STAGES {
            let confirmed = self
                .journal
                .last_confirmed_conversion_step()
                .is_some_and(|last| stage.step <= last);
            if !confirmed {
                if stage.uploads_bundle {
                    self.run_command(UPLOAD_LABEL, self.upload(bundle.root()))?;
                }
                for action in stage.actions {
                    self.run_command(action, self.staged_step(action))?;
                }
            }
            if let Some(check) = stage.check {
                self.run_command(check, self.installed_step(check))?;
            }
            self.confirm(stage.step)?;
        }

        let last_confirmed_step = self.journal.last_confirmed_conversion_step();
        vmlord_core::diagnostic!(
            Info,
            Subsystem::Provisioning,
            vm_id = %self.request.endpoint.vm_id,
            "The copied guest ({}) is now a VMLord guest and has been asked to shut down",
            identity.pretty_name()
        );
        Ok(ConversionReport {
            identity,
            commands: self.commands,
            last_confirmed_step,
        })
    }

    /// Asks the guest what it is, and records the step as confirmed.
    fn observe(&mut self) -> Result<GuestIdentity, RepositoryError> {
        let invocation = self.session(Some(OBSERVE_COMMAND));
        let answer = self.run_command("observe-guest", invocation)?;
        let identity = GuestIdentity::parse(&answer)?;
        tracing::info!(
            "the copied guest is {} on {} ({})",
            identity.pretty_name(),
            identity.architecture(),
            identity.kernel_release()
        );
        self.confirm(ConversionStep::GuestObserved)?;
        Ok(identity)
    }

    /// Runs one labelled command, keeping it in the report either way.
    fn run_command(
        &mut self,
        label: &'static str,
        invocation: SshInvocation,
    ) -> Result<String, RepositoryError> {
        let command = ConversionCommand { label, invocation };
        tracing::debug!(
            "converting the copied guest: {label} runs {}",
            command.invocation.command_line()
        );
        let answer = (self.execute)(&command);
        self.commands.push(command);
        answer.map_err(|error| {
            RepositoryError::new(format!(
                "the conversion step \"{label}\" did not succeed on the copied guest: {error}"
            ))
        })
    }

    /// Records `step` as confirmed, durably, before the next one starts.
    fn confirm(&mut self, step: ConversionStep) -> Result<(), RepositoryError> {
        if self
            .journal
            .last_confirmed_conversion_step()
            .is_some_and(|last| last >= step)
        {
            return Ok(());
        }
        self.journal.set_last_confirmed_conversion_step(Some(step));
        self.journal.save()
    }

    /// A bootstrap session running `remote_command`, or an interactive one.
    fn session(&self, remote_command: Option<&str>) -> SshInvocation {
        ssh::invocation_with(
            self.request.ssh_client,
            self.request.endpoint,
            self.request.vm_directory,
            Some(CONNECT_TIMEOUT),
            SshCredential::AppSandboxBootstrapKey(self.request.bootstrap_key),
            remote_command,
        )
    }

    /// The copy that puts the bundle where the guest can root it.
    fn upload(&self, local: &Path) -> SshInvocation {
        ssh::copy_invocation(
            self.request.scp_client,
            self.request.endpoint,
            self.request.vm_directory,
            Some(CONNECT_TIMEOUT),
            SshCredential::AppSandboxBootstrapKey(self.request.bootstrap_key),
            local,
            GUEST_STAGED_DIRECTORY,
        )
    }

    /// One step of the freshly uploaded copy of the program, before it is
    /// rooted.
    fn staged_step(&self, label: &'static str) -> SshInvocation {
        self.session(Some(&guest_step_command(GUEST_STAGED_DIRECTORY, label)))
    }

    /// One step of the installed, root-owned copy of the program.
    fn installed_step(&self, label: &'static str) -> SshInvocation {
        self.session(Some(&guest_step_command(GUEST_BUNDLE_DIRECTORY, label)))
    }
}

/// The remote command for one step.
///
/// Every piece of it is a constant of VMLord's own: the directory, the
/// interpreter, the program's name and the label, which is a `&'static str`
/// from the stage table. Nothing a user, a guest or a source application ever
/// named reaches this string -- those values travel in the bundle's
/// `input.json`, which the program reads.
///
/// `sudo -n` because the work is root's -- units, `/etc`, `/usr/local` -- and
/// `-n` so that a guest whose sudo would prompt fails the step instead of
/// hanging on a password nobody is there to type.
fn guest_step_command(directory: &str, label: &str) -> String {
    format!("sudo -n python3 {directory}/{GUEST_PROGRAM_NAME} {label}")
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use uuid::Uuid;
    use vmlord_core::{
        AppSandboxSourceId, DesktopProfile, GpuMode, RepositoryError, SshAuthentication, SshConfig,
        SshEndpoint, SshPort,
    };

    use super::{ConversionReport, ConversionRequest, ConversionRunner, GuestIdentity, SecretText};
    use crate::appsandbox::{
        bundle::test_release_directory,
        journal::{
            BootstrapSshFacts, ConversionStep, ImportJournal, ImportJournalDetails,
            ImportResources, SourceFingerprint,
        },
    };

    /// What the observation command's own output looks like coming back from a
    /// guest that has not been touched yet.
    const OBSERVED: &str = "x86_64\n6.8.0-31-generic\nID=ubuntu\nVERSION_ID=\"24.04\"\n\
                            PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\n";

    /// Every label one complete conversion asks of a guest, in order.
    const EVERY_LABEL: [&str; 18] = [
        "observe-guest",
        "upload-bundle",
        "install-bundle",
        "verify-bundle",
        "deploy-vmlord-key",
        "verify-vmlord-key",
        "install-agent",
        "verify-agent-files",
        "install-display-payload",
        "verify-display-payload",
        "install-gpu-payload",
        "verify-gpu-payload",
        "disable-appsandbox-units",
        "verify-appsandbox-units-disabled",
        "validate-replacements",
        "remove-obsolete-files",
        "verify-obsolete-files-removed",
        "request-shutdown",
    ];

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temporary_root(label: &str) -> TempRoot {
        let path = std::env::temp_dir().join(format!(
            "vmlord-appsandbox-conversion-{label}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        TempRoot(path)
    }

    fn vm_id() -> Uuid {
        Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef)
    }

    fn endpoint() -> SshEndpoint {
        SshEndpoint::new(
            vm_id(),
            &SshConfig {
                username: "sandbox".into(),
                port: SshPort::new(2222).unwrap(),
                // What the bootstrap mapping records; the first session must
                // not take it at its word.
                authentication: SshAuthentication::VmlordKey,
            },
            "172.22.42.7".parse().unwrap(),
        )
        .unwrap()
    }

    fn journal_details(destination: PathBuf) -> ImportJournalDetails {
        ImportJournalDetails {
            import_id: Uuid::from_u128(21),
            source_fingerprint: SourceFingerprint {
                source_id: AppSandboxSourceId::from_stable_hash("source-21").unwrap(),
                disk_path: PathBuf::from(r"C:\ProgramData\AppSandbox\ubuntu\disk.vhdx"),
                vm_ordinal: 1,
            },
            destination,
            requested_resources: ImportResources {
                ram_mb: 4096,
                cpu_cores: 4,
                disk_gb: 80,
                desktop_profile: DesktopProfile::Gnome,
            },
            desired_gpu: GpuMode::Default,
            bootstrap_ssh: BootstrapSshFacts {
                username: "sandbox".to_owned(),
                port: 2222,
            },
        }
    }

    /// Everything one conversion is run against, kept alive for the run.
    struct Fixture {
        root: TempRoot,
        release: PathBuf,
        agent: PathBuf,
        endpoint: SshEndpoint,
        ssh_client: PathBuf,
        scp_client: PathBuf,
        bootstrap_key: PathBuf,
        vm_directory: PathBuf,
        secret: SecretText,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = temporary_root(label);
            let release = test_release_directory(&root.0);
            let agent = root.0.join("vmlord-agent");
            fs::write(&agent, b"static musl agent").unwrap();
            let vm_directory = root.0.join("imported");
            fs::create_dir_all(&vm_directory).unwrap();
            Self {
                release,
                agent,
                endpoint: endpoint(),
                ssh_client: PathBuf::from(r"C:\Windows\System32\OpenSSH\ssh.exe"),
                scp_client: PathBuf::from(r"C:\Windows\System32\OpenSSH\scp.exe"),
                bootstrap_key: PathBuf::from(
                    r"C:\ProgramData\AppSandbox\keys\id_ed25519_appsandbox",
                ),
                vm_directory,
                secret: SecretText::new("c2VjcmV0"),
                root,
            }
        }

        fn request<'a>(&'a self, staging: &'a Path) -> ConversionRequest<'a> {
            ConversionRequest {
                endpoint: &self.endpoint,
                vm_directory: &self.vm_directory,
                ssh_client: &self.ssh_client,
                scp_client: &self.scp_client,
                bootstrap_key: &self.bootstrap_key,
                release_directory: &self.release,
                staging_directory: staging,
                agent_binary: &self.agent,
                vmlord_public_key: "ssh-ed25519 AAAAC3Nz vmlord",
                agent_secret: &self.secret,
            }
        }
    }

    /// A conversion whose journal already confirms `resume_from`, run against a
    /// guest that answers everything.
    fn run_resuming_from(
        fixture: &Fixture,
        resume_from: Option<ConversionStep>,
    ) -> Result<ConversionReport, RepositoryError> {
        let storage_root = fixture.root.0.join("storage");
        fs::create_dir_all(&storage_root).unwrap();
        let destination = storage_root.join("imported");
        let mut journal =
            ImportJournal::create(&storage_root, journal_details(destination)).unwrap();
        journal.set_last_confirmed_conversion_step(resume_from);
        journal.save().unwrap();

        let staging = fixture.root.0.join("staging");
        fs::create_dir_all(&staging).unwrap();
        ConversionRunner::new(
            fixture.request(&staging),
            &mut journal,
            Box::new(|command| {
                Ok(if command.label == "observe-guest" {
                    OBSERVED.to_owned()
                } else {
                    String::new()
                })
            }),
        )
        .run()
    }

    fn labels(report: &ConversionReport) -> Vec<&str> {
        report
            .commands
            .iter()
            .map(|command| command.label)
            .collect()
    }

    #[test]
    fn a_conversion_records_what_the_guest_itself_said_it_is() {
        let fixture = Fixture::new("observe");

        let report = run_resuming_from(&fixture, None).unwrap();

        assert_eq!(report.identity.distribution(), "ubuntu");
        assert_eq!(report.identity.release(), "24.04");
        assert_eq!(
            report.identity.architecture(),
            "amd64",
            "the payload catalogs name the architecture the way a package does"
        );
        assert_eq!(report.identity.kernel_release(), "6.8.0-31-generic");
        assert_eq!(report.identity.pretty_name(), "Ubuntu 24.04.1 LTS");
        assert_eq!(
            labels(&report).first().copied(),
            Some("observe-guest"),
            "nothing is chosen before the guest has said what it is"
        );
    }

    #[test]
    fn a_first_run_walks_every_step_in_order_and_leaves_the_last_one_confirmed() {
        let fixture = Fixture::new("ordered");

        let report = run_resuming_from(&fixture, None).unwrap();

        assert_eq!(labels(&report), EVERY_LABEL.to_vec());
        assert_eq!(
            report.last_confirmed_step,
            Some(ConversionStep::ShutdownRequested)
        );
    }

    #[test]
    fn replay_skips_confirmed_steps_but_revalidates_their_postconditions() {
        let fixture = Fixture::new("replay");

        let report = run_resuming_from(&fixture, Some(ConversionStep::AgentInstalled)).unwrap();

        assert!(
            !report
                .commands
                .iter()
                .any(|command| command.label == "install-agent")
        );
        assert!(
            report
                .commands
                .iter()
                .any(|command| command.label == "verify-agent-files")
        );
    }

    /// The rule the whole resumption rests on: a confirmed step is never done
    /// again, and every step's postcondition is checked again anyway, because
    /// the journal records what VMLord did and not what the guest still is.
    #[test]
    fn a_replay_from_any_confirmed_step_re_runs_every_check_and_no_earlier_action() {
        for step in ConversionStep::ALL {
            let fixture = Fixture::new("resumption");
            let report = run_resuming_from(&fixture, Some(step)).unwrap();
            let seen = labels(&report);

            for label in EVERY_LABEL {
                let is_check = label.starts_with("verify-")
                    || label == "observe-guest"
                    || label == "validate-replacements";
                if is_check {
                    assert!(seen.contains(&label), "{step:?} must still check {label}");
                }
            }
            assert!(
                !seen.contains(&"install-agent") || step < ConversionStep::AgentInstalled,
                "{step:?} re-ran an action it had already confirmed: {seen:?}"
            );
            assert_eq!(
                report.last_confirmed_step,
                Some(ConversionStep::ShutdownRequested)
            );
        }
    }

    /// The first session is opened with the source application's own key, at
    /// the path that application keeps it -- never with the VM's own key, which
    /// the guest has not been given yet.
    #[test]
    fn the_bootstrap_session_offers_the_appsandbox_key_from_its_own_path() {
        let fixture = Fixture::new("bootstrap-key");

        let report = run_resuming_from(&fixture, None).unwrap();

        for command in &report.commands {
            let arguments: Vec<String> = command
                .invocation
                .args
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect();
            assert!(
                arguments
                    .contains(&r"C:\ProgramData\AppSandbox\keys\id_ed25519_appsandbox".to_owned()),
                "{}: {arguments:?}",
                command.label
            );
            assert!(
                !arguments
                    .iter()
                    .any(|argument| argument.contains("id_ed25519\"")
                        || argument.ends_with("keys\\id_ed25519")),
                "the VM's own key cannot open a session the conversion has not created yet: \
                 {arguments:?}"
            );
        }
    }

    /// Every remote command is one of a fixed set of constants. A name, a path
    /// or a key that came from outside VMLord is never part of one.
    #[test]
    fn no_remote_command_is_built_out_of_a_value_from_outside_vmlord() {
        let fixture = Fixture::new("shell-free");

        let report = run_resuming_from(&fixture, None).unwrap();

        for command in &report.commands {
            let line = command.invocation.command_line();
            assert!(
                !line.contains("ssh-ed25519 AAAAC3Nz vmlord"),
                "{}: {line}",
                command.label
            );
            assert!(!line.contains("c2VjcmV0"), "{}: {line}", command.label);
            assert!(
                !line.to_lowercase().contains("powershell") && !line.contains("cmd.exe"),
                "{}: {line}",
                command.label
            );
        }
    }

    /// A step whose postcondition the guest cannot confirm stops the
    /// conversion where it is, so a resumed run starts from the last step that
    /// really happened rather than from the one VMLord had hoped for.
    #[test]
    fn a_step_whose_check_fails_leaves_the_journal_at_the_last_confirmed_one() {
        let fixture = Fixture::new("failure");
        let storage_root = fixture.root.0.join("storage");
        fs::create_dir_all(&storage_root).unwrap();
        let mut journal = ImportJournal::create(
            &storage_root,
            journal_details(storage_root.join("imported")),
        )
        .unwrap();
        let staging = fixture.root.0.join("staging");
        fs::create_dir_all(&staging).unwrap();

        let error = ConversionRunner::new(
            fixture.request(&staging),
            &mut journal,
            Box::new(|command| match command.label {
                "observe-guest" => Ok(OBSERVED.to_owned()),
                "verify-agent-files" => Err(RepositoryError::new("the agent unit is not enabled")),
                _ => Ok(String::new()),
            }),
        )
        .run()
        .expect_err("a guest that cannot confirm a step has not taken it");

        assert!(error.to_string().contains("verify-agent-files"), "{error}");
        assert_eq!(
            journal.last_confirmed_conversion_step(),
            Some(ConversionStep::VmlordSshKeyDeployed),
            "the failed step is not recorded as confirmed"
        );
    }

    /// The one property a secret type exists for: nothing that prints it prints
    /// it.
    #[test]
    fn a_secret_does_not_reveal_itself_in_its_own_debug_output() {
        let secret = SecretText::new("c2VjcmV0LXZhbHVl");

        let printed = format!("{secret:?}");

        assert!(!printed.contains("c2VjcmV0LXZhbHVl"), "{printed}");
        assert!(printed.contains("redacted"), "{printed}");
    }

    #[test]
    fn an_architecture_the_kernel_spells_its_own_way_becomes_the_catalogs_name() {
        for (machine, expected) in [
            ("x86_64", "amd64"),
            ("amd64", "amd64"),
            ("aarch64", "arm64"),
            ("arm64", "arm64"),
        ] {
            let identity =
                GuestIdentity::observed("ubuntu", "24.04", machine, "6.8.0-31-generic", "Ubuntu");

            assert_eq!(identity.architecture(), expected);
        }
    }

    /// `/etc/os-release` is a document with quoting rules of its own, and a
    /// guest that does not have one is not a guest this conversion understands.
    #[test]
    fn an_unreadable_observation_is_refused_rather_than_guessed_at() {
        for answer in [
            "",
            "x86_64\n",
            "x86_64\n6.8.0-31\nPRETTY_NAME=\"nothing\"\n",
        ] {
            assert!(
                GuestIdentity::parse(answer).is_err(),
                "\"{answer}\" is not something to convert a guest on"
            );
        }

        let identity = GuestIdentity::parse(OBSERVED).unwrap();
        assert_eq!(identity.distribution(), "ubuntu");
        assert_eq!(identity.release(), "24.04");
    }
}
