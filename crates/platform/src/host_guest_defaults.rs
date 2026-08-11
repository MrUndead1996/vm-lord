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

/// What a KLID means to X11, for the layouts a host is likely to be set to.
///
/// No crate maps these: the Windows side is a registry identifier and the
/// Linux side is an `xkeyboard-config` name, and nothing publishes the join.
/// The list is the frequent layouts rather than every one Windows ships --
/// what is missing falls through to the language below, and then to `us`.
const KEYBOARD_LAYOUTS: [(u32, &str); 50] = [
    (0x0000_0409, "us"),
    (0x0000_0809, "gb"),
    (0x0000_0407, "de"),
    (0x0000_0807, "ch"),
    (0x0000_040c, "fr"),
    (0x0000_080c, "be"),
    (0x0000_0c0c, "ca"),
    (0x0000_040a, "es"),
    (0x0000_080a, "latam"),
    (0x0000_0410, "it"),
    (0x0000_0816, "pt"),
    (0x0000_0416, "br"),
    (0x0000_0413, "nl"),
    (0x0000_041d, "se"),
    (0x0000_0406, "dk"),
    (0x0000_0414, "no"),
    (0x0000_040b, "fi"),
    (0x0000_040f, "is"),
    (0x0000_0415, "pl"),
    (0x0000_0405, "cz"),
    (0x0000_041b, "sk"),
    (0x0000_040e, "hu"),
    (0x0000_0418, "ro"),
    (0x0000_0402, "bg"),
    (0x0000_041a, "hr"),
    (0x0000_0424, "si"),
    (0x0000_081a, "rs"),
    (0x0000_0c1a, "rs"),
    (0x0000_0419, "ru"),
    (0x0000_0422, "ua"),
    (0x0000_0423, "by"),
    (0x0000_043f, "kz"),
    (0x0000_0408, "gr"),
    (0x0000_041f, "tr"),
    (0x0000_042c, "az"),
    (0x0000_040d, "il"),
    (0x0000_0401, "ara"),
    (0x0000_0429, "ir"),
    (0x0000_041e, "th"),
    (0x0000_0411, "jp"),
    (0x0000_0412, "kr"),
    (0x0000_0804, "cn"),
    (0x0000_0404, "tw"),
    (0x0000_042a, "vn"),
    (0x0000_0425, "ee"),
    (0x0000_0426, "lv"),
    (0x0000_0427, "lt"),
    (0x0000_0439, "in"),
    (0x0000_042f, "mk"),
    (0x0000_041c, "al"),
];

/// The layout a primary language implies, for the sublanguages
/// [`KEYBOARD_LAYOUTS`] does not list one by one.
///
/// Keyed by the low ten bits of a LANGID, which is the language without its
/// country: every Spanish of Latin America shares `0x0a`.
const LANGUAGE_LAYOUTS: [(u16, &str); 24] = [
    (0x0009, "us"),
    (0x0007, "de"),
    (0x000c, "fr"),
    (0x000a, "es"),
    (0x0010, "it"),
    (0x0016, "pt"),
    (0x0013, "nl"),
    (0x001d, "se"),
    (0x0006, "dk"),
    (0x0014, "no"),
    (0x000b, "fi"),
    (0x0015, "pl"),
    (0x0005, "cz"),
    (0x000e, "hu"),
    (0x0018, "ro"),
    (0x0002, "bg"),
    (0x0019, "ru"),
    (0x0022, "ua"),
    (0x0008, "gr"),
    (0x001f, "tr"),
    (0x000d, "il"),
    (0x0001, "ara"),
    (0x0011, "jp"),
    (0x0004, "cn"),
];

/// The XKB layout name for the Windows keyboard identifier `klid`.
///
/// `klid` is eight hexadecimal digits, `00000419`. Four steps, each one wider
/// than the last: the identifier itself, the identifier without its variant,
/// the language alone, and finally `us` -- which the epic names as the
/// fallback that keeps an unrecognised host from blocking a VM.
fn xkb_layout(klid: &str) -> String {
    const DEFAULT_LAYOUT: &str = "us";

    let Ok(identifier) = u32::from_str_radix(klid.trim(), 16) else {
        log::warn!("the host keyboard identifier \"{klid}\" is not a KLID; using {DEFAULT_LAYOUT}");
        return DEFAULT_LAYOUT.to_owned();
    };

    let language_id = (identifier & 0xffff) as u16;
    let base = u32::from(language_id);
    let primary_language = language_id & 0x03ff;

    let found = KEYBOARD_LAYOUTS
        .iter()
        .find(|(known, _)| *known == identifier || *known == base)
        .map(|(_, layout)| *layout)
        .or_else(|| {
            LANGUAGE_LAYOUTS
                .iter()
                .find(|(known, _)| *known == primary_language)
                .map(|(_, layout)| *layout)
        });

    match found {
        Some(layout) => layout.to_owned(),
        None => {
            log::warn!("no XKB layout is known for the host KLID {klid}; using {DEFAULT_LAYOUT}");
            DEFAULT_LAYOUT.to_owned()
        }
    }
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

    #[test]
    fn a_known_klid_becomes_its_xkb_layout() {
        assert_eq!(xkb_layout("00000419"), "ru");
        assert_eq!(xkb_layout("00000409"), "us");
        assert_eq!(xkb_layout("0000080a"), "latam");
    }

    /// Windows numbers a variant of a layout by setting the high word. The
    /// base layout is the right answer for a variant the table does not list:
    /// US-International is still a `us` keyboard.
    #[test]
    fn a_variant_falls_back_to_its_base_layout() {
        assert_eq!(xkb_layout("00020409"), "us");
        assert_eq!(xkb_layout("00010419"), "ru");
    }

    /// Sublanguages the table does not list still name a language, and the
    /// language names a layout.
    #[test]
    fn an_unlisted_sublanguage_falls_back_to_its_language() {
        assert_eq!(xkb_layout("00000c0a"), "es");
        assert_eq!(xkb_layout("00001407"), "de");
    }

    /// The last resort the epic names: a layout nobody recognises does not
    /// stop a VM from being created.
    #[test]
    fn an_unrecognised_klid_falls_back_to_us() {
        assert_eq!(xkb_layout("0000ffff"), "us");
        assert_eq!(xkb_layout("not a klid"), "us");
        assert_eq!(xkb_layout(""), "us");
    }

    /// Windows writes a KLID in lower case; a table that only matched that
    /// would be one `to_ascii_lowercase` away from a silent fallback.
    #[test]
    fn a_klid_is_read_regardless_of_its_case() {
        assert_eq!(xkb_layout("0000041D"), "se");
    }
}
