//! The files one conversion sends into a copied guest, bound to their hashes.
//!
//! Two things travel to a guest that is about to stop being an AppSandbox one:
//! a fixed program that does the work, and the values that program acts on.
//! Keeping them apart is the whole design. The program is the same bytes for
//! every import and is hashed by the host before root ever runs it; the values
//! -- a user name, a public key, where the agent's unit goes -- are a JSON
//! document the program reads. Nothing a person named ever becomes part of a
//! command, on either side of the connection.
//!
//! What the bundle carries is decided here and nowhere else: the agent VMLord
//! ships and the agent secret this VM was minted. No display or GPU payload:
//! those live on the host and reach a guest through the Plan9 shares its
//! compute system offers, which is how a created VM gets them and is what the
//! second boot will do for this one. A payload unpacked into the guest's own
//! disk would be a second delivery route for an artifact that already has one.
//!
//! The names the agent is installed under are `vmlord-seed`'s, not this
//! module's. A seed is simply not the only thing that installs an agent -- an
//! imported guest never runs cloud-init -- and two spellings of one unit would
//! be two units that could diverge, with the imported guest still passing every
//! check while running the older one.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};

use serde::Serialize;
use vmlord_agent_protocol::auth::GUEST_SECRET_PATH;
use vmlord_core::RepositoryError;
use vmlord_payload::Sha256Digest;
use vmlord_seed::{AGENT_SERVICE, AGENT_SERVICE_NAME, AGENT_SERVICE_PATH, GUEST_AGENT_PATH};

use super::conversion::SecretText;

/// Where the bundle waits in the guest between the upload and the reboot.
///
/// Under `/var/lib` and not `/tmp`: a conversion resumes after a boot that went
/// wrong, and every step after the upload verifies itself against the manifest
/// that travelled with it. A bundle on a tmpfs would be gone exactly when a
/// resumption needs it.
pub(crate) const GUEST_BUNDLE_DIRECTORY: &str = "/var/lib/vmlord/convert";

/// Where the upload lands, relative to the bootstrap user's home directory.
///
/// The home directory and not `/tmp`, which is the difference between a
/// directory only this login can write and one every local account can. Root is
/// about to run what lands here, and `/tmp` would let any other account on the
/// guest pre-create the directory and choose that program.
///
/// Relative, so that the user's name -- which VMLord did not choose -- is never
/// part of a path VMLord spells. `scp` resolves it against the home directory of
/// the account it logs in as, and the remote commands reach it through the
/// shell's `~`, which expands to the same place for the same reason.
pub(crate) const GUEST_STAGED_DIRECTORY: &str = ".vmlord-convert";

/// The staged directory as a remote command names it.
pub(crate) const GUEST_STAGED_PATH: &str = "~/.vmlord-convert";

/// The name of the guest program, which is also how every step names it.
pub(crate) const GUEST_PROGRAM_NAME: &str = "vmlord-convert";

const MANIFEST_NAME: &str = "manifest.json";
const INPUT_NAME: &str = "input.json";
const AGENT_NAME: &str = "vmlord-agent";
const AGENT_SECRET_NAME: &str = "agent.secret";

/// The AppSandbox guest units this conversion stops and disables.
///
/// Exactly the units the source application's own installer enables -- its
/// agent, the three daemons that agent's units order themselves after, and the
/// service its DRM module ships to evict `simpledrm`. A unit missing from this
/// list is a daemon left running against VMLord's; a unit that does not belong
/// on it is somebody else's service stopped for no reason.
const APPSANDBOX_UNITS: [&str; 5] = [
    "appsandbox-agent.service",
    "appsandbox-audio.service",
    "appsandbox-display.service",
    "appsandbox-input.service",
    "asb-evict-simpledrm.service",
];

/// The AppSandbox files this conversion removes once VMLord's own have been
/// validated in their place.
///
/// The binaries the source application installs under `/usr/local/bin`, the
/// units that start them, and the module configuration that makes its DRM
/// driver own the guest's display. Not its DKMS sources: removing those is
/// `dkms remove`'s business, and the program does that by name rather than by
/// deleting a directory tree it guessed at.
const OBSOLETE_APPSANDBOX_PATHS: [&str; 12] = [
    "/usr/local/bin/appsandbox-agent",
    "/usr/local/bin/appsandbox-audio",
    "/usr/local/bin/appsandbox-clipboard",
    "/usr/local/bin/appsandbox-display",
    "/usr/local/bin/appsandbox-input",
    "/etc/systemd/system/appsandbox-agent.service",
    "/etc/systemd/system/appsandbox-audio.service",
    "/etc/systemd/system/appsandbox-display.service",
    "/etc/systemd/system/appsandbox-input.service",
    "/etc/systemd/system/asb-evict-simpledrm.service",
    "/etc/modprobe.d/asb_drm.conf",
    "/etc/modules-load.d/asb_drm.conf",
];

/// Where VMLord's own network configuration goes in an imported guest.
///
/// A higher number than the source application's `99-appsandbox.yaml` would
/// merge with it rather than replace it, so the handover removes that file
/// instead of outranking it, and this one is numbered like any other.
const GUEST_NETWORK_CONFIG_PATH: &str = "/etc/netplan/90-vmlord.yaml";

/// What the source application's networking consists of, all of which goes.
///
/// The netplan file pins a static address on the subnet that application was
/// serving, and the drop-in stops cloud-init from ever writing one of its own.
/// Together they are why an imported guest would come up on an address nothing
/// assigned it and answer at none that HNS did.
const APPSANDBOX_NETWORK_PATHS: [&str; 2] = [
    "/etc/netplan/99-appsandbox.yaml",
    "/etc/cloud/cloud.cfg.d/99-disable-network-config.cfg",
];

/// VMLord's network configuration for an imported guest, bar its renderer.
///
/// DHCP, because that is how every VMLord guest gets its address: HNS assigns
/// one to the VM's endpoint and VMLord's DHCP server offers the guest that one
/// and no other. A guest with an address written into its own configuration
/// answers on whatever it was given once and on nothing afterwards.
///
/// `$RENDERER` is the one thing the guest fills in, because only the guest
/// knows whether NetworkManager is running in it -- and a netplan naming the
/// renderer that is not active makes the one that is stop managing the
/// interface entirely.
const GUEST_NETWORK_CONFIG: &str = "network:
  version: 2
  renderer: $RENDERER
  ethernets:
    vmlordnic:
      match: { name: \"e*\" }
      dhcp4: true
      dhcp6: false
";

/// The document the guest program takes every outside value from.
///
/// The agent's four names are here rather than in the program for the reason
/// the unit and path lists are: they are data VMLord owns, they come from
/// `vmlord-seed` and `vmlord-agent-protocol`, and a copy of them inside the
/// program would be a copy that could fall behind the originals unnoticed.
#[derive(Debug, Serialize)]
struct GuestInput<'a> {
    guest_username: &'a str,
    vmlord_public_key: &'a str,
    agent_binary_path: &'a str,
    agent_secret_path: &'a str,
    agent_unit_name: &'a str,
    agent_unit_path: &'a str,
    agent_unit_text: &'a str,
    appsandbox_units: [&'a str; 5],
    obsolete_paths: [&'a str; 12],
    network_config_path: &'a str,
    network_config_template: &'a str,
    appsandbox_network_paths: [&'a str; 2],
}

/// One file the bundle carries, as the guest checks it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct BundleEntry {
    name: String,
    sha256: String,
    size: u64,
}

impl BundleEntry {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }

    #[allow(dead_code)] // Read by the guest program rather than by the host.
    pub(crate) fn size(&self) -> u64 {
        self.size
    }
}

/// The manifest as it is written, which is the only thing whose shape matters.
#[derive(Debug, Serialize)]
struct Manifest<'a> {
    files: &'a [BundleEntry],
}

/// What building one bundle needs.
pub(crate) struct BundleRequest<'a> {
    /// The import's own staging directory. The bundle is built in a child of
    /// it, so an interrupted import leaves it where a resumption looks.
    pub(crate) staging_directory: &'a Path,
    /// VMLord's Linux agent, as the release ships it beside the executable.
    pub(crate) agent_binary: &'a Path,
    /// The guest user the bootstrap session connects as, and whose
    /// `authorized_keys` the VM's own key is installed into.
    pub(crate) guest_username: &'a str,
    /// The public half of the key pair the bootstrap already generated for this
    /// VM.
    pub(crate) vmlord_public_key: &'a str,
    /// This VM's agent secret, which travels as a file of its own so that it is
    /// never a value in a command, a log or the manifest.
    pub(crate) agent_secret: &'a SecretText,
}

/// A built bundle: a directory of files and the manifest that binds them.
#[derive(Debug)]
pub(crate) struct ConversionBundle {
    root: PathBuf,
    entries: Vec<BundleEntry>,
}

impl ConversionBundle {
    /// Assembles the bundle for `request` and writes its manifest.
    ///
    /// The agent is checked before anything is written: an import that cannot
    /// install one stops here rather than inside a guest that has already had
    /// half of AppSandbox removed.
    pub(crate) fn build(request: &BundleRequest<'_>) -> Result<Self, RepositoryError> {
        if !request.agent_binary.is_file() {
            return Err(RepositoryError::new(format!(
                "the VMLord agent is not at {}, so there is no {AGENT_NAME} to install in the \
                 copied guest",
                request.agent_binary.display()
            )));
        }

        let root = request.staging_directory.join("bundle");
        replace_directory(&root)?;

        write_file(&root.join(GUEST_PROGRAM_NAME), GUEST_PROGRAM.as_bytes())?;
        write_file(
            &root.join(INPUT_NAME),
            &serde_json::to_vec_pretty(&GuestInput {
                guest_username: request.guest_username,
                vmlord_public_key: request.vmlord_public_key,
                agent_binary_path: GUEST_AGENT_PATH,
                agent_secret_path: GUEST_SECRET_PATH,
                agent_unit_name: AGENT_SERVICE_NAME,
                agent_unit_path: AGENT_SERVICE_PATH,
                agent_unit_text: AGENT_SERVICE,
                appsandbox_units: APPSANDBOX_UNITS,
                obsolete_paths: OBSOLETE_APPSANDBOX_PATHS,
                network_config_path: GUEST_NETWORK_CONFIG_PATH,
                network_config_template: GUEST_NETWORK_CONFIG,
                appsandbox_network_paths: APPSANDBOX_NETWORK_PATHS,
            })
            .map_err(|error| {
                RepositoryError::new(format!(
                    "the conversion input could not be written: {error}"
                ))
            })?,
        )?;
        write_file(
            &root.join(AGENT_SECRET_NAME),
            request.agent_secret.expose().as_bytes(),
        )?;
        copy_file(request.agent_binary, &root.join(AGENT_NAME))?;

        let mut entries = Vec::new();
        for name in [
            AGENT_SECRET_NAME,
            INPUT_NAME,
            AGENT_NAME,
            GUEST_PROGRAM_NAME,
        ] {
            entries.push(entry_for(&root, name)?);
        }
        // Sorted, so that the same inputs produce the same document. A resumed
        // conversion builds the bundle again and uploads it again, and a
        // manifest whose order depended on how a directory happened to list
        // would make one bundle into two.
        entries.sort_by(|left, right| left.name.cmp(&right.name));

        let manifest =
            serde_json::to_vec_pretty(&Manifest { files: &entries }).map_err(|error| {
                RepositoryError::new(format!(
                    "the conversion manifest could not be written: {error}"
                ))
            })?;
        write_file(&root.join(MANIFEST_NAME), &manifest)?;

        tracing::info!(
            "the conversion bundle at {} carries {} files",
            root.display(),
            entries.len()
        );
        Ok(Self { root, entries })
    }

    /// The host directory the bundle was built in, which is what gets uploaded.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn entries(&self) -> &[BundleEntry] {
        &self.entries
    }

    /// The digest of the guest program, as the host computed it.
    ///
    /// What the delivery compares the uploaded copy against. Held by the host
    /// and never sent to the guest: a digest that travelled with the upload
    /// would prove only that the upload agrees with itself.
    pub(crate) fn program_sha256(&self) -> &str {
        self.entries
            .iter()
            .find(|entry| entry.name == GUEST_PROGRAM_NAME)
            .map(BundleEntry::sha256)
            .expect("the bundle always carries its own program")
    }

    pub(crate) fn manifest_path(&self) -> PathBuf {
        self.root.join(MANIFEST_NAME)
    }
}

/// An empty directory at `path`, whatever was there before.
///
/// A rebuild after an interrupted attempt must not inherit a half-written file:
/// what the manifest binds is what this build put there, and nothing else.
fn replace_directory(path: &Path) -> Result<(), RepositoryError> {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(RepositoryError::new(format!(
                "the conversion bundle at {} could not be cleared: {error}",
                path.display()
            )));
        }
    }
    fs::create_dir_all(path).map_err(|error| {
        RepositoryError::new(format!(
            "the conversion bundle directory {} could not be created: {error}",
            path.display()
        ))
    })
}

fn write_file(path: &Path, contents: &[u8]) -> Result<(), RepositoryError> {
    fs::write(path, contents).map_err(|error| {
        RepositoryError::new(format!(
            "the conversion bundle file {} could not be written: {error}",
            path.display()
        ))
    })
}

fn copy_file(from: &Path, to: &Path) -> Result<(), RepositoryError> {
    fs::copy(from, to).map(|_| ()).map_err(|error| {
        RepositoryError::new(format!(
            "{} could not be copied into the conversion bundle: {error}",
            from.display()
        ))
    })
}

fn entry_for(root: &Path, name: &str) -> Result<BundleEntry, RepositoryError> {
    let path = root.join(name);
    let size = fs::metadata(&path)
        .map_err(|error| {
            RepositoryError::new(format!(
                "the conversion bundle file {} could not be measured: {error}",
                path.display()
            ))
        })?
        .len();
    let file = File::open(&path).map_err(|error| {
        RepositoryError::new(format!(
            "the conversion bundle file {} could not be read back: {error}",
            path.display()
        ))
    })?;
    let sha256 = Sha256Digest::hash_reader(file).map_err(|error| {
        RepositoryError::new(format!(
            "the conversion bundle file {} could not be hashed: {error}",
            path.display()
        ))
    })?;
    Ok(BundleEntry {
        name: name.to_owned(),
        sha256: sha256.as_hex().to_owned(),
        size,
    })
}

/// The one program that runs inside the guest, byte for byte the same in every
/// import.
///
/// Fixed on purpose. The host hashes the uploaded copy against the bytes it
/// wrote before root runs it, and it takes every name, path and key it acts on
/// from `input.json` beside it -- so the host never has to build a command out
/// of a value it did not choose, and the guest never has to parse one.
///
/// Python 3 rather than a shell: the input is JSON, and a shell that parsed
/// JSON would be a parser written twice and wrongly. Every guest this
/// conversion supports ships Python 3, because cloud-init -- which provisioned
/// it -- is written in it.
const GUEST_PROGRAM: &str = include_str!("convert.py");

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use serde_json::json;
    use uuid::Uuid;
    use vmlord_payload::Sha256Digest;

    use super::{BundleRequest, ConversionBundle, GUEST_BUNDLE_DIRECTORY};
    use crate::appsandbox::conversion::SecretText;

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temporary_root(label: &str) -> TempRoot {
        let path = std::env::temp_dir().join(format!(
            "vmlord-appsandbox-bundle-{label}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        TempRoot(path)
    }

    fn agent_binary(root: &Path) -> PathBuf {
        let path = root.join("vmlord-agent");
        fs::write(&path, b"static musl agent").unwrap();
        path
    }

    fn request<'a>(
        staging: &'a Path,
        agent: &'a Path,
        secret: &'a SecretText,
    ) -> BundleRequest<'a> {
        BundleRequest {
            staging_directory: staging,
            agent_binary: agent,
            guest_username: "sandbox",
            vmlord_public_key: "ssh-ed25519 AAAAC3Nz vmlord",
            agent_secret: secret,
        }
    }

    fn build_in(root: &Path, label: &str) -> ConversionBundle {
        let staging = root.join(label);
        fs::create_dir_all(&staging).unwrap();
        let agent = agent_binary(root);
        let secret = SecretText::new("c2VjcmV0");
        ConversionBundle::build(&request(&staging, &agent, &secret)).unwrap()
    }

    #[test]
    fn a_bundle_binds_every_file_it_carries_to_a_hash_a_guest_can_check() {
        let root = temporary_root("manifest");
        let bundle = build_in(&root.0, "first");

        let names: Vec<&str> = bundle.entries().iter().map(|entry| entry.name()).collect();
        assert_eq!(
            names,
            vec![
                "agent.secret",
                "input.json",
                "vmlord-agent",
                "vmlord-convert"
            ],
            "every file the guest installs is bound to the manifest, in one stable order"
        );
        for entry in bundle.entries() {
            let bytes = fs::read(bundle.root().join(entry.name())).unwrap();
            assert_eq!(
                entry.sha256(),
                Sha256Digest::hash_reader(bytes.as_slice())
                    .unwrap()
                    .as_hex(),
                "{} is bound to the hash of what was written",
                entry.name()
            );
        }
        assert!(
            bundle.manifest_path().is_file(),
            "the guest verifies itself against a manifest that travels with it"
        );
    }

    /// Two builds of the same inputs must produce the same manifest, or a
    /// resumed conversion could not tell an unchanged bundle from a tampered
    /// one.
    #[test]
    fn a_manifest_is_the_same_document_for_the_same_inputs() {
        let root = temporary_root("deterministic");
        let first = build_in(&root.0, "first");
        let second = build_in(&root.0, "second");

        assert_ne!(first.root(), second.root());
        assert_eq!(
            fs::read(first.manifest_path()).unwrap(),
            fs::read(second.manifest_path()).unwrap()
        );
    }

    #[test]
    fn a_build_without_the_agent_binary_names_the_file_it_could_not_find() {
        let root = temporary_root("no-agent");
        let staging = root.0.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let missing = root.0.join("nowhere").join("vmlord-agent");
        let secret = SecretText::new("c2VjcmV0");

        let error = ConversionBundle::build(&request(&staging, &missing, &secret))
            .expect_err("a conversion that cannot install an agent is not one to start");

        assert!(error.to_string().contains("vmlord-agent"), "{error}");
    }

    /// The guest-side program takes every name and path it needs from this
    /// document, so that nothing a user chose is ever part of a command.
    #[test]
    fn the_user_chosen_values_travel_in_a_json_document_rather_than_a_command() {
        let root = temporary_root("input");
        let bundle = build_in(&root.0, "first");

        let input: serde_json::Value =
            serde_json::from_slice(&fs::read(bundle.root().join("input.json")).unwrap()).unwrap();

        assert_eq!(input["guest_username"], json!("sandbox"));
        assert_eq!(
            input["vmlord_public_key"],
            json!("ssh-ed25519 AAAAC3Nz vmlord")
        );
        // The agent's own names are VMLord's, taken from the crates that
        // publish them rather than spelled again here or in the program.
        assert_eq!(
            input["agent_secret_path"],
            json!(vmlord_agent_protocol::auth::GUEST_SECRET_PATH)
        );
        assert_eq!(
            input["agent_unit_path"],
            json!(vmlord_seed::AGENT_SERVICE_PATH)
        );
        assert_eq!(
            input["agent_unit_name"],
            json!(vmlord_seed::AGENT_SERVICE_NAME)
        );
        assert_eq!(input["agent_unit_text"], json!(vmlord_seed::AGENT_SERVICE));
        assert_eq!(
            input["agent_binary_path"],
            json!(vmlord_seed::GUEST_AGENT_PATH)
        );
    }

    /// The units to stop are data the guest program is handed rather than
    /// knowledge baked into it, and they are exactly AppSandbox's own: a unit
    /// An imported guest that keeps the source application's networking is a VM
    /// that answers at the address it was handed once and at none afterwards:
    /// HNS assigns a new one to the VM's endpoint on every start, and VMLord
    /// offers it over DHCP to a guest that has been told not to ask.
    #[test]
    fn the_input_document_puts_the_guest_back_on_dhcp_and_names_what_it_replaces() {
        let root = temporary_root("network");
        let bundle = build_in(&root.0, "first");

        let input: serde_json::Value =
            serde_json::from_slice(&fs::read(bundle.root().join("input.json")).unwrap()).unwrap();

        let template = input["network_config_template"].as_str().unwrap();
        assert!(template.contains("dhcp4: true"), "{template}");
        assert!(
            template.contains("$RENDERER"),
            "only the guest knows which renderer is running in it: {template}"
        );
        assert_eq!(
            input["network_config_path"],
            json!("/etc/netplan/90-vmlord.yaml")
        );
        assert_eq!(
            input["appsandbox_network_paths"],
            json!([
                "/etc/netplan/99-appsandbox.yaml",
                "/etc/cloud/cloud.cfg.d/99-disable-network-config.cfg",
            ]),
            "the static address and the drop-in that stops cloud-init both go"
        );
    }

    /// this list got wrong is either a daemon left running against VMLord or a
    /// guest service stopped for no reason.
    #[test]
    fn the_input_document_names_the_exact_appsandbox_units_to_disable() {
        let root = temporary_root("units");
        let bundle = build_in(&root.0, "first");

        let input: serde_json::Value =
            serde_json::from_slice(&fs::read(bundle.root().join("input.json")).unwrap()).unwrap();

        assert_eq!(
            input["appsandbox_units"],
            json!([
                "appsandbox-agent.service",
                "appsandbox-audio.service",
                "appsandbox-display.service",
                "appsandbox-input.service",
                "asb-evict-simpledrm.service",
            ])
        );
        let obsolete = input["obsolete_paths"].as_array().unwrap();
        for path in [
            "/usr/local/bin/appsandbox-agent",
            "/etc/systemd/system/appsandbox-agent.service",
            "/etc/modprobe.d/asb_drm.conf",
        ] {
            assert!(
                obsolete.contains(&json!(path)),
                "{path} is AppSandbox's and must go: {obsolete:?}"
            );
        }
    }

    /// The agent secret reaches the guest as a file of its own, so that no
    /// command line, log or manifest ever holds the secret itself.
    #[test]
    fn the_agent_secret_travels_as_a_file_and_never_as_a_value_in_the_manifest() {
        let root = temporary_root("secret");
        let bundle = build_in(&root.0, "first");

        assert_eq!(
            fs::read_to_string(bundle.root().join("agent.secret")).unwrap(),
            "c2VjcmV0"
        );
        let manifest = fs::read_to_string(bundle.manifest_path()).unwrap();
        assert!(!manifest.contains("c2VjcmV0"), "{manifest}");
        let input = fs::read_to_string(bundle.root().join("input.json")).unwrap();
        assert!(!input.contains("c2VjcmV0"), "{input}");
    }

    /// The uploaded program is the only thing that runs in the guest, and every
    /// value it acts on comes out of `input.json`.
    #[test]
    fn the_guest_program_is_fixed_and_reads_its_values_from_the_input_document() {
        let root = temporary_root("script");
        let bundle = build_in(&root.0, "first");

        let script = fs::read_to_string(bundle.root().join("vmlord-convert")).unwrap();
        assert!(script.starts_with("#!/usr/bin/env python3"), "{script}");
        assert!(script.contains("input.json"), "{script}");
        assert!(script.contains(GUEST_BUNDLE_DIRECTORY), "{script}");
        for value in [
            "ssh-ed25519 AAAAC3Nz vmlord",
            vmlord_seed::AGENT_SERVICE_PATH,
            vmlord_agent_protocol::auth::GUEST_SECRET_PATH,
        ] {
            assert!(
                !script.contains(value),
                "the program is the same bytes for every import, and {value} is not"
            );
        }
    }

    /// A second attempt writes into the same staging child, and a file left by
    /// an interrupted first attempt must not survive into the manifest.
    #[test]
    fn a_rebuild_replaces_whatever_an_interrupted_attempt_had_left() {
        let root = temporary_root("rebuild");
        let staging = root.0.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let agent = agent_binary(&root.0);
        let secret = SecretText::new("c2VjcmV0");
        let first = ConversionBundle::build(&request(&staging, &agent, &secret)).unwrap();
        fs::write(first.root().join("leftover"), b"half an upload").unwrap();

        let second = ConversionBundle::build(&request(&staging, &agent, &secret)).unwrap();

        assert_eq!(first.root(), second.root());
        assert!(!second.root().join("leftover").exists());
        assert!(
            !second
                .entries()
                .iter()
                .any(|entry| entry.name() == "leftover")
        );
    }
}
