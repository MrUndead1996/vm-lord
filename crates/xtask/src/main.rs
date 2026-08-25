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

/// The release target for the application. MSVC is the toolchain Windows
/// itself is built against, and the one the HCS bindings expect.
const APP_TARGET: &str = "x86_64-pc-windows-msvc";

/// The release target for the guest agent. musl links statically through
/// `rust-lld`, which is why this build needs no C toolchain on the host.
const AGENT_TARGET: &str = "x86_64-unknown-linux-musl";

/// What `dist` collects, as (target directory, file name) pairs.
const ARTIFACTS: [(&str, &str); 4] = [
    (APP_TARGET, "vmlord.exe"),
    (APP_TARGET, "vmlord-com1.exe"),
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
    use std::path::{Path, PathBuf};

    use super::distribution_directory;

    #[test]
    fn the_distribution_lives_under_target_so_cargo_clean_removes_it() {
        let workspace = Path::new("workspace");

        assert_eq!(
            distribution_directory(workspace),
            PathBuf::from("workspace").join("target").join("dist")
        );
    }
}
