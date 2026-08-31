//! Build automation behind the Cargo aliases in `.cargo/config.toml`.
//!
//! Only `dist` lives here: the other aliases are single `cargo` invocations
//! and need no program to run them.

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
mod display_bench;
mod display_payload;
mod dist_arguments;
mod gpu_payload;
mod release;
mod workflow;

/// The release target for the application. MSVC is the toolchain Windows
/// itself is built against, and the one the HCS bindings expect.
const APP_TARGET: &str = "x86_64-pc-windows-msvc";

/// The release target for the guest agent. musl links statically through
/// `rust-lld`, which is why this build needs no C toolchain on the host.
const AGENT_TARGET: &str = "x86_64-unknown-linux-musl";

/// What `dist` collects, as (target directory, file name) pairs.
const ARTIFACTS: [(&str, &str); 5] = [
    (APP_TARGET, "vmlord.exe"),
    (APP_TARGET, "vmlord-com1.exe"),
    // The host of an interactive SSH session, which is what makes the end of
    // one reportable. It ships beside `vmlord.exe` because that is where the
    // launcher looks for it.
    (APP_TARGET, "vmlord-ssh.exe"),
    // The display window, opened by VMLord for one VM at a time. It ships
    // beside `vmlord.exe` because that is where the launcher looks for it.
    (APP_TARGET, "vmlord-display.exe"),
    (AGENT_TARGET, "vmlord-agent"),
];

fn main() -> ExitCode {
    let task = env::args().nth(1);
    let result = match task.as_deref() {
        Some("dist") => dist_arguments::parse(env::args().skip(2)).and_then(dist),
        Some("gpu-payload") => gpu_payload::run(env::args().skip(2)),
        Some("display-payload") => display_payload::run(env::args().skip(2)),
        Some("display-bench") => display_bench::run(env::args().skip(2)),
        Some("release-manifest") => release::run(env::args().skip(2)),
        Some("workflow-check") => workspace_root().and_then(|workspace| workflow::run(&workspace)),
        Some(other) => Err(format!("unknown task `{other}`")),
        None => Err("missing task".to_owned()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Builds the release artifacts and gathers them under Cargo's target directory.
fn dist(payloads: Vec<dist_arguments::DistPayload>) -> Result<(), String> {
    if !cfg!(windows) {
        return Err(format!(
            "`cargo dist` runs on Windows only: the release application is built for {APP_TARGET}, \
             which needs the MSVC toolchain. From WSL use `cargo check-windows` and \
             `cargo test-windows` instead."
        ));
    }

    let workspace = workspace_root()?;

    cargo(
        &workspace,
        &["build", "-p", "vmlord", "--release", "--target", APP_TARGET],
    )?;
    cargo(
        &workspace,
        &[
            "build",
            "-p",
            "vmlord-display-viewer",
            "--release",
            "--target",
            APP_TARGET,
        ],
    )?;
    cargo(
        &workspace,
        &[
            "build",
            "-p",
            "vmlord-agent",
            "--release",
            "--target",
            AGENT_TARGET,
        ],
    )?;

    let destination = distribution_directory(&workspace);
    if destination.exists() {
        fs::remove_dir_all(&destination)
            .map_err(|error| format!("cannot clear {}: {error}", destination.display()))?;
    }
    fs::create_dir_all(&destination)
        .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;

    for (target, file) in ARTIFACTS {
        let source = workspace
            .join("target")
            .join(target)
            .join("release")
            .join(file);
        fs::copy(&source, destination.join(file)).map_err(|error| {
            format!(
                "cannot copy {} into the distribution: {error}",
                source.display()
            )
        })?;
        println!("dist: {file}");
    }
    stage_distros(&workspace, &destination)?;
    stage_licence(&workspace, &destination)?;
    stage_third_party_notices(&workspace, &destination)?;

    if payloads.is_empty() {
        println!(
            "dist: no payload included; pass --gpu-payload <directory> or \
             --display-payload <directory>"
        );
    }
    for payload in &payloads {
        let (kind, payload_id) = match payload {
            dist_arguments::DistPayload::Gpu(source) => (
                vmlord_gpu_payload::LOCAL_ARCHIVE_DIRECTORY,
                gpu_payload::stage_release_payload(source, &destination)?,
            ),
            dist_arguments::DistPayload::Display(source) => (
                vmlord_display_payload::LOCAL_ARCHIVE_DIRECTORY,
                display_payload::stage_release_payload(source, &destination)?,
            ),
        };
        println!("dist: {kind}/{payload_id}.zip and {kind}/{payload_id}.json");
    }

    println!("dist: written to {}", destination.display());
    Ok(())
}

/// The name the notices are staged and installed under.
///
/// Plain text and not HTML: it is opened from a Program Files directory by
/// whoever wants to read it, and Notepad is the one viewer every Windows has.
const THIRD_PARTY_NOTICES: &str = "THIRD-PARTY-LICENSES.txt";

/// Copies VMLord's own licence beside the binaries.
///
/// The GPL requires the text to travel with the program, and the installed
/// tree is the only copy a user who never saw the repository has.
fn stage_licence(workspace: &Path, destination: &Path) -> Result<(), String> {
    let source = workspace.join("LICENSE");
    fs::copy(&source, destination.join("LICENSE"))
        .map_err(|error| format!("cannot copy {}: {error}", source.display()))?;
    println!("dist: LICENSE");
    Ok(())
}

/// Generates the third-party licence notices from the resolved dependency
/// graph, through `cargo-about` and the repository's own template.
///
/// Generated rather than kept in the repository: a hand-written list is one
/// dependency upgrade away from being wrong, and being wrong here means
/// shipping someone's code without their licence. `about.toml` names the
/// licences the audit accepted, so an unfamiliar one fails this build rather
/// than reaching the notices unread.
fn stage_third_party_notices(workspace: &Path, destination: &Path) -> Result<(), String> {
    let output = destination.join(THIRD_PARTY_NOTICES);
    let output = output
        .to_str()
        .ok_or("the distribution path is not UTF-8")?;
    cargo(
        workspace,
        &[
            "about",
            "generate",
            "--config",
            "about.toml",
            "--fail",
            "--output-file",
            output,
            "installer/third-party-licenses.hbs",
        ],
    )
    .map_err(|error| {
        // Two quite different failures reach here, and naming only the first
        // sends a reader to reinstall a tool that ran perfectly well: the tool
        // may be missing, or it may have run and refused a licence.
        format!(
            "{error}\nEither the pinned generator is not installed -- \
             cargo install --locked --features cli cargo-about@0.9.2 -- or a \
             dependency's licence is not in about.toml's accepted set, in \
             which case the errors above name it and the audit in \
             docs/dependency-licenses.md has to be repeated before it ships."
        )
    })?;
    println!("dist: {THIRD_PARTY_NOTICES}");
    Ok(())
}

fn stage_distros(workspace: &Path, destination: &Path) -> Result<(), String> {
    let source = workspace.join("distros");
    let target = destination.join("distros");
    fs::create_dir_all(&target)
        .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
    let entries = fs::read_dir(&source)
        .map_err(|error| format!("cannot read {}: {error}", source.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", source.display()))?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let file_name = path
            .file_name()
            .ok_or_else(|| format!("distribution profile has no file name: {}", path.display()))?;
        fs::copy(&path, target.join(file_name))
            .map_err(|error| format!("cannot copy {}: {error}", path.display()))?;
        println!("dist: distros/{}", file_name.to_string_lossy());
    }
    Ok(())
}

/// Returns the directory holding the assembled release distribution.
///
/// Keeping it below `target` makes `cargo clean` remove generated release
/// artifacts as well as Cargo's normal build outputs.
fn distribution_directory(workspace: &Path) -> PathBuf {
    workspace.join("target").join("dist")
}

/// Runs Cargo in the workspace, failing on anything but a clean exit.
fn cargo(workspace: &Path, arguments: &[&str]) -> Result<(), String> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let status = Command::new(&cargo)
        .args(arguments)
        .current_dir(workspace)
        .status()
        .map_err(|error| format!("cannot run cargo {}: {error}", arguments.join(" ")))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cargo {} failed with {status}",
            arguments.join(" ")
        ))
    }
}

/// The workspace root, reached from this crate's own manifest.
fn workspace_root() -> Result<PathBuf, String> {
    let manifest = env::var_os("CARGO_MANIFEST_DIR")
        .ok_or_else(|| "CARGO_MANIFEST_DIR is unset; run this through `cargo dist`".to_owned())?;
    PathBuf::from(manifest)
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| "cannot locate the workspace root above crates/xtask".to_owned())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{distribution_directory, stage_distros};

    #[test]
    fn the_distribution_lives_under_target_so_cargo_clean_removes_it() {
        let workspace = Path::new("workspace");

        assert_eq!(
            distribution_directory(workspace),
            PathBuf::from("workspace").join("target").join("dist")
        );
    }

    #[test]
    fn every_distribution_profile_is_staged_with_the_release() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("vmlord-dist-profile-test-{unique}"));
        let source = root.join("workspace").join("distros");
        let destination = root.join("target").join("dist");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("ubuntu.json"), "ubuntu").unwrap();
        fs::write(source.join("fedora.json"), "fedora").unwrap();

        stage_distros(&root.join("workspace"), &destination).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("distros/ubuntu.json")).unwrap(),
            "ubuntu"
        );
        assert_eq!(
            fs::read_to_string(destination.join("distros/fedora.json")).unwrap(),
            "fedora"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
