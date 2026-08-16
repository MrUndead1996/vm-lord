//! Running one external program, with a bound on how long it may take.
//!
//! The recipe runs `apt-get`, `dkms` and `modprobe`, and all three are
//! distribution-owned operations with no library form. What they have in
//! common is that none of them may run forever: a hung `apt-get` behind a
//! broken NAT would be an agent that never answers its host again.
//!
//! Output is captured on threads rather than read after the wait, because a
//! program that fills its pipe while nobody reads it blocks, and `dkms build`
//! produces far more than a pipe holds.

use std::{
    io::Read,
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// How many lines of a program's output are kept for a stage's message.
const KEPT_LINES: usize = 20;

/// How often a running program is asked whether it has finished.
const POLL: Duration = Duration::from_millis(50);

/// How a program ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// It exited on its own, with this code. A signal counts as a non-zero
    /// code that nothing here needs to tell apart.
    Exited(i32),
    /// It outran its budget and was killed.
    TimedOut,
    /// It could not be started: not installed, or not executable.
    NotStarted,
}

/// What running one program produced.
pub struct Outcome {
    pub ending: Ending,
    /// The last [`KEPT_LINES`] lines of its standard output and error.
    pub output: String,
}

impl Outcome {
    /// Whether the program ran and said it succeeded.
    pub fn succeeded(&self) -> bool {
        self.ending == Ending::Exited(0)
    }
}

/// Runs `program` with a wall-clock budget, and keeps the tail of its output.
///
/// Never fails: a program that cannot be started, one that fails and one that
/// hangs are three endings of the same call, because every caller here turns
/// all three into the same thing -- a stage that did not succeed.
pub fn run(
    program: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
    budget: Duration,
) -> Outcome {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so that a budget that runs out takes the
        // whole tree with it. `apt-get` and `dkms` both work through children,
        // and a killed parent whose child still holds the pipe open is a
        // timeout that waits for the program it just gave up on.
        .process_group(0);
    for (name, value) in environment {
        command.env(name, value);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Outcome {
                ending: Ending::NotStarted,
                output: format!("{program} could not be started: {error}"),
            };
        }
    };

    // Both pipes are drained on their own threads. A program that fills a pipe
    // nobody reads blocks in `write`, which would turn every long build into a
    // timeout.
    let standard_output = child.stdout.take().map(drain);
    let standard_error = child.stderr.take().map(drain);

    let started = Instant::now();
    let ending = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ending::Exited(status.code().unwrap_or(-1)),
            Ok(None) => {}
            // Asking a child whether it has finished does not fail short of a
            // kernel-level surprise, and there is nothing to report about one
            // but a non-zero ending.
            Err(_) => {
                kill_group(&mut child);
                break Ending::Exited(-1);
            }
        }
        if started.elapsed() >= budget {
            kill_group(&mut child);
            break Ending::TimedOut;
        }
        thread::sleep(POLL);
    };

    let mut output = String::new();
    for reader in [standard_output, standard_error].into_iter().flatten() {
        output.push_str(&reader.join().unwrap_or_default());
    }

    Outcome {
        ending,
        output: tail(&output, KEPT_LINES),
    }
}

/// Kills the program and everything it started, and reaps it.
///
/// The group rather than the process: a shell that forked rather than exec'd
/// leaves a child holding the pipe this call is about to read to its end, and
/// waiting for that child is exactly the wait the budget exists to avoid.
fn kill_group(child: &mut Child) {
    let group = child.id() as libc::pid_t;
    // SAFETY: `kill` takes no pointers. The negative pid names the process
    // group this child leads, which `process_group(0)` made it the leader of,
    // and the child has not been reaped yet, so the group cannot have been
    // reused for anything else.
    unsafe { libc::kill(-group, libc::SIGKILL) };
    let _ = child.wait();
}

/// Reads one pipe to its end on a thread of its own.
fn drain<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// The last `lines` lines of `output`, without a trailing newline.
///
/// A stage's message ends up in the host's log, and the useful part of a
/// failing build is at the end of it.
fn tail(output: &str, lines: usize) -> String {
    output
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Ending, run, tail};

    #[test]
    fn a_program_that_succeeds_reports_its_output() {
        let outcome = run(
            "/bin/sh",
            &["-c", "printf 'first\\nsecond\\n'"],
            &[],
            Duration::from_secs(10),
        );

        assert_eq!(outcome.ending, Ending::Exited(0));
        assert!(outcome.succeeded());
        assert!(outcome.output.contains("second"), "{}", outcome.output);
    }

    #[test]
    fn a_program_that_fails_keeps_its_code_and_its_standard_error() {
        let outcome = run(
            "/bin/sh",
            &["-c", "echo bad 1>&2; exit 3"],
            &[],
            Duration::from_secs(10),
        );

        assert_eq!(outcome.ending, Ending::Exited(3));
        assert!(!outcome.succeeded());
        assert!(outcome.output.contains("bad"), "{}", outcome.output);
    }

    #[test]
    fn the_environment_reaches_the_program() {
        let outcome = run(
            "/bin/sh",
            &["-c", "printf '%s' \"$DEBIAN_FRONTEND\""],
            &[("DEBIAN_FRONTEND", "noninteractive")],
            Duration::from_secs(10),
        );

        assert_eq!(outcome.output.trim(), "noninteractive");
    }

    #[test]
    fn a_program_that_outruns_its_budget_is_killed() {
        let started = Instant::now();
        let outcome = run(
            "/bin/sh",
            &["-c", "sleep 30"],
            &[],
            Duration::from_millis(200),
        );

        assert_eq!(outcome.ending, Ending::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the budget was not enforced"
        );
    }

    #[test]
    fn a_program_that_cannot_be_started_is_not_a_panic() {
        let outcome = run("/vmlord/no/such/program", &[], &[], Duration::from_secs(10));

        assert_eq!(outcome.ending, Ending::NotStarted);
        assert!(!outcome.output.is_empty());
    }

    #[test]
    fn only_the_last_lines_of_output_are_kept() {
        let long: String = (0..100).map(|line| format!("line {line}\n")).collect();

        let kept = tail(&long, 3);

        assert_eq!(kept, "line 97\nline 98\nline 99");
    }
}
