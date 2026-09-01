//! The one thing this process asks of systemd.
//!
//! The tray may ask for the guest display session to be put back, and the
//! parts of that session which are system units are restarted here, over the
//! system bus, in place of whatever is running. The broker itself is never
//! among them: the process answering the restart has to live through it.
//!
//! No subprocess: the same `zbus` the clipboard daemon uses for mutter, on
//! the system bus rather than the session one.

use zbus::blocking::{Connection, Proxy};

/// The capture process, which a restart puts back on the broker's socket.
pub const SESSION_UNIT: &str = "vmlord-display-session.service";

/// The sound daemon, a system service like the capture process.
pub const AUDIO_UNIT: &str = "vmlord-display-audio.service";

/// The units a session restart covers, in the order they are asked for.
///
/// The clipboard daemon is deliberately not here: its unit belongs to the
/// graphical session of whoever is logged in and lives on that session's bus,
/// which this process has no business on. The tray lives there too, and a
/// restart it can reach on its own bus is its own to do.
pub const GUEST_UNITS: &[&str] = &[SESSION_UNIT, AUDIO_UNIT];

/// Restarts the guest display services this process answers for.
///
/// # Errors
///
/// A string naming the first unit that refused, if any did.
pub fn restart_guest_services() -> Result<(), String> {
    restart_units(GUEST_UNITS, restart_unit)
}

/// The body of [`restart_guest_services`], over a seam the tests drive.
///
/// Every unit named is attempted even after one refused, because the units
/// do not depend on one another: a session that restarted beside an audio
/// daemon that would not is still worth having. The first refusal is what
/// comes back.
fn restart_units(
    units: &[&str],
    restart: impl Fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    let mut failure = None;
    for unit in units {
        if let Err(error) = restart(unit) {
            let _ = failure.get_or_insert_with(|| format!("{unit} did not restart: {error}"));
        }
    }

    failure.map_or(Ok(()), Err)
}

/// Asks systemd to restart one unit, in place of whatever is running.
///
/// `replace` rather than `fail` or `isolate`: the unit may well be running,
/// and replacing it is the whole request.
fn restart_unit(unit: &str) -> Result<(), String> {
    let connection = Connection::system().map_err(|error| format!("no system bus: {error}"))?;
    let manager: Proxy<'static> = Proxy::new(
        &connection,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .map_err(|error| error.to_string())?;
    manager
        .call::<_, _, ()>("RestartUnit", &(unit, "replace"))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::{AUDIO_UNIT, GUEST_UNITS, SESSION_UNIT, restart_units};

    /// The units that ship in the payload, as the guest will read them.
    const SESSION_UNIT_FILE: &str =
        include_str!("../../../payloads/display/services/vmlord-display-session.service");
    const AUDIO_UNIT_FILE: &str =
        include_str!("../../../payloads/display/services/vmlord-display-audio.service");
    const CLIPBOARD_UNIT_FILE: &str =
        include_str!("../../../payloads/display/services/vmlord-display-clipboard.service");
    const BROKER_UNIT_FILE: &str =
        include_str!("../../../payloads/display/services/vmlord-display-broker.service");

    #[test]
    fn a_restart_attempts_every_unit_and_names_the_first_refusal() {
        let asked = RefCell::new(Vec::new());
        let result = restart_units(&["first.service", "second.service"], |unit| {
            asked.borrow_mut().push(unit.to_owned());

            Err(format!("{unit} refused"))
        });

        assert_eq!(
            asked.into_inner(),
            vec!["first.service".to_owned(), "second.service".to_owned()],
            "the units do not depend on one another, so one refusal stops nothing"
        );
        assert_eq!(
            result,
            Err("first.service did not restart: first.service refused".to_owned())
        );
    }

    #[test]
    fn a_restart_that_nobody_refused_is_said_to_have_happened() {
        let asked = RefCell::new(Vec::new());
        let result = restart_units(&["only.service"], |unit| {
            asked.borrow_mut().push(unit.to_owned());

            Ok(())
        });

        assert_eq!(asked.into_inner(), vec!["only.service".to_owned()]);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn a_session_restart_is_the_capture_process_and_the_sound_daemon() {
        assert_eq!(GUEST_UNITS, &[SESSION_UNIT, AUDIO_UNIT]);
    }

    #[test]
    fn the_units_restarted_here_are_system_units() {
        // The restart is a call on the system bus, which reaches only the
        // system's own units. A unit moved to the user's session would make
        // this call fail in a running guest, however green the tests stay.
        for unit in [SESSION_UNIT_FILE, AUDIO_UNIT_FILE] {
            assert!(
                unit.contains("WantedBy=multi-user.target"),
                "a unit installed for the system, not for a user's session"
            );
        }
    }

    #[test]
    fn the_clipboard_is_a_user_unit_and_no_restart_here_names_it() {
        // A selection exists inside a compositor, so the clipboard daemon
        // lives on the logged-in user's session bus. This process never
        // touches that bus, and the tray which does is what restarts it.
        assert!(CLIPBOARD_UNIT_FILE.contains("WantedBy=graphical-session.target"));
        assert!(!GUEST_UNITS.contains(&"vmlord-display-clipboard.service"));
    }

    #[test]
    fn a_restart_never_names_the_broker_itself() {
        // The process that answers the restart has to live through it.
        assert!(!GUEST_UNITS.contains(&"vmlord-display-broker.service"));
        assert!(BROKER_UNIT_FILE.contains("WantedBy=multi-user.target"));
    }
}
