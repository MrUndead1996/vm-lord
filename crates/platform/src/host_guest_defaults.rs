//! What a new VM's locale, keyboard layout and timezone start out as, read
//! from the host.
//!
//! The three values live in `GuestDefaults`, which the create form fills its
//! fields from before anyone edits them. Reading them is three Win32 calls and
//! three mappings into what a Linux guest names the same thing, so it belongs
//! here rather than in the layer drawing the fields -- the same reason
//! `host_dns` reads the host's resolvers from this crate.
//!
//! Nothing here fails. A setting that cannot be read, or that has no
//! counterpart in the guest, falls back to the matching field of
//! `GuestDefaults::default()` and says so in the log: an unknown keyboard
//! layout is not a reason to refuse to create a VM.

use vmlord_core::GuestDefaults;
use windows_timezones::WindowsTimezone;

/// The IANA name of the Windows time-zone key `windows_id`.
///
/// `windows_id` is `TimeZoneKeyName` and not `StandardName`: the second is
/// localized -- on a Russian Windows it reads «Русское стандартное время» --
/// and appears in no CLDR table. The first is the invariant registry key, the
/// same string on every locale.
fn iana_timezone(windows_id: &str) -> Option<String> {
    let zone = windows_id.parse::<WindowsTimezone>().ok()?;
    Some(zone.tzdb_id().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_time_zone_key_becomes_an_iana_name() {
        assert_eq!(
            iana_timezone("Russian Standard Time").as_deref(),
            Some("Europe/Moscow")
        );
        assert_eq!(
            iana_timezone("W. Europe Standard Time").as_deref(),
            Some("Europe/Berlin")
        );
    }

    /// The one key whose IANA name is also VMLord's own fallback, so a UTC host
    /// and an unreadable one produce the same guest -- by different routes.
    #[test]
    fn the_utc_key_is_a_key_like_any_other() {
        assert_eq!(iana_timezone("UTC").as_deref(), Some("Etc/UTC"));
    }

    /// Windows ships new zones faster than the CLDR table is regenerated, and
    /// an empty key is what a failed read leaves behind.
    #[test]
    fn a_key_the_table_does_not_know_maps_to_nothing() {
        assert_eq!(iana_timezone("No Such Standard Time"), None);
        assert_eq!(iana_timezone(""), None);
    }
}
