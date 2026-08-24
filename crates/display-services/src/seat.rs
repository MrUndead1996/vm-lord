//! Which user is at the virtual screen.
//!
//! The clipboard socket cannot be owned by a group the way the socket between
//! the broker and the capture process is: the daemon on the other end runs as
//! whichever human logged in, and that name is not known when the VM is
//! provisioned. What *is* known at the moment of an accept is who logind says
//! is at `seat0`, so that is what the socket is authorised against -- the
//! clipboard belongs to the person sitting at the screen, and to nobody else.
//!
//! Read out of `/run/systemd/sessions` rather than asked of logind over D-Bus:
//! the broker is a small privileged process that talks to no bus, and these are
//! `KEY=value` files it can read without one. A guest whose files cannot be
//! read has no clipboard, which is the safe end of the failure.

/// The uid of the active graphical session on `seat0`, if there is one.
///
/// `None` while nobody is logged in -- at the GDM greeter, or on a guest that
/// has only console logins. A clipboard daemon that connects then is refused
/// and retries, which is what it does anyway while it waits for a session.
#[must_use]
pub fn active_graphical_uid() -> Option<libc::uid_t> {
    uid_in(SESSIONS)
}

/// Where logind keeps one file per session.
const SESSIONS: &str = "/run/systemd/sessions";

/// The directory half, so that the walk can be pointed at a fixture.
fn uid_in(directory: &str) -> Option<libc::uid_t> {
    let entries = std::fs::read_dir(directory).ok()?;

    for entry in entries.flatten() {
        // logind writes `<id>` for a session and `<id>.ref` for a reference
        // held on it; only the first is a session's state.
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(uid) = uid_of_active_graphical_session(&text) {
            return Some(uid);
        }
    }

    None
}

/// The uid of one session file, if that session is the graphical one on screen.
///
/// Three things have to hold at once, and each rules out a session that exists
/// but is not the one a clipboard belongs to: a console login has no selection
/// to share, an inactive session is not the one on screen, and a session with
/// no seat is not at the screen at all.
fn uid_of_active_graphical_session(text: &str) -> Option<libc::uid_t> {
    let mut uid = None;
    let (mut seat, mut graphical, mut active) = (false, false, false);

    for line in text.lines() {
        match line.split_once('=') {
            Some(("UID", value)) => uid = value.trim().parse().ok(),
            Some(("SEAT", value)) => seat = value.trim() == "seat0",
            Some(("TYPE", value)) => graphical = matches!(value.trim(), "wayland" | "x11"),
            Some(("ACTIVE", value)) => active = value.trim() == "1",
            _ => {}
        }
    }

    (seat && graphical && active).then_some(uid?)
}

#[cfg(test)]
mod tests {
    use super::{uid_in, uid_of_active_graphical_session};

    const WAYLAND: &str = "UID=1000\nSEAT=seat0\nTYPE=wayland\nACTIVE=1\nSTATE=active\n";
    const TTY: &str = "UID=1000\nSEAT=seat0\nTYPE=tty\nACTIVE=1\nSTATE=active\n";
    const INACTIVE: &str = "UID=1000\nSEAT=seat0\nTYPE=wayland\nACTIVE=0\nSTATE=online\n";
    const REMOTE: &str = "UID=1001\nTYPE=wayland\nACTIVE=1\nSTATE=active\n";

    #[test]
    fn an_active_graphical_session_on_the_seat_names_its_uid() {
        assert_eq!(uid_of_active_graphical_session(WAYLAND), Some(1000));
        assert_eq!(
            uid_of_active_graphical_session("UID=1000\nSEAT=seat0\nTYPE=x11\nACTIVE=1\n"),
            Some(1000)
        );
    }

    #[test]
    fn nothing_else_does() {
        assert_eq!(uid_of_active_graphical_session(TTY), None);
        assert_eq!(uid_of_active_graphical_session(INACTIVE), None);
        assert_eq!(uid_of_active_graphical_session(REMOTE), None);
        assert_eq!(uid_of_active_graphical_session(""), None);
        assert_eq!(
            uid_of_active_graphical_session("SEAT=seat0\nTYPE=wayland\nACTIVE=1\n"),
            None,
            "a session file with no uid names nobody"
        );
    }

    #[test]
    fn the_walk_finds_the_graphical_session_among_the_others() {
        let directory = std::env::temp_dir().join(format!("vmlord-seat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("c1"), TTY).unwrap();
        std::fs::write(directory.join("c2"), WAYLAND).unwrap();

        let uid = uid_in(directory.to_str().unwrap());

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(uid, Some(1000));
    }

    #[test]
    fn a_guest_without_logind_has_no_clipboard_rather_than_a_wrong_one() {
        assert_eq!(uid_in("/nonexistent/run/systemd/sessions"), None);
    }
}
