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

/// The script subtags glibc keeps in a locale name, and the modifier it
/// spells them with.
///
/// Everywhere else the region already says which script is meant, so the
/// subtag is dropped. These two are the pairs where the script *is* the
/// distinction, and the name below is the one in glibc's `SUPPORTED` -- which
/// is the list `locale-gen` reads, modifier and no codeset. `locale -a` prints
/// the same locales differently; matching it would generate nothing.
const SCRIPT_MODIFIERS: [(&str, &str, &str); 2] =
    [("sr", "Latn", "latin"), ("uz", "Cyrl", "cyrillic")];

/// The POSIX locale name matching the BCP-47 tag `bcp47`.
///
/// Windows says `ru-RU`, `pt-BR`, `zh-Hans-CN` or `sr-Latn-RS`; a guest wants
/// `ru_RU.UTF-8`. The parse is deliberately strict -- a tag that does not
/// carry both a language and a two-letter region has no POSIX counterpart, and
/// answering `None` puts the default in the form where the user can see and
/// correct it.
fn posix_locale(bcp47: &str) -> Option<String> {
    let mut subtags = bcp47.split('-');

    let language = subtags.next()?;
    if !matches!(language.len(), 2..=3) || !language.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return None;
    }

    let mut next = subtags.next()?;
    let mut script = None;
    if next.len() == 4 && next.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        script = Some(next);
        next = subtags.next()?;
    }

    let region = next;
    if region.len() != 2 || !region.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    let region = region.to_ascii_uppercase();

    let modifier = script.and_then(|script| {
        SCRIPT_MODIFIERS
            .iter()
            .find(|(kept_language, kept_script, _)| {
                *kept_language == language && *kept_script == script
            })
            .map(|(_, _, modifier)| *modifier)
    });

    Some(match modifier {
        Some(modifier) => format!("{language}_{region}@{modifier}"),
        None => format!("{language}_{region}.UTF-8"),
    })
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

    #[test]
    fn a_language_and_a_region_become_a_posix_name() {
        assert_eq!(posix_locale("ru-RU").as_deref(), Some("ru_RU.UTF-8"));
        assert_eq!(posix_locale("en-US").as_deref(), Some("en_US.UTF-8"));
        assert_eq!(posix_locale("pt-BR").as_deref(), Some("pt_BR.UTF-8"));
    }

    /// The region already says which script is meant, so the script subtag is
    /// noise -- except where glibc itself keeps both, and then the name it
    /// keeps is the one in `SUPPORTED`: a modifier and no codeset.
    #[test]
    fn a_script_is_dropped_unless_glibc_keeps_it() {
        assert_eq!(posix_locale("zh-Hans-CN").as_deref(), Some("zh_CN.UTF-8"));
        assert_eq!(posix_locale("zh-Hant-TW").as_deref(), Some("zh_TW.UTF-8"));
        assert_eq!(posix_locale("sr-Latn-RS").as_deref(), Some("sr_RS@latin"));
        assert_eq!(posix_locale("sr-Cyrl-RS").as_deref(), Some("sr_RS.UTF-8"));
        assert_eq!(
            posix_locale("uz-Cyrl-UZ").as_deref(),
            Some("uz_UZ@cyrillic")
        );
        assert_eq!(posix_locale("uz-Latn-UZ").as_deref(), Some("uz_UZ.UTF-8"));
    }

    /// A POSIX name is a language *and* a territory. Inventing the territory
    /// the user did not choose is worse than falling back to the default.
    #[test]
    fn a_tag_without_a_two_letter_region_maps_to_nothing() {
        assert_eq!(posix_locale("en"), None);
        assert_eq!(posix_locale("es-419"), None);
        assert_eq!(posix_locale("zh-Hans"), None);
    }

    #[test]
    fn a_tag_that_is_not_a_tag_maps_to_nothing() {
        assert_eq!(posix_locale(""), None);
        assert_eq!(posix_locale("---"), None);
        assert_eq!(posix_locale("ru_RU"), None);
        assert_eq!(posix_locale("r-RU"), None);
    }
}
