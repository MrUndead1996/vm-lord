//! `cargo run -p xtask -- workflow-check`: the workflows, read as data.
//!
//! Two things make a workflow dangerous, and neither shows up in a test run: a
//! token with more rights than the job needs, and an action reference that can
//! change under it. Both are properties of the YAML, so they are checked here
//! rather than discovered after a tag is pushed.
//!
//! Every workflow is checked for those two. The release workflow is checked
//! for more, because it is the one that publishes.

use std::{fs, path::Path};

use serde_yaml_ng::Value;

const WORKFLOW_DIRECTORY: &str = ".github/workflows";
const RELEASE_WORKFLOW_FILE: &str = "release.yml";

/// The job in the release workflow that is allowed to write to the repository.
const RELEASE_JOB: &str = "release";

pub fn run(workspace: &Path) -> Result<(), String> {
    let directory = workspace.join(WORKFLOW_DIRECTORY);
    let mut files = Vec::new();
    let entries = fs::read_dir(&directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
            .path();
        if matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("yml" | "yaml")
        ) {
            files.push(path);
        }
    }
    if files.is_empty() {
        return Err(format!("no workflows found in {}", directory.display()));
    }
    // Read in a fixed order so two runs report the same problems in the same
    // sequence, whatever order the filesystem hands them over in.
    files.sort();

    let mut problems = Vec::new();
    for path in &files {
        problems.extend(check(workspace, path)?);
    }

    if problems.is_empty() {
        println!("workflow-check: {} workflow(s) are sound", files.len());
        return Ok(());
    }
    for problem in &problems {
        eprintln!("workflow-check: {problem}");
    }
    Err(format!("{} workflow problem(s)", problems.len()))
}

fn check(workspace: &Path, path: &Path) -> Result<Vec<String>, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let document: Value = serde_yaml_ng::from_str(&text)
        .map_err(|error| format!("{} is not valid YAML: {error}", path.display()))?;

    // Named by its path relative to the workspace, so a problem reads the same
    // way whether it was found from the repository root or from anywhere else.
    let relative = path.strip_prefix(workspace).unwrap_or(path).display();
    let mut problems = every_workflow(&document);
    if path.ends_with(RELEASE_WORKFLOW_FILE) {
        problems.extend(check_release(&document));
    }

    Ok(problems
        .into_iter()
        .map(|problem| format!("{relative}: {problem}"))
        .collect())
}

/// What is true of every workflow in this repository, whatever it does.
fn every_workflow(document: &Value) -> Vec<String> {
    let mut problems = unpinned_actions(document);
    // Read by default, so a step added later starts with no rights over this
    // repository and has to be given them deliberately.
    if permission(document.get("permissions"), "contents").as_deref() != Some("read") {
        problems.push("the workflow's default permissions are not `contents: read`".to_owned());
    }
    if let Some(permissions) = document.get("permissions").and_then(Value::as_mapping) {
        for (name, access) in permissions {
            if access.as_str() == Some("write") {
                problems.push(format!(
                    "the workflow grants default `{}: write`; write access must be scoped to one job",
                    name.as_str().unwrap_or("unknown")
                ));
            }
        }
    }
    problems
}

/// What the release workflow has to be true of.
fn check_release(document: &Value) -> Vec<String> {
    let mut problems = Vec::new();

    // A release is cut from a tag and from nothing else: a workflow that also
    // ran on a push to a branch would build a "release" of whatever was on it.
    let tags = triggers(document, "push", "tags");
    if tags != vec!["v*".to_owned()] {
        problems.push(format!(
            "the release trigger is {tags:?}, not the version tags `v*`"
        ));
    }
    if document.get("pull_request").is_some() || trigger(document, "pull_request").is_some() {
        problems.push("a pull request must not be able to start a release".to_owned());
    }

    problems.extend(installer_version_passed_in(document));
    problems.extend(distribution_carries_payloads(document));

    for (name, job) in jobs(document) {
        let contents = permission(job.get("permissions"), "contents");
        match (name.as_str(), contents.as_deref()) {
            (RELEASE_JOB, Some("write")) => {}
            (RELEASE_JOB, _) => problems.push(format!(
                "job `{RELEASE_JOB}` needs `contents: write` to create the release"
            )),
            (_, Some("write")) => problems.push(format!(
                "job `{name}` has `contents: write`; only `{RELEASE_JOB}` may write"
            )),
            _ => {}
        }
    }

    problems
}

/// That the step which compiles the installer hands it the workspace version.
///
/// The Inno Setup script states no version of its own; it takes one through
/// `/DAppVersion=`. A step that drops the flag does not build the wrong
/// installer -- the script refuses to compile at all -- but it refuses after
/// the tests, the distribution and the licence notices have already been
/// built. Read here, that is a failed pull request instead.
fn installer_version_passed_in(document: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, job) in jobs(document) {
        for script in run_scripts(job) {
            if !script.to_ascii_lowercase().contains("iscc") {
                continue;
            }
            if !script.contains("/DAppVersion=") {
                problems.push(format!(
                    "job `{name}` compiles the installer without `/DAppVersion=`; the version \
                     has to come from the workspace"
                ));
            }
        }
    }
    problems
}

/// That the step which builds the distribution asks for both payloads.
///
/// `cargo dist` with neither flag is not an error: it prints one line saying
/// no payload was included and builds a release that starts VMs with no GPU
/// support and no guest display. That is what 0.2.0 shipped. The flags are the
/// difference, so their absence is read here rather than discovered by
/// somebody installing the result.
fn distribution_carries_payloads(document: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, job) in jobs(document) {
        for script in run_scripts(job) {
            if !script.contains("cargo dist") {
                continue;
            }
            for flag in ["--gpu-payload", "--display-payload"] {
                if !script.contains(flag) {
                    problems.push(format!(
                        "job `{name}` builds the distribution without `{flag}`; a release \
                         without it has no such payload at all"
                    ));
                }
            }
        }
    }
    problems
}

/// The `run:` scripts of one job, with their comments removed.
///
/// Comments go first because both checks over these scripts look for a flag
/// that is also named in the comment beside the command: read whole, a script
/// would go on passing after the command itself had lost the flag.
fn run_scripts(job: &Value) -> Vec<String> {
    let Some(steps) = job.get("steps").and_then(Value::as_sequence) else {
        return Vec::new();
    };
    steps
        .iter()
        .filter_map(|step| step.get("run").and_then(Value::as_str))
        .map(|run| {
            run.lines()
                .filter(|line| !line.trim_start().starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect()
}

/// Every action reference that is not pinned to a full commit SHA.
///
/// A tag or a branch in `uses:` is a name someone else can move, which makes
/// it someone else's decision what runs with this repository's token.
fn unpinned_actions(document: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, job) in jobs(document) {
        let Some(steps) = job.get("steps").and_then(Value::as_sequence) else {
            continue;
        };
        for step in steps {
            let Some(uses) = step.get("uses").and_then(Value::as_str) else {
                continue;
            };
            let reference = uses.rsplit('@').next().unwrap_or_default();
            let pinned = reference.len() == 40
                && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
                && reference.bytes().all(|byte| !byte.is_ascii_uppercase());
            if !pinned {
                problems.push(format!(
                    "job `{name}` uses `{uses}`, which is not pinned to a commit SHA"
                ));
            }
        }
    }
    problems
}

/// The jobs of a workflow, in the order they are written.
fn jobs(document: &Value) -> Vec<(String, &Value)> {
    document
        .get("jobs")
        .and_then(Value::as_mapping)
        .map(|jobs| {
            jobs.iter()
                .filter_map(|(name, job)| Some((name.as_str()?.to_owned(), job)))
                .collect()
        })
        .unwrap_or_default()
}

/// One trigger of the `on:` block.
///
/// Reached by name rather than by key, because `on` is YAML 1.1's boolean
/// `true`: a parser that does not quote it turns the block's own key into
/// `true`, and looking for the string alone would silently find nothing.
fn trigger<'a>(document: &'a Value, name: &str) -> Option<&'a Value> {
    let events = document
        .get("on")
        .or_else(|| document.get(Value::Bool(true)))?;
    events.get(name)
}

/// The patterns one trigger filters on, such as `on.push.tags`.
fn triggers(document: &Value, event: &str, filter: &str) -> Vec<String> {
    trigger(document, event)
        .and_then(|event| event.get(filter))
        .and_then(Value::as_sequence)
        .map(|patterns| {
            patterns
                .iter()
                .filter_map(|pattern| Some(pattern.as_str()?.to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// One entry of a `permissions:` block, or `None` when it is absent.
fn permission(permissions: Option<&Value>, name: &str) -> Option<String> {
    permissions?.get(name)?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use serde_yaml_ng::Value;

    use super::{
        check_release, distribution_carries_payloads, every_workflow, installer_version_passed_in,
        unpinned_actions,
    };

    fn parse(text: &str) -> Value {
        serde_yaml_ng::from_str(text).expect("the fixture is valid YAML")
    }

    const PINNED: &str = "actions/checkout@08c6903cd8c0fde910a37f88322edcfb5dd907a8";

    fn release(permissions: &str, job_permissions: &str, tags: &str) -> Value {
        parse(&format!(
            "on:\n  push:\n    tags:\n{tags}\npermissions:\n{permissions}\njobs:\n  \
             release:\n    permissions:\n{job_permissions}\n    steps:\n      - uses: {PINNED}\n"
        ))
    }

    /// A release comes from a version tag; anything else is not a release.
    #[test]
    fn the_release_runs_only_on_version_tags() {
        let sound = release(
            "  contents: read\n",
            "      contents: write\n",
            "      - 'v*'\n",
        );
        assert_eq!(check_release(&sound), Vec::<String>::new());

        let branch = release(
            "  contents: read\n",
            "      contents: write\n",
            "      - main\n",
        );
        assert!(!check_release(&branch).is_empty());
    }

    /// Read by default: a step added later must ask for more rather than
    /// inherit them. Asserted of every workflow, not just the release, because
    /// a token is a token whatever the job around it is for.
    #[test]
    fn the_default_permissions_are_read_only() {
        let writable = release(
            "  contents: write\n",
            "      contents: write\n",
            "      - 'v*'\n",
        );

        let problems = every_workflow(&writable);

        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("default permissions")),
            "{problems:?}"
        );
    }

    /// A named read permission does not make the defaults read-only when a
    /// second capability is granted write access beside it.
    #[test]
    fn the_default_permissions_reject_other_write_capabilities() {
        let document = parse(
            "permissions:\n  contents: read\n  security-events: write\njobs:\n  check:\n    steps: []\n",
        );

        let problems = every_workflow(&document);

        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("security-events")),
            "{problems:?}"
        );
    }

    /// The installer script states no version, so the step that compiles it
    /// has to hand one over. Without the flag the release fails only after
    /// everything before it has been built.
    #[test]
    fn the_installer_is_compiled_with_the_workspace_version() {
        let compile = |run: &str| {
            parse(&format!(
                "jobs:\n  release:\n    steps:\n      - run: |\n          {run}\n"
            ))
        };

        assert_eq!(
            installer_version_passed_in(&compile(
                "& $iscc \"/DAppVersion=$env:VMLORD_VERSION\" installer/vmlord.iss"
            )),
            Vec::<String>::new()
        );

        let problems = installer_version_passed_in(&compile("& $iscc installer/vmlord.iss"));
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("/DAppVersion=")),
            "{problems:?}"
        );

        // The flag is explained in a comment beside the command; finding it
        // there is not finding it in the command.
        let problems = installer_version_passed_in(&parse(
            "jobs:\n  release:\n    steps:\n      - run: |\n          # pass /DAppVersion= \
             here\n          & $iscc installer/vmlord.iss\n",
        ));
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("/DAppVersion=")),
            "{problems:?}"
        );

        // A step that does not compile the installer is not asked about it.
        assert_eq!(
            installer_version_passed_in(&compile("cargo dist")),
            Vec::<String>::new()
        );
    }

    /// A release without the payload flags is not a failed release: it is a
    /// quieter one, with no GPU support and no guest display.
    #[test]
    fn the_distribution_is_built_with_both_payloads() {
        let build = |run: &str| {
            parse(&format!(
                "jobs:\n  release:\n    steps:\n      - run: |\n          {run}\n"
            ))
        };

        assert_eq!(
            distribution_carries_payloads(&build(
                "cargo dist --gpu-payload gpu --display-payload display"
            )),
            Vec::<String>::new()
        );

        let problems = distribution_carries_payloads(&build("cargo dist --gpu-payload gpu"));
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("--display-payload")),
            "{problems:?}"
        );

        let problems = distribution_carries_payloads(&build("cargo dist"));
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    /// Only the job that creates the release may write to the repository.
    #[test]
    fn no_job_but_the_release_may_write() {
        let document = parse(&format!(
            "on:\n  push:\n    tags:\n      - 'v*'\npermissions:\n  contents: read\njobs:\n  \
             release:\n    permissions:\n      contents: write\n    steps:\n      - uses: \
             {PINNED}\n  notify:\n    permissions:\n      contents: write\n    steps:\n      - \
             uses: {PINNED}\n"
        ));

        let problems = check_release(&document);

        assert!(
            problems.iter().any(|problem| problem.contains("`notify`")),
            "{problems:?}"
        );
    }

    /// A tag in `uses:` is a name its owner can move onto other code.
    #[test]
    fn every_action_is_pinned_to_a_commit() {
        let moving = parse("jobs:\n  release:\n    steps:\n      - uses: actions/checkout@v4\n");

        let problems = unpinned_actions(&moving);

        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("checkout@v4"), "{problems:?}");
    }
}
