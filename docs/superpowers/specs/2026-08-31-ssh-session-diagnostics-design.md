# SSH Session Diagnostics Design

## Goal

Tell a person how their interactive SSH session ended, and say which kind of
failure it was: authentication, a changed host key, transport, or something
else OpenSSH exited with.

This design implements Vikunja task 76, the follow-up to task 12 that task 12
deliberately left open: the mechanism was to be chosen after living with the
direct launcher from task 75.

Today a session is opened and forgotten. `SshLauncher::launch` establishes
everything knowable in advance -- the client exists, the VM has SSH access, it
has an address, something answers on its port, the key is still there -- hands
an argument vector to a terminal host, and returns. What OpenSSH goes on to say
lands in a window VMLord cannot read, and the window closes when the session
ends, so in practice nobody reads it either.

## Decisions

- The terminal hosts a new console program, `vmlord-ssh.exe`, which runs
  `ssh.exe` as its child and reports how it ended. This is the same shape as
  `vmlord-com1.exe`: a process that exists so a terminal window has something to
  host, with all of its logic in `vmlord-platform`.
- The helper is used on both paths -- the Windows Terminal window and the
  `CREATE_NEW_CONSOLE` fallback -- so there is one code path and one kind of
  report, not one that works only when Windows Terminal is missing.
- `ssh.exe` keeps the helper's standard input, output and error, so the session
  is interactive exactly as it is now, and password and host-key prompts still
  appear in the window.
- `ssh.exe` is given `-E <session log>`. OpenSSH answers almost every failure of
  its own with exit code 255, so the code alone cannot tell authentication from
  a changed host key; the text can. The cost is that OpenSSH's own messages
  leave the window for the file -- which loses nothing a person could read
  today, because the window closes with the session.
- The outcome crosses the process boundary as a small JSON report file, and its
  arrival is announced by named events, the way a COM1 reader announces that it
  has finished.
- Classification is a pure function in `vmlord-core`, beside `SshEndpoint`, so
  it is tested without Windows, a network, or an `ssh.exe` to run.
- Nothing here changes what a session *is*: VMLord keeps no child handle for
  `ssh.exe`, kills nothing on shutdown, and still allows two shells into one
  guest.

## The launch, end to end

1. `SshLauncher::launch` performs the preflight it performs today and builds the
   `SshInvocation` for `ssh.exe`, now with `-E <vm>\ssh-sessions\<id>.log`
   appended. `<id>` is a v4 UUID minted per launch: two shells into one guest
   are ordinary, so nothing may be named after the VM alone.
2. The launcher creates two named Windows events for the session:
   - `finished`, which the helper signals however it leaves -- return, error or
     panic -- through a `SignalOnDrop` guard;
   - `alive`, which the helper creates and holds. A named object exists while a
     handle to it does, so a name that is gone is a helper that is gone. This is
     the only way to notice a window someone closed, which kills the helper with
     no chance to signal anything.
3. The launcher wraps the invocation in the helper's command line and hands that
   to the terminal hosts, best first, as it does today:

   ```
   wt.exe -w new new-tab --title "VMLord SSH — <vm>"
       -- vmlord-ssh.exe --report <path> --log <path> --vm-name <name>
          --finished-event <name> --alive-event <name>
          -- <ssh.exe> <ssh args...>
   ```

   and, if `wt.exe` cannot be started at all, `vmlord-ssh.exe` with the same
   arguments and `CREATE_NEW_CONSOLE`. Neither path goes through a shell, so no
   user name, path or address becomes a substring something else parses.
4. The launcher records the session -- id, VM name, report path, the two events
   -- in the shared `SshSessions` registry, and returns. The record is added
   before the terminal is started, and removed again if no terminal could be
   started, so a failed launch leaves nothing behind.
5. The helper runs `ssh.exe`, waits for it, and writes the report: the outcome,
   the exit code, and the tail of the session log. Then it deletes the session
   log -- it owns that file end to end -- and exits, signalling `finished`.
6. VMLord's refresh tick reaps the registry. A session whose `finished` is
   signalled, or whose `alive` name has vanished, is over. If a report is there,
   it is read, turned into a diagnostic, and deleted; if there is none, the
   session is reported as a window that was closed.

## The report

`<vm directory>\ssh-sessions\<id>.json`, written once, read once, then deleted:

```json
{
  "outcome": "authentication_failed",
  "exit_code": 255,
  "detail": "machi@172.22.42.7: Permission denied (publickey)."
}
```

`detail` is the tail of the `-E` log, capped the way `guest_ready` caps its
transcript tail, so one long-running verbose session cannot put a page of text
in the diagnostics panel.

Lifetimes are deliberate and dull: the helper deletes the log, VMLord deletes
the report. The one file that can be orphaned is a report written after the
VMLord that was waiting for it has exited. A launch sweeps the VM's
`ssh-sessions` directory before it starts, removing every file that does not
belong to a session in the registry -- the sessions this VMLord is still
waiting for are exactly the ones it keeps -- so an orphan survives at most
until the next session into that VM. The directory also goes with the VM when
it is deleted, including a delete with `delete_disks = false`, beside `keys/`
and `known_hosts`.

## Classification

`vmlord_core::ssh::classify_session(exit_code: Option<i32>, log_tail: &str) ->
SshSessionOutcome`, in the shape of `guest_ready::outcome`:

| Condition | Outcome | Level |
|---|---|---|
| `Some(0)` | `Ended { code: 0 }` | Info |
| `Some(code)`, `code != 255` | `Ended { code }` -- the remote shell's status; the session happened | Info |
| 255, tail has `REMOTE HOST IDENTIFICATION HAS CHANGED`, `Host key verification failed`, or `differs from the key for the IP address` | `HostKeyMismatch` | Error |
| 255, tail has `Permission denied`, `Too many authentication failures`, or `No supported authentication methods` | `AuthenticationFailed` | Error |
| 255, tail has `connect to host`, `Connection refused`, `Connection timed out`, `Connection reset`, `Could not resolve`, `kex_exchange_identification`, `Network is unreachable`, or `No route to host` | `TransportFailure` | Warning |
| 255, anything else | `Unrecognized { code: 255 }`, with the tail as it stands | Warning |
| `None` | `Terminated` -- `ssh.exe` died without a code | Warning |

The order matters and is the order above: a changed host key is decided before
a refused credential, because the two can appear in one log and the host key is
the one that has to be acted on.

Two outcomes never come from `ssh.exe` itself:

- `NotStarted { detail }` -- the helper could not run `ssh.exe` at all. Error.
- `WindowClosed` -- the helper is gone and left no report. Info: closing a
  window is how people end shells.

What each outcome says to the user is decided in `vmlord-platform`, next to the
`diagnostic!` calls: a host-key mismatch names the VM's own `known_hosts` file
and says it is not reset automatically; an authentication failure names the
configured mode (the VMLord key, or a password); a transport failure names the
endpoint. Diagnostics are not localized -- they do not go through `t!` -- so no
catalogue changes.

## Code

New:

- `crates/core/src/ssh.rs` (extended): `SshSessionOutcome`, `classify_session`,
  and the serde types of the report.
- `crates/platform/src/ssh_session.rs`: the helper -- argument parsing
  (`parse_ssh_helper_args`) and its run (`run_ssh_helper`), split so that the
  part that waits, classifies and writes the report is testable without
  spawning `ssh.exe`.
- `crates/platform/src/ssh_sessions.rs`: the host side -- the registry, `reap()`,
  and reading a report.
- `crates/vmlord/src/bin/vmlord-ssh.rs`: `main`, and nothing else.

Changed:

- `crates/platform/src/ssh_terminal.rs`: the helper wrapping, the events, the
  `-E` option, the registry entry.
- `crates/platform/src/ssh_launches.rs`: the worker adds the session to the
  registry it was handed.
- `crates/platform/src/repository.rs`: the registry field, the reap in
  `refresh()`, and the diagnostics.
- `crates/platform/src/layout.rs`: `ssh_sessions_directory`,
  `ssh_session_log_path`, `ssh_session_report_path`.
- `crates/platform/src/delete.rs` and the cleanup path: the `ssh-sessions`
  directory goes with the VM.
- `crates/vmlord/Cargo.toml`, `crates/xtask/src/main.rs`,
  `installer/check.ps1`: one more shipped binary.
- `ARCHITECTURE.md`.

## Testing

- `classify_session` for every row of the table, in both orders where a log
  could match two rules, and for an empty tail.
- The helper's argument parsing: exact flag/value pairs, a missing value, a
  repeated flag, and the `--` that separates the helper's arguments from the
  client's.
- The helper's reporting half: an exit code and a log file become the report
  file that the host side then reads back into the same outcome.
- `ssh_terminal`: the command handed to each terminal host runs the helper, the
  client follows `--`, and `-E` names the session log. The existing tests move
  to these assertions rather than being replaced.
- `ssh_sessions`: a signalled `finished` with a report yields the classified
  diagnostic; a vanished `alive` with no report yields `WindowClosed`; a session
  still running is not reaped. Mirrors the COM1 tests' `finish_for_test`.
- Manual, alongside the ignored Hyper-V scenario of task 78: a wrong key, a
  replaced host key, and a stopped `sshd` each produce their own diagnostic.
