//! Stamps the agent with the revision it was built from.
//!
//! The crate version alone is `0.1.0` on every build ever made, which is why
//! a host log naming it could not tell a fresh agent from one a VM was created
//! with months ago -- and did not, for three rounds of a real-host
//! investigation. The revision is what distinguishes two builds of one
//! version.
//!
//! Nothing here is allowed to fail the build: an agent built from a source
//! archive rather than a checkout, or on a machine with no `git`, is a
//! perfectly good agent that simply cannot say where it came from.

use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    rerun_when_head_moves();

    let version = env::var("CARGO_PKG_VERSION").expect("Cargo must set the package version");
    println!("cargo::rustc-env=VMLORD_AGENT_BUILD={version}+{}", revision());
}

/// The short commit this was built from, plus whether the tree was edited.
///
/// `unknown` rather than an error: see the module note. A dirty tree is worth
/// saying out loud, because that is exactly the build whose behaviour no
/// commit accounts for.
fn revision() -> String {
    let Some(commit) = git(&["rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".to_owned();
    };

    match git(&["status", "--porcelain"]) {
        Some(status) if !status.is_empty() => format!("{commit}-dirty"),
        _ => commit,
    }
}

/// One `git` invocation's trimmed output, or nothing if it did not answer.
fn git(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(env::var("CARGO_MANIFEST_DIR").ok()?)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

/// Asks Cargo to run this again when the checked-out commit changes.
///
/// Without it the stamp is decided once and then carried by every later
/// rebuild, which would make it lie in precisely the way it exists to prevent.
fn rerun_when_head_moves() {
    let Some(git_dir) = git(&["rev-parse", "--git-dir"]).map(PathBuf::from) else {
        return;
    };
    let head = git_dir.join("HEAD");
    println!("cargo::rerun-if-changed={}", head.display());

    // A checked-out branch's HEAD names a ref file, and it is that file rather
    // than HEAD that moves when a commit is made on the branch.
    let Ok(contents) = fs::read_to_string(&head) else {
        return;
    };
    if let Some(reference) = contents.strip_prefix("ref:") {
        println!(
            "cargo::rerun-if-changed={}",
            git_dir.join(reference.trim()).display()
        );
    }
}
