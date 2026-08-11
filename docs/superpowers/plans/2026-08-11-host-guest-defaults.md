# TASK-60: хостовые locale, раскладка и timezone — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** прочитать локаль, раскладку и часовой пояс Windows и отдать их форме создания VM как `GuestDefaults`, вместо сегодняшних `en_US.UTF-8` / `us` / `Etc/UTC`.

**Architecture:** один новый модуль `crates/platform/src/host_guest_defaults.rs`. Внутри он делится на чистые функции отображения (`&str -> Option<String>`, покрыты юнит-тестами), три функции чтения Win32 (`unsafe`, тестами не покрываются) и сборку, которая на каждом промахе берёт соответствующее поле `GuestDefaults::default()`. Композиционный корень `crates/vmlord/src/main.rs` вызывает результат одной строкой в уже существующий шов `WorkspaceApp::with_guest_defaults`.

**Tech Stack:** Rust 2024, `windows` 0.61 (фичи `Win32_Globalization`, `Win32_System_Time`, `Win32_UI_Input_KeyboardAndMouse`), `windows-timezones` 0.5.1, `log`.

Спека: `docs/superpowers/specs/2026-08-11-host-guest-defaults-design.md`.

## Global Constraints

- Ветка `task-60-host-guest-defaults` уже создана; работать в ней.
- Каждый коммит начинается с `TASK-60: ` (AGENTS.md).
- Все Win32-вызовы и весь `unsafe` — только в `crates/platform`; там `unsafe_code = "allow"`, во всех остальных крейтах он `deny`.
- Сборка и тесты идут на Windows-таргет: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform`. Другой таргет `vmlord-platform` не собирает — в `lib.rs:9` стоит `compile_error!`.
- Ни один промах чтения или отображения не возвращает `Err` и не паникует: результат — `None`, а вызывающий подставляет дефолт. Уровень `ERROR` в этом модуле не используется.
- Комментарии и doc-комментарии — на английском, как во всём крейте; тексты логов — тоже на английском.
- Имена тестов — фразы-утверждения, как в соседних модулях (`the_host_locale_becomes_a_posix_name`), а не `test_foo`.

---

## Файловая структура

- **Создать** `crates/platform/src/host_guest_defaults.rs` — весь модуль: три отображения, три чтения, сборка, тесты. Около 350 строк вместе с тестами, что для этого крейта немного (`create.rs` — 47 КБ).
- **Изменить** `crates/platform/src/lib.rs` — объявление `mod host_guest_defaults;` и реэкспорт `host_guest_defaults`.
- **Изменить** `crates/platform/Cargo.toml` — зависимость `windows-timezones` и три новые фичи `windows`.
- **Изменить** `crates/vmlord/src/main.rs:37-42` — одна строка в цепочке построения `WorkspaceApp`.
- **Изменить** `ARCHITECTURE.md` — абзац про то, откуда берутся гостевые дефолты.

Порядок задач: сперва три отображения, каждое со своими тестами и своим коммитом, потом сборка, потом Win32 и проводка. Каждая задача заканчивается зелёными тестами.

---

### Task 1: часовой пояс — Windows ID в IANA

**Files:**
- Create: `crates/platform/src/host_guest_defaults.rs`
- Modify: `crates/platform/src/lib.rs` (добавить `mod host_guest_defaults;` в алфавитном порядке, между `mod host_dns;` и `mod import;`)
- Modify: `crates/platform/Cargo.toml`

**Interfaces:**
- Consumes: ничего.
- Produces: `fn iana_timezone(windows_id: &str) -> Option<String>` — приватная для модуля.

- [ ] **Step 1: добавить зависимость**

В `crates/platform/Cargo.toml`, в `[dependencies]`, в алфавитном порядке (после `uuid`, перед `windows`):

```toml
# The CLDR table that turns a Windows time-zone key -- `Russian Standard Time`
# -- into the IANA name a Linux guest wants. Generated data and nothing else:
# with default features it pulls in no dependencies of its own.
windows-timezones = "0.5.1"
```

- [ ] **Step 2: написать падающие тесты**

Создать `crates/platform/src/host_guest_defaults.rs` целиком:

```rust
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
fn iana_timezone(windows_id: &str) -> Option<String> {
    todo!()
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
```

В `crates/platform/src/lib.rs` добавить строку `mod host_guest_defaults;` сразу после `mod host_dns;`.

- [ ] **Step 3: убедиться, что тесты падают**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform host_guest_defaults`
Expected: три теста падают с паникой `not yet implemented`.

- [ ] **Step 4: реализовать**

Заменить тело `iana_timezone`:

```rust
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
```

- [ ] **Step 5: убедиться, что тесты проходят**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform host_guest_defaults`
Expected: `test result: ok. 3 passed`.

- [ ] **Step 6: коммит**

```bash
git add crates/platform/Cargo.toml crates/platform/src/lib.rs crates/platform/src/host_guest_defaults.rs Cargo.lock
git commit -m "TASK-60: Map a Windows time-zone key to its IANA name"
```

---

### Task 2: локаль — BCP-47 в POSIX

**Files:**
- Modify: `crates/platform/src/host_guest_defaults.rs`

**Interfaces:**
- Consumes: ничего из Task 1 (соседняя функция того же модуля).
- Produces: `fn posix_locale(bcp47: &str) -> Option<String>` — приватная для модуля.

- [ ] **Step 1: написать падающие тесты**

Добавить в `mod tests` (перед закрывающей скобкой):

```rust
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
        assert_eq!(posix_locale("uz-Cyrl-UZ").as_deref(), Some("uz_UZ@cyrillic"));
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
```

- [ ] **Step 2: убедиться, что тесты падают**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform host_guest_defaults`
Expected: не компилируется — `cannot find function posix_locale`.

- [ ] **Step 3: реализовать**

Добавить в модуль, после `iana_timezone`:

```rust
/// The script subtags glibc keeps in a locale name, and the modifier it
/// spells them with.
///
/// Everywhere else the region already says which script is meant, so the
/// subtag is dropped. These two are the pairs where the script *is* the
/// distinction, and the name below is the one in glibc's `SUPPORTED` -- which
/// is the list `locale-gen` reads, modifier and no codeset. `locale -a` prints
/// the same locales differently; matching it would generate nothing.
const SCRIPT_MODIFIERS: [(&str, &str, &str); 2] = [
    ("sr", "Latn", "latin"),
    ("uz", "Cyrl", "cyrillic"),
];

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
```

- [ ] **Step 4: убедиться, что тесты проходят**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform host_guest_defaults`
Expected: `test result: ok. 7 passed`.

- [ ] **Step 5: коммит**

```bash
git add crates/platform/src/host_guest_defaults.rs
git commit -m "TASK-60: Map a BCP-47 tag to a POSIX locale name"
```

---

### Task 3: раскладка — KLID в XKB

**Files:**
- Modify: `crates/platform/src/host_guest_defaults.rs`

**Interfaces:**
- Consumes: ничего из предыдущих задач.
- Produces: `fn xkb_layout(klid: &str) -> String` — приватная для модуля. Возвращает `String`, а не `Option`: у раскладки, в отличие от двух других полей, есть свой осмысленный ответ на любой вход, и он же — дефолт.

- [ ] **Step 1: написать падающие тесты**

Добавить в `mod tests`:

```rust
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
```

- [ ] **Step 2: убедиться, что тесты падают**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform host_guest_defaults`
Expected: не компилируется — `cannot find function xkb_layout`.

- [ ] **Step 3: реализовать**

Добавить в модуль, после `posix_locale`:

```rust
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
```

- [ ] **Step 4: убедиться, что тесты проходят**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform host_guest_defaults`
Expected: `test result: ok. 12 passed`.

- [ ] **Step 5: коммит**

```bash
git add crates/platform/src/host_guest_defaults.rs
git commit -m "TASK-60: Map a Windows keyboard identifier to an XKB layout"
```

---

### Task 4: сборка трёх ответов в `GuestDefaults`

**Files:**
- Modify: `crates/platform/src/host_guest_defaults.rs`

**Interfaces:**
- Consumes: `iana_timezone`, `posix_locale`, `xkb_layout` из задач 1–3.
- Produces: `fn guest_defaults(locale: Option<&str>, klid: Option<&str>, timezone: Option<&str>) -> GuestDefaults` — приватная для модуля. Три `Option` — это то, что вернуло чтение хоста; `None` означает «прочитать не удалось».

- [ ] **Step 1: написать падающие тесты**

Добавить в `mod tests`:

```rust
    #[test]
    fn a_host_that_reads_gives_the_guest_its_own_settings() {
        assert_eq!(
            guest_defaults(Some("ru-RU"), Some("00000419"), Some("Russian Standard Time")),
            GuestDefaults {
                locale: "ru_RU.UTF-8".into(),
                keyboard: "ru".into(),
                timezone: "Europe/Moscow".into(),
            }
        );
    }

    /// The promise the epic makes: an unreadable host setting is not a reason
    /// to refuse to create a VM.
    #[test]
    fn a_host_that_reads_nothing_gives_the_guest_the_defaults() {
        assert_eq!(guest_defaults(None, None, None), GuestDefaults::default());
    }

    /// The three fields are read and mapped apart, so one that fails leaves
    /// the other two alone.
    #[test]
    fn a_field_that_does_not_map_leaves_the_others_untouched() {
        assert_eq!(
            guest_defaults(Some("en"), Some("00000419"), Some("Russian Standard Time")),
            GuestDefaults {
                locale: GuestDefaults::default().locale,
                keyboard: "ru".into(),
                timezone: "Europe/Moscow".into(),
            }
        );
    }
```

- [ ] **Step 2: убедиться, что тесты падают**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform host_guest_defaults`
Expected: не компилируется — `cannot find function guest_defaults`.

- [ ] **Step 3: реализовать**

Добавить в модуль, после `xkb_layout`:

```rust
/// Turns what the host said into what the create form starts out with.
///
/// Separate from the Win32 reads above it so that the fallbacks -- the part
/// worth being sure about -- can be tested without a host to read.
///
/// Each field falls back on its own. A host whose keyboard is unrecognised
/// still hands the guest its own timezone.
fn guest_defaults(
    locale: Option<&str>,
    klid: Option<&str>,
    timezone: Option<&str>,
) -> GuestDefaults {
    let fallback = GuestDefaults::default();

    let locale = match locale.and_then(posix_locale) {
        Some(mapped) => mapped,
        None => {
            log::warn!(
                "the host locale {} has no POSIX name; the guest starts out with {}",
                locale.unwrap_or("<unreadable>"),
                fallback.locale
            );
            fallback.locale
        }
    };
    let keyboard = xkb_layout(klid.unwrap_or_default());
    let timezone = match timezone.and_then(iana_timezone) {
        Some(mapped) => mapped,
        None => {
            log::warn!(
                "the host time zone {} has no IANA name; the guest starts out with {}",
                timezone.unwrap_or("<unreadable>"),
                fallback.timezone
            );
            fallback.timezone
        }
    };

    log::info!("a new VM starts out with locale {locale}, keyboard {keyboard}, timezone {timezone}");
    GuestDefaults {
        locale,
        keyboard,
        timezone,
    }
}
```

Заодно поднять `posix_locale` и `iana_timezone` до сигнатуры, которую здесь ждёт `and_then`: обе уже `fn(&str) -> Option<String>`, и `and_then` над `Option<&str>` вызывает их напрямую — менять ничего не нужно.

- [ ] **Step 4: убедиться, что тесты проходят**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform host_guest_defaults`
Expected: `test result: ok. 15 passed`.

- [ ] **Step 5: коммит**

```bash
git add crates/platform/src/host_guest_defaults.rs
git commit -m "TASK-60: Fall back per field when a host setting does not map"
```

---

### Task 5: чтение Windows и проводка в композиционный корень

**Files:**
- Modify: `crates/platform/src/host_guest_defaults.rs`
- Modify: `crates/platform/Cargo.toml` (три фичи `windows`)
- Modify: `crates/platform/src/lib.rs` (реэкспорт)
- Modify: `crates/vmlord/src/main.rs:37-42`
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Consumes: `guest_defaults` из Task 4.
- Produces: `pub fn host_guest_defaults() -> GuestDefaults`, реэкспортированная как `vmlord_platform::host_guest_defaults`.

Юнит-тестов в этой задаче нет, и это осознанно: результат трёх функций чтения — настройки той машины, на которой идут тесты, так что утверждать про них нечего. Проверка — ручная, на реальной Windows, по критерию выхода спеки.

- [ ] **Step 1: добавить фичи `windows`**

В `crates/platform/Cargo.toml`, в список `windows = { workspace = true, features = [...] }`, в алфавитном порядке:

```toml
    "Win32_Globalization",
```
перед `"Win32_NetworkManagement_IpHelper"`, и

```toml
    "Win32_System_Time",
    "Win32_UI_Input_KeyboardAndMouse",
```
после `"Win32_System_Threading"`.

- [ ] **Step 2: реализовать чтение**

Дописать в `crates/platform/src/host_guest_defaults.rs` — импорты вверху файла, к уже существующим:

```rust
use windows::Win32::{
    Globalization::GetUserDefaultLocaleName,
    System::Time::{DYNAMIC_TIME_ZONE_INFORMATION, GetDynamicTimeZoneInformation},
    UI::Input::KeyboardAndMouse::GetKeyboardLayoutNameW,
};
```

и функции — перед `guest_defaults`, чтобы читалось сверху вниз:

```rust
/// What a new VM starts out with on this host.
///
/// Read once, at startup: these settings change rarely, and every one of the
/// three fields stays editable in the create form regardless.
#[must_use]
pub fn host_guest_defaults() -> GuestDefaults {
    guest_defaults(
        host_locale().as_deref(),
        host_klid().as_deref(),
        host_time_zone_key().as_deref(),
    )
}

/// The BCP-47 tag of the user's locale, `ru-RU`.
fn host_locale() -> Option<String> {
    // `LOCALE_NAME_MAX_LENGTH`, which the buffer is documented to need.
    let mut buffer = [0u16; 85];
    let written = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    if written <= 0 {
        log::warn!("the host locale could not be read");
        return None;
    }
    let tag = terminated(&buffer);
    log::debug!("the host locale is {tag}");
    Some(tag)
}

/// The Windows keyboard identifier of the calling thread's layout, `00000419`.
///
/// The calling thread is the composition root before it has a window, where
/// the layout is the user's default input profile.
fn host_klid() -> Option<String> {
    let mut buffer = [0u16; 9];
    if let Err(error) = unsafe { GetKeyboardLayoutNameW(&mut buffer) } {
        log::warn!("the host keyboard layout could not be read: {error}");
        return None;
    }
    let klid = terminated(&buffer);
    log::debug!("the host keyboard identifier is {klid}");
    Some(klid)
}

/// The invariant registry key of the host's time zone, `Russian Standard Time`.
fn host_time_zone_key() -> Option<String> {
    // `TIME_ZONE_ID_INVALID`, the one return value that means nothing was
    // written; the other three each describe a zone that was.
    const TIME_ZONE_ID_INVALID: u32 = 0xffff_ffff;

    let mut information = DYNAMIC_TIME_ZONE_INFORMATION::default();
    if unsafe { GetDynamicTimeZoneInformation(&mut information) } == TIME_ZONE_ID_INVALID {
        log::warn!("the host time zone could not be read");
        return None;
    }
    let key = terminated(&information.TimeZoneKeyName);
    log::debug!("the host time zone key is {key}");
    Some(key)
}

/// The string in a fixed-size UTF-16 buffer, up to its first NUL.
fn terminated(buffer: &[u16]) -> String {
    let length = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}
```

- [ ] **Step 3: реэкспорт**

В `crates/platform/src/lib.rs`, в блок `pub use`, в алфавитном порядке — между `pub use hcs_config`-соседями и `pub use import::...` стоит строка после `pub use hcn_endpoint::{...};`:

```rust
pub use host_guest_defaults::host_guest_defaults;
```

- [ ] **Step 4: проверить сборку и все тесты крейта**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform`
Expected: всё зелёное, включая пятнадцать тестов `host_guest_defaults`. Предупреждений clippy быть не должно: `cargo clippy --target x86_64-pc-windows-gnu -p vmlord-platform` — чисто.

- [ ] **Step 5: проводка в композиционный корень**

В `crates/vmlord/src/main.rs`, в цепочке построения приложения (строки 37–42), добавить вызов первым звеном:

```rust
    let mut application = vmlord_app::WorkspaceApp::new(repository)
        .with_guest_defaults(vmlord_platform::host_guest_defaults())
        .with_image_picker(Box::new(vmlord_legacy_backend::WindowsImagePicker::new()))
        .with_settings_path_picker(Box::new(
            vmlord_legacy_backend::WindowsSettingsPathPicker::new(),
        ));
```

Место в цепочке имеет значение только одним: вызов идёт после `initialize_logging`, которое уже отработало выше при загрузке настроек, — иначе `DEBUG`- и `WARN`-строки этого модуля не попали бы в файл лога.

- [ ] **Step 6: собрать всё рабочее пространство**

Run: `cargo test --target x86_64-pc-windows-gnu`
Expected: всё зелёное; ни один существующий тест не поменял поведения — шов `with_guest_defaults` и его тест в `app/src/lib.rs` уже были на месте.

- [ ] **Step 7: обновить ARCHITECTURE.md**

Абзац уже есть и заканчивается обещанием, которое эта задача выполняет, — `ARCHITECTURE.md:1112-1118`, «Three fields are filled from the host…», последнее предложение «#60 replaces what VMLord passes in with the values mapped from Windows; nothing else moves when it does.» Заменить абзац целиком на:

```markdown
Three fields are filled from the host: locale, keyboard layout and timezone.
`platform::host_guest_defaults` reads them once at startup --
`GetUserDefaultLocaleName`, `GetKeyboardLayoutNameW` and
`GetDynamicTimeZoneInformation` -- and maps each into what the guest names the
same thing: a POSIX locale, an XKB layout, an IANA zone. The timezone is mapped
by the CLDR table in `windows-timezones` from `TimeZoneKeyName`, the invariant
registry key, rather than from the localized `StandardName` -- which on a
Russian Windows reads «Русское стандартное время» and appears in no table. The
keyboard has no such crate anywhere, so it has a table of its own: the frequent
KLIDs, then the layout without its variant, then the language alone, then `us`.
`GuestDefaults` carries the three from the composition root through
`WorkspaceApp::with_guest_defaults`, and its `Default` -- `en_US.UTF-8`, `us`,
`Etc/UTC` -- is what each field falls back to on its own, so a host whose
keyboard is unrecognised still hands the guest its own timezone, and a VM is
created in a state that works rather than not created at all.
```

- [ ] **Step 8: коммит**

```bash
git add crates/platform/Cargo.toml crates/platform/src/lib.rs crates/platform/src/host_guest_defaults.rs crates/vmlord/src/main.rs ARCHITECTURE.md
git commit -m "TASK-60: Read the guest defaults from the host at startup"
```

---

## Ручная проверка на Windows (за владельцем проекта)

1. Запустить VMLord на русской Windows, открыть форму создания VM: поля показывают `ru_RU.UTF-8`, `ru`, `Europe/Moscow` без правок руками.
2. Все три поля по-прежнему редактируются, и введённое значение доезжает до создаваемой VM.
3. В `vmlord.log` на уровне `DEBUG` видны три прочитанные строки хоста и одна `INFO` с итогом.
4. Переключить в Windows раскладку на редкую (например, `Hausa`), перезапустить: поле показывает `us`, в логе — `WARN` с исходным KLID, два других поля верны.
