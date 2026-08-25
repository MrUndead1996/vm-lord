# UI localization implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The VMLord desktop shell reads its text from message catalogues and ships a Russian one, and the settings dialog switches language while the application runs.

**Architecture:** `rust-i18n` embeds two TOML catalogues into `vmlord-ui` at compile time. Every user-facing literal in `crates/ui/src/lib.rs` becomes a `t!` lookup. The locale is set from `AppSettings::language` when the window opens and again when the settings dialog saves; egui rebuilds every frame from the catalogue, so the change is visible without a restart. Nothing outside `vmlord-ui` learns about localization.

**Tech Stack:** Rust 2024, `eframe`/`egui` 0.33, `rust-i18n` 3, `toml` 0.8 (test only), `cargo test-windows`.

**Spec:** `docs/superpowers/specs/2026-08-25-ui-localization-design.md`

## Global Constraints

- Only `vmlord-ui` depends on `rust-i18n`. `core`, `app`, `platform` must not gain the dependency.
- Diagnostics raised through `vmlord_core::diagnostic!` and the `Display` text of `vmlord-core` error types stay in English and are not touched by any task.
- Catalogue files: `crates/ui/locales/en-US.toml` and `crates/ui/locales/ru-RU.toml`. `_version` is left unset, which means version 1 -- one file per locale.
- Placeholders use the `rust-i18n` syntax `%{name}`, never positional `{}`.
- The English catalogue must carry the *exact* text the code has today, character for character. Tests already in `crates/ui/src/lib.rs` assert on those strings and must keep passing untouched.
- No test may call `rust_i18n::set_locale`. A test that needs Russian passes `locale = "ru-RU"` to `t!`. The locale is global, and a test that changes it corrupts every test that runs after it.
- Widget ids passed to `egui::ComboBox::from_id_salt` and `egui::Grid::new` (`"create-vm-form"`, `"vm-list"`, `"settings-language"` and the rest) are identifiers, not text. Leave them alone.
- Units and proper names that are the same in both languages -- `MiB`, `GiB`, `NAT`, `GPU`, `SSH`, `GNOME`, `VMLord` -- stay as literals where they stand alone.
- Every key added to one catalogue is added to the other in the same commit. The parity test from Task 2 enforces it.
- Functions returning `&'static str` that start returning catalogue text change to `String` (or `Option<String>`). `assert_eq!` against a `&str` literal keeps compiling.
- Commit subjects are `TASK-18: <comment>`, per **AGENTS.md**.
- Build and test with `cargo test-windows`. It runs the Windows binary through WSL interop.

---

### Task 1: Russian is a language the settings can hold

**Files:**
- Modify: `crates/core/src/settings.rs:75-80` (the `Language` enum), and its test module at `crates/core/src/settings.rs:287`

**Interfaces:**
- Produces: `vmlord_core::Language::RuRu`, and `Language::code(self) -> &'static str` returning `"en-US"` or `"ru-RU"`. Task 2 onwards drive `rust_i18n::set_locale` with `code()`.

- [ ] **Step 1: Write the failing tests**

Add to the test module at the bottom of `crates/core/src/settings.rs`:

```rust
#[test]
fn a_language_is_stored_under_its_locale_tag() {
    let settings = AppSettings {
        language: Language::RuRu,
        ..default_settings()
    };

    let document = toml::to_string_pretty(&settings).expect("settings serialize");

    assert!(document.contains(r#"language = "ru-RU""#), "{document}");
}

#[test]
fn a_stored_locale_tag_loads_back() {
    let settings = AppSettings {
        language: Language::RuRu,
        ..default_settings()
    };
    let document = toml::to_string_pretty(&settings).expect("settings serialize");

    let loaded: AppSettings = toml::from_str(&document).expect("settings load");

    assert_eq!(loaded.language, Language::RuRu);
}

#[test]
fn each_language_names_its_locale() {
    assert_eq!(Language::EnUs.code(), "en-US");
    assert_eq!(Language::RuRu.code(), "ru-RU");
}
```

`default_settings()` is a helper the test module needs if it has none: read `crates/core/src/settings.rs:370-400`, where two tests already build an `AppSettings` literal, and lift that literal into

```rust
fn default_settings() -> AppSettings {
    AppSettings {
        vm_storage_path: PathBuf::from("vms"),
        language: Language::EnUs,
        log_file_path: PathBuf::from("vmlord.log"),
        log_level: LogLevel::Info,
        image_cache_path: PathBuf::from("images"),
        guest_readiness: GuestReadinessTimeouts::default(),
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-core settings`
Expected: FAIL -- `no variant named RuRu found for enum Language`, `no method named code`.

- [ ] **Step 3: Add the variant and the accessor**

In `crates/core/src/settings.rs`, replace the enum:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "en-US")]
    #[default]
    EnUs,
    #[serde(rename = "ru-RU")]
    RuRu,
}

impl Language {
    /// The BCP 47 tag the UI's message catalogues are named after.
    ///
    /// The tag is spelled here and nowhere else: `serde` writes it into
    /// `settings.toml`, and the UI hands the same string to its i18n backend,
    /// so the two cannot drift apart.
    pub fn code(self) -> &'static str {
        match self {
            Self::EnUs => "en-US",
            Self::RuRu => "ru-RU",
        }
    }
}
```

`EnUs` keeps `#[default]`: a fresh installation starts in English, and the language is chosen at install time rather than guessed from the host.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-core settings`
Expected: PASS, including the existing `assert_eq!(settings.language, Language::EnUs)` at `crates/core/src/settings.rs:314`.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/settings.rs
git commit -m "TASK-18: Let the settings hold a second language"
```

---

### Task 2: The settings dialog speaks both languages

This task wires the catalogue machinery and proves it on one dialog end to end: opening settings, choosing Русский and pressing Save must repaint the dialog in Russian.

**Files:**
- Modify: `crates/ui/Cargo.toml`
- Create: `crates/ui/locales/en-US.toml`, `crates/ui/locales/ru-RU.toml`
- Modify: `crates/ui/src/lib.rs:1-20` (imports and the `i18n!` call), `:54-74` (`run`), `:173-186` (`SettingsForm::settings`), `:527-535` (the `Submit` arm), `:928-1028` (`render_settings_dialog`), `:1335-1349` (`log_level_label`, `language_label`)

**Interfaces:**
- Consumes: `vmlord_core::Language::code` from Task 1.
- Produces: `rust_i18n::i18n!("locales", fallback = "en-US")` in `crates/ui/src/lib.rs`, so every later task can call `t!`. The `[common]` catalogue table -- `common.cancel`, `common.save`, `common.browse`, `common.none`, `common.default`, `common.unknown`, `common.unavailable`, `common.disabled` -- is shared by all later tasks. Also `fn catalogue_keys(document: &str) -> BTreeSet<String>` in the test module, used by the parity test alone.

- [ ] **Step 1: Add the dependencies**

In `crates/ui/Cargo.toml`:

```toml
[dependencies]
eframe = "0.33"
# The UI's message catalogues, embedded at compile time from `locales/`.
# It stops here on purpose: no other crate has text a user reads.
rust-i18n = "3"
vmlord-app = { path = "../app" }
vmlord-core = { path = "../core" }

[dev-dependencies]
# Read back by the catalogue parity test, and by nothing else. The same
# version `vmlord-core` parses `settings.toml` with.
toml = "0.8"
```

- [ ] **Step 2: Write the failing parity test**

Add to the test module in `crates/ui/src/lib.rs`:

```rust
/// Every key of one catalogue exists in the other.
///
/// A forgotten translation would otherwise fall back to English silently,
/// which looks like a rendering bug months later rather than a missing line
/// in a pull request.
#[test]
fn the_catalogues_agree_on_their_keys() {
    let english = catalogue_keys(include_str!("../locales/en-US.toml"));
    let russian = catalogue_keys(include_str!("../locales/ru-RU.toml"));

    let missing_in_russian: Vec<_> = english.difference(&russian).collect();
    let missing_in_english: Vec<_> = russian.difference(&english).collect();

    assert!(missing_in_russian.is_empty(), "not translated: {missing_in_russian:?}");
    assert!(missing_in_english.is_empty(), "no English original: {missing_in_english:?}");
}

/// The dotted paths of every string in a catalogue.
fn catalogue_keys(document: &str) -> std::collections::BTreeSet<String> {
    fn walk(
        prefix: &str,
        value: &toml::Value,
        keys: &mut std::collections::BTreeSet<String>,
    ) {
        match value {
            toml::Value::Table(table) => {
                for (key, value) in table {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(&path, value, keys);
                }
            }
            _ => {
                keys.insert(prefix.to_string());
            }
        }
    }

    let document: toml::Value = document.parse().expect("catalogue parses");
    let mut keys = std::collections::BTreeSet::new();
    walk("", &document, &mut keys);
    keys
}

#[test]
fn the_settings_dialog_is_translated() {
    assert_eq!(t!("settings.title", locale = "ru-RU"), "Настройки приложения");
    assert_ne!(
        t!("settings.title", locale = "ru-RU"),
        t!("settings.title", locale = "en-US")
    );
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-ui catalogue`
Expected: FAIL -- `couldn't read ../locales/en-US.toml`, and `t!` unresolved.

- [ ] **Step 4: Write the catalogues**

`crates/ui/locales/en-US.toml`:

```toml
[common]
cancel = "Cancel"
save = "Save"
browse = "Browse..."
none = "None"
default = "Default"
unknown = "Unknown"
unavailable = "Unavailable"
disabled = "Disabled"

[settings]
title = "Application settings"
description = "Configure where VMLord stores VM data and diagnostic logs."
vm_storage = "VM storage"
vm_storage_hint = "Directory for virtual machine data"
language = "Language"
log_file = "Log file"
log_file_hint = "Path to the log file"
log_level = "Log level"
vm_storage_required = "VM storage path is required."
log_file_required = "Log file path is required."

[log_level]
error = "Error"
warning = "Warning"
info = "Info"
debug = "Debug"
trace = "Trace"
```

`crates/ui/locales/ru-RU.toml`:

```toml
[common]
cancel = "Отмена"
save = "Сохранить"
browse = "Обзор..."
none = "Нет"
default = "По умолчанию"
unknown = "Неизвестно"
unavailable = "Недоступно"
disabled = "Отключено"

[settings]
title = "Настройки приложения"
description = "Где VMLord хранит данные виртуальных машин и журналы диагностики."
vm_storage = "Хранилище ВМ"
vm_storage_hint = "Каталог для данных виртуальных машин"
language = "Язык"
log_file = "Файл журнала"
log_file_hint = "Путь к файлу журнала"
log_level = "Уровень журнала"
vm_storage_required = "Укажите путь к хранилищу ВМ."
log_file_required = "Укажите путь к файлу журнала."

[log_level]
error = "Ошибки"
warning = "Предупреждения"
info = "Информация"
debug = "Отладка"
trace = "Трассировка"
```

- [ ] **Step 5: Load the catalogues**

At the top of `crates/ui/src/lib.rs`, after the existing `use` block:

```rust
use rust_i18n::t;

// The catalogues in `locales/`, embedded at compile time. English is the
// fallback, so a key missing from another catalogue shows English rather than
// its own name -- and the parity test keeps that from happening unnoticed.
rust_i18n::i18n!("locales", fallback = "en-US");
```

- [ ] **Step 6: Run the parity test to verify it passes**

Run: `cargo test-windows -p vmlord-ui catalogue`
Expected: PASS.

- [ ] **Step 7: Set the locale when the window opens and when settings are saved**

In `run` (`crates/ui/src/lib.rs:54`), before `eframe::run_native`:

```rust
// Settings that failed to load leave the locale at the fallback, which is
// where a fresh installation starts anyway.
if let Some(settings) = application.settings() {
    rust_i18n::set_locale(settings.language.code());
}
```

In the `Submit` arm at `crates/ui/src/lib.rs:527`:

```rust
Some(SettingsDialogAction::Submit(settings)) => {
    let language = settings.language;
    if let Err(error) = self.application.update_settings(settings) {
        if let Some(form) = &mut self.settings_form {
            form.error = Some(error.to_string());
        }
    } else {
        // egui rebuilds every frame from the catalogue, so this is the
        // whole of switching language: no restart, no reload.
        rust_i18n::set_locale(language.code());
        self.settings_form = None;
    }
}
```

- [ ] **Step 8: Translate the dialog**

In `render_settings_dialog` (`crates/ui/src/lib.rs:928`) replace each literal with its key: `"Application settings"` with `t!("settings.title")`, `"Configure where VMLord stores VM data and diagnostic logs."` with `t!("settings.description")`, `"VM storage"` with `t!("settings.vm_storage")`, the hint text with `t!("settings.vm_storage_hint")`, `"Language"` with `t!("settings.language")`, `"Log file"` with `t!("settings.log_file")`, its hint with `t!("settings.log_file_hint")`, `"Log level"` with `t!("settings.log_level")`, both `"Browse..."` buttons with `t!("common.browse")`, `"Save"` with `t!("common.save")` and `"Cancel"` with `t!("common.cancel")`.

`egui::Label::new` and `egui::Button::new` take `impl Into<WidgetText>`, which `Cow<str>` does not implement -- write `t!("settings.title").to_string()` where the compiler asks for it.

Add the second language to the combo box at `crates/ui/src/lib.rs:960`:

```rust
ui.selectable_value(&mut form.language, Language::EnUs, "English (US)");
ui.selectable_value(&mut form.language, Language::RuRu, "Русский");
```

and the labels at `crates/ui/src/lib.rs:1335-1349`:

```rust
fn log_level_label(level: LogLevel) -> String {
    match level {
        LogLevel::Error => t!("log_level.error"),
        LogLevel::Warn => t!("log_level.warning"),
        LogLevel::Info => t!("log_level.info"),
        LogLevel::Debug => t!("log_level.debug"),
        LogLevel::Trace => t!("log_level.trace"),
    }
    .to_string()
}

/// Each language is named in itself, and neither name is translated: a user
/// who cannot read the language on screen has to find their own in this list.
fn language_label(language: Language) -> &'static str {
    match language {
        Language::EnUs => "English (US)",
        Language::RuRu => "Русский",
    }
}
```

The five `selectable_value` calls for the log level at `crates/ui/src/lib.rs:986-990` pass `log_level_label(LogLevel::Error)` and so on instead of their literals.

- [ ] **Step 9: Translate the form's validation messages**

In `SettingsForm::settings` (`crates/ui/src/lib.rs:173`):

```rust
if vm_storage_path.is_empty() {
    return Err(t!("settings.vm_storage_required").to_string());
}
```

and the same shape for `log_file_path` with `t!("settings.log_file_required")`.

- [ ] **Step 10: Run the whole UI suite**

Run: `cargo test-windows -p vmlord-ui`
Expected: PASS. Nothing here changes English text, so the tests that assert on it are untouched.

- [ ] **Step 11: Commit**

```bash
git add crates/ui/Cargo.toml crates/ui/locales crates/ui/src/lib.rs Cargo.lock
git commit -m "TASK-18: Read the settings dialog from a message catalogue"
```

---

### Task 3: The main window, the VM table and the state labels

**Files:**
- Modify: `crates/ui/locales/en-US.toml`, `crates/ui/locales/ru-RU.toml`
- Modify: `crates/ui/src/lib.rs:306-360` (the top bar of `update`), `:1457-1525` (the label helpers), `:1560-1643` (`render_backend_status`, `render_vm_list`), `:2288-2335` (`render_agent_status`, `agent_status_label`, `vm_state_label`), `:2398-2412` (`vm_state`)

**Interfaces:**
- Consumes: `t!` and the `[common]` table from Task 2.
- Produces: `fn cores_label(count: u32) -> String`, used by Task 4 as well.

- [ ] **Step 1: Write the failing plural test**

In the test module of `crates/ui/src/lib.rs`:

```rust
#[test]
fn a_core_count_takes_the_form_russian_asks_for() {
    assert_eq!(t!("vm_table.cores_one", locale = "ru-RU", count = 1), "1 ядро");
    assert_eq!(t!("vm_table.cores_few", locale = "ru-RU", count = 2), "2 ядра");
    assert_eq!(t!("vm_table.cores_many", locale = "ru-RU", count = 5), "5 ядер");
}

#[test]
fn the_plural_form_follows_the_count() {
    assert_eq!(plural_form(1), PluralForm::One);
    assert_eq!(plural_form(2), PluralForm::Few);
    assert_eq!(plural_form(5), PluralForm::Many);
    assert_eq!(plural_form(11), PluralForm::Many);
    assert_eq!(plural_form(21), PluralForm::One);
    assert_eq!(plural_form(0), PluralForm::Many);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test-windows -p vmlord-ui plural`
Expected: FAIL -- `cannot find function plural_form`.

- [ ] **Step 3: Add the plural helper**

In `crates/ui/src/lib.rs`, beside the other label helpers:

```rust
/// Which of the three forms a counted noun takes.
///
/// Russian inflects a counted noun three ways -- 1 ядро, 2 ядра, 5 ядер --
/// and the catalogue backend carries no plural rules. A rule engine would be
/// a large answer to one string: `"{} cores"` is the only place in the UI
/// where a number stands before a noun that bends. English is served by the
/// same three keys, because its rule -- one against everything else -- is a
/// coarsening of this one, and `en-US.toml` repeats itself in the last two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PluralForm {
    One,
    Few,
    Many,
}

fn plural_form(count: u32) -> PluralForm {
    let last_two = count % 100;
    if (11..=14).contains(&last_two) {
        return PluralForm::Many;
    }
    match count % 10 {
        1 => PluralForm::One,
        2..=4 => PluralForm::Few,
        _ => PluralForm::Many,
    }
}

fn cores_label(count: u32) -> String {
    match plural_form(count) {
        PluralForm::One => t!("vm_table.cores_one", count = count),
        PluralForm::Few => t!("vm_table.cores_few", count = count),
        PluralForm::Many => t!("vm_table.cores_many", count = count),
    }
    .to_string()
}
```

- [ ] **Step 4: Extend the catalogues**

Append to `crates/ui/locales/en-US.toml`:

```toml
[app]
subtitle = "Linux workspaces on Windows"
refresh = "Refresh"
refresh_hint = "Available when the backend is ready"
create_vm = "Create VM"
create_vm_hint = "Create a virtual machine"
settings = "Settings"
settings_hint = "Open application settings"
backend_starting = "Backend: starting…"
backend_ready = "Backend: ready"
backend_unavailable = "Backend unavailable: %{message}"

[vm_table]
title = "Workspaces"
empty = "No virtual machines found."
name = "Name"
os = "OS"
status = "Status"
agent_status = "Agent status"
cpu = "CPU"
ram = "RAM"
disk = "Disk"
network_type = "Network type"
cores_one = "%{count} core"
cores_few = "%{count} cores"
cores_many = "%{count} cores"
mebibytes = "%{count} MiB"
gibibytes = "%{count} GiB"

[agent_status]
offline = "Offline"
online = "Online"

[vm_state]
stopped = "Stopped"
starting = "Starting"
running = "Running"
building_downloading = "Building: downloading"
building_writing_disk = "Building: writing the disk"
building_provisioning = "Building: provisioning"
building_registering = "Building: registering"
building_starting = "Building: starting the VM"
building_waiting = "Building: waiting for the guest"
with_percentage = "%{label} %{percent}%"

[gpu_state]
disabled = "Disabled"
waiting_for_guest = "Waiting for guest"
assigned = "Assigned"
ready = "Ready"
degraded = "Degraded"
failed = "Failed"

[display_state]
installing = "Installing"
ready = "Ready"
degraded = "Degraded"
unsupported = "Unsupported"

[desktop_profile]
headless = "None (headless)"
gnome = "GNOME"

[gpu_mode]
none = "None"
mirror = "Mirror"
unsupported = "Unsupported"

[network_mode]
nat = "NAT"
external = "External"
internal = "Internal"
```

Append to `crates/ui/locales/ru-RU.toml`:

```toml
[app]
subtitle = "Linux-окружения в Windows"
refresh = "Обновить"
refresh_hint = "Доступно, когда бэкенд готов"
create_vm = "Создать ВМ"
create_vm_hint = "Создать виртуальную машину"
settings = "Настройки"
settings_hint = "Открыть настройки приложения"
backend_starting = "Бэкенд: запускается…"
backend_ready = "Бэкенд: готов"
backend_unavailable = "Бэкенд недоступен: %{message}"

[vm_table]
title = "Окружения"
empty = "Виртуальных машин нет."
name = "Имя"
os = "ОС"
status = "Состояние"
agent_status = "Состояние агента"
cpu = "CPU"
ram = "RAM"
disk = "Диск"
network_type = "Тип сети"
cores_one = "%{count} ядро"
cores_few = "%{count} ядра"
cores_many = "%{count} ядер"
mebibytes = "%{count} МиБ"
gibibytes = "%{count} ГиБ"

[agent_status]
offline = "Не в сети"
online = "В сети"

[vm_state]
stopped = "Остановлена"
starting = "Запускается"
running = "Работает"
building_downloading = "Сборка: загрузка"
building_writing_disk = "Сборка: запись диска"
building_provisioning = "Сборка: подготовка"
building_registering = "Сборка: регистрация"
building_starting = "Сборка: запуск ВМ"
building_waiting = "Сборка: ожидание гостя"
with_percentage = "%{label} %{percent}%"

[gpu_state]
disabled = "Отключён"
waiting_for_guest = "Ожидание гостя"
assigned = "Назначен"
ready = "Готов"
degraded = "Ограничен"
failed = "Ошибка"

[display_state]
installing = "Устанавливается"
ready = "Готов"
degraded = "Ограничен"
unsupported = "Не поддерживается"

[desktop_profile]
headless = "Нет (без графики)"
gnome = "GNOME"

[gpu_mode]
none = "Нет"
mirror = "Зеркало"
unsupported = "Не поддерживается"

[network_mode]
nat = "NAT"
external = "Внешняя"
internal = "Внутренняя"
```

- [ ] **Step 5: Translate the call sites**

Replace the literals at these lines with their keys, leaving the English catalogue text identical to what stood there:

- `crates/ui/src/lib.rs:319` `"Linux workspaces on Windows"` -> `t!("app.subtitle")`
- `:327` `"Refresh"` -> `t!("app.refresh")`; `:331` `"Available when the backend is ready"` -> `t!("app.refresh_hint")`
- `:342` `"Create VM"` -> `t!("app.create_vm")`; `:347` -> `t!("app.create_vm_hint")`
- `:357` `"Settings"` -> `t!("app.settings")`; `:358` -> `t!("app.settings_hint")`
- `:1562-1566` in `render_backend_status` -> `t!("app.backend_starting")`, `t!("app.backend_ready")`, `t!("app.backend_unavailable", message = message)`
- `:1572-1615` in `render_vm_list` -> `t!("vm_table.title")`, `t!("vm_table.empty")` and the column heads; the three figures become `cores_label(cores)`, `t!("vm_table.mebibytes", count = ram_mb)`, `t!("vm_table.gibibytes", count = disk_gb)`
- `:1457-1525`: `gpu_state_label`, `display_state_label`, `desktop_profile_label`, `gpu_mode_label` and `network_mode_label` return `String` and read `[gpu_state]`, `[display_state]`, `[desktop_profile]`, `[gpu_mode]`, `[network_mode]`
- `:2291-2306`: `agent_status_label` returns `String` from `[agent_status]`
- `:2398-2412`: `vm_state` returns `String` from `[vm_state]`; `vm_state_label` at `:2320` composes the percentage with `t!("vm_state.with_percentage", label = label, percent = percent)`

- [ ] **Step 6: Run the suite**

Run: `cargo test-windows -p vmlord-ui`
Expected: PASS, `each_state_gets_its_own_label` (`:2658`) included -- it asserts the English strings, which the catalogue reproduces exactly.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/locales crates/ui/src/lib.rs
git commit -m "TASK-18: Read the workspace list from the catalogue"
```

---

### Task 4: The selected VM panel

**Files:**
- Modify: `crates/ui/locales/en-US.toml`, `crates/ui/locales/ru-RU.toml`
- Modify: `crates/ui/src/lib.rs:1356-1394` (`gpu_status_detail`), `:1430-1455` (`gpu_mode_locked`, `ssh_port_locked`), `:1487-1496` (`display_status_detail`), `:1645-1800` (`ssh_offer`, `ssh_detail`, `connect_offer`, `update_display_offer`, `display_payload_detail`), `:1801-1992` (`render_selected_vm`, `render_action_group`), `:2225-2242` (`render_build_progress`), `:2337-2384` (`build_detail`)

**Interfaces:**
- Consumes: `t!`, `[common]` from Task 2, `cores_label` from Task 3.
- Produces: nothing later tasks read.

- [ ] **Step 1: Extend the catalogues**

Append to `crates/ui/locales/en-US.toml`:

```toml
[selected_vm]
title = "Selected VM: %{name}"
ip_address = "IP address"
operating_system = "Operating system"
gpu_status = "GPU status"
desktop_status = "Desktop status"
display_payload = "Display payload"
progress = "Progress"
percent = "%{percent}%"
locked_reason = "%{label}: %{reason}"
adapter = " Adapter: %{adapter}."
render_node = " Render node: %{node}."
desktop_reinstallable = " The desktop can be installed again."
not_reported = "Not reported"
payload_updating = "%{running} (updating to %{available})"
payload_offered = "%{running} (this release offers %{available})"
payload_none_yet = "Not reported; this release offers %{available}"

[actions]
start = "Start"
stop = "Stop"
force_stop = "Force stop"
connect = "Connect"
update_display = "Update display"
open_com_port = "Open COM port"
cancel_creation = "Cancel creation"
edit = "Edit"
open_ssh = "Open SSH"
open_in_windows_terminal = "Open in Windows Terminal"
after_build = "Available when the VM has finished building"
while_running = "Available when the VM is running"
only_while_running = "Available only when the VM is running"
only_while_stopped = "Available only when the VM is stopped"
only_while_building = "Available only while the VM is being created"
restart_needed = "Changes to a running VM apply after a restart"
after_start = "Available once the VM has been built and started"
needs_address = "Available when the guest has an address on the VMLord network"
gpu_mode_locked = "Stop the VM to change its GPU mode."
ssh_port_locked = "Start the VM to change its SSH port: the change is made inside the guest."
display_not_reported = "The display of this VM has not been reported yet"
display_update_offer = "Moves the guest from display payload %{running} to %{available}"
display_up_to_date = "The guest runs display payload %{running}, and this release carries nothing else"

[ssh]
endpoint = "%{user}@%{host}:%{port} (%{login} login)"
endpoint_pending = "%{user} on port %{port} (%{login} login); the address appears when the VM is running"
no_password = "no password"
running_condition = "the VM is running"

[build]
connecting = "Connecting to the image server"
downloaded_of = "Downloaded %{done} of %{total} (%{percent}%)"
downloaded = "Downloaded %{done}"
checking = "Checking the image: %{done} of %{total} (%{percent}%)"
image_ready = "Image ready"
```

Append to `crates/ui/locales/ru-RU.toml`:

```toml
[selected_vm]
title = "Выбранная ВМ: %{name}"
ip_address = "IP-адрес"
operating_system = "Операционная система"
gpu_status = "Состояние GPU"
desktop_status = "Состояние рабочего стола"
display_payload = "Пакет отображения"
progress = "Прогресс"
percent = "%{percent}%"
locked_reason = "%{label}: %{reason}"
adapter = " Адаптер: %{adapter}."
render_node = " Узел рендеринга: %{node}."
desktop_reinstallable = " Рабочий стол можно установить снова."
not_reported = "Нет данных"
payload_updating = "%{running} (обновляется до %{available})"
payload_offered = "%{running} (в этом выпуске есть %{available})"
payload_none_yet = "Нет данных; в этом выпуске есть %{available}"

[actions]
start = "Запустить"
stop = "Остановить"
force_stop = "Выключить принудительно"
connect = "Подключиться"
update_display = "Обновить отображение"
open_com_port = "Открыть COM-порт"
cancel_creation = "Отменить создание"
edit = "Изменить"
open_ssh = "Открыть SSH"
open_in_windows_terminal = "Открыть в Windows Terminal"
after_build = "Доступно после того, как ВМ собрана"
while_running = "Доступно, когда ВМ работает"
only_while_running = "Доступно только во время работы ВМ"
only_while_stopped = "Доступно только когда ВМ остановлена"
only_while_building = "Доступно только во время создания ВМ"
restart_needed = "Изменения работающей ВМ применяются после перезапуска"
after_start = "Доступно после того, как ВМ собрана и запущена"
needs_address = "Доступно, когда у гостя есть адрес в сети VMLord"
gpu_mode_locked = "Остановите ВМ, чтобы изменить режим GPU."
ssh_port_locked = "Запустите ВМ, чтобы изменить порт SSH: изменение вносится внутри гостя."
display_not_reported = "Отображение этой ВМ ещё не сообщило о себе"
display_update_offer = "Переводит гостя с пакета отображения %{running} на %{available}"
display_up_to_date = "Гость работает с пакетом отображения %{running}, другого в этом выпуске нет"

[ssh]
endpoint = "%{user}@%{host}:%{port} (вход: %{login})"
endpoint_pending = "%{user} на порту %{port} (вход: %{login}); адрес появится, когда ВМ заработает"
no_password = "без пароля"
running_condition = "ВМ работает"

[build]
connecting = "Соединение с сервером образов"
downloaded_of = "Загружено %{done} из %{total} (%{percent}%)"
downloaded = "Загружено %{done}"
checking = "Проверка образа: %{done} из %{total} (%{percent}%)"
image_ready = "Образ готов"
```

- [ ] **Step 2: Translate the call sites**

Replace each literal listed in the catalogue above at its line: `:1815` `format!("Selected VM: {}", name)` becomes `t!("selected_vm.title", name = name)`; `:1818-1907` the action names and their hover texts read `[actions]`; `:1920-1957` the detail rows read `[selected_vm]`; `:1978` `format!("{label}: {reason}")` becomes `t!("selected_vm.locked_reason", label = label, reason = reason)`; `:1369` and `:1376` read `selected_vm.adapter` and `selected_vm.render_node`; `:1493` reads `selected_vm.desktop_reinstallable`; `:1784-1796` read the `payload_*` keys and `selected_vm.not_reported`; `:1695-1707` read `[ssh]`; `:1720`, `:1741`, `:1766`, `:1771` read `[actions]`; `:1433` and `:1453` return `Some(t!("actions.gpu_mode_locked").to_string())` and `Some(t!("actions.ssh_port_locked").to_string())`, so `gpu_mode_locked` and `ssh_port_locked` return `Option<String>`; `:2230-2236` read `selected_vm.progress` and `selected_vm.percent`; `:2359-2381` read `[build]`.

`"Unknown"` at `:1360` and `"Unavailable"` at `:1922` read `common.unknown` and `common.unavailable` from Task 2's table rather than gaining keys of their own.

`const SSH_ACTION_LABEL: &str = "Open SSH"` at `:1628` cannot stand: a `const` cannot call `t!`. Replace it with

```rust
fn ssh_action_label() -> String {
    t!("actions.open_ssh").to_string()
}
```

and call it at each of the const's two use sites, `:1625` included, where the Windows Terminal variant reads `t!("actions.open_in_windows_terminal")`.

`t!` returns a `Cow<str>`; add `.to_string()` where a `String` or a `WidgetText` is wanted.

- [ ] **Step 3: Run the suite**

Run: `cargo test-windows -p vmlord-ui`
Expected: PASS. `the_details_state_the_endpoint_a_session_would_use` (`:3145`), `the_action_is_named_after_what_it_opens` (`:3181`), `a_display_that_is_not_ready_says_what_it_is_waiting_for` (`:2486`), `an_unreported_payload_says_what_the_display_is_waiting_for` (`:2580`), `the_details_state_both_payload_versions` (`:2616`), `a_build_shows_its_download_and_says_nothing_when_there_is_nothing_to_say` (`:3207`) and `the_steps_after_registering_have_labels_of_their_own` (`:2636`) all assert English text that the catalogue reproduces exactly.

- [ ] **Step 4: Add a Russian assertion**

```rust
#[test]
fn the_actions_are_translated() {
    assert_eq!(t!("actions.start", locale = "ru-RU"), "Запустить");
    assert_eq!(
        t!("ssh.endpoint", locale = "ru-RU", user = "ubuntu", host = "10.0.0.2", port = 22, login = "по ключу"),
        "ubuntu@10.0.0.2:22 (вход: по ключу)"
    );
}
```

Run: `cargo test-windows -p vmlord-ui translated`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/locales crates/ui/src/lib.rs
git commit -m "TASK-18: Read the VM panel from the catalogue"
```

---

### Task 5: The create-VM dialog

**Files:**
- Modify: `crates/ui/locales/en-US.toml`, `crates/ui/locales/ru-RU.toml`
- Modify: `crates/ui/src/lib.rs:590-827` (`render_create_vm_dialog`), `:828-927` (`render_provisioning_fields`), `:1232-1302` (`create_vm_request`, `create_vm_source`, `release_label`), `:1396-1428` (`gpu_capability_warnings`), `:1473-1478` (`create_vm_advisories`)

**Interfaces:**
- Consumes: `t!`, `[common]`, `[gpu_mode]`, `[network_mode]`, `[desktop_profile]` from Tasks 2 and 3.

- [ ] **Step 1: Extend the catalogues**

Append to `crates/ui/locales/en-US.toml`:

```toml
[create_vm]
title = "New Linux VM"
description = "Create a persistent Linux workspace."
system = "System"
cloud_image = "Cloud image (ready to use)"
own_iso = "Own ISO (installed by hand)"
distribution = "Distribution"
release = "Release"
release_label = "%{distribution} %{release} LTS"
os_image = "OS Image"
os_image_hint = "Path to ISO or VHDX..."
vm_name = "VM Name"
hdd_size = "HDD Size"
ram_size = "RAM Size"
cpu_cores = "CPU Cores"
gpu = "GPU"
desktop = "Desktop"
network = "Network"
guest = "Guest"
user_name = "User name"
password = "Password"
password_optional = "Optional"
ssh = "SSH"
ssh_server = "Run an SSH server in the guest"
ssh_key = "Generate a key pair for this VM and install the public half"
port = "Port"
private_key = "Private key: %{path}"
private_key_note = "The private key is stored in the VM's own folder."
locale = "Locale"
keyboard = "Keyboard layout"
timezone = "Timezone"
guest_defaults_note = "The three settings above are filled in from this computer and applied to the guest."
name_taken = "A VM with this name already exists."
no_gpu_adapter = "This host presents no GPU partition adapter, so the VM will start without a GPU."
```

Append to `crates/ui/locales/ru-RU.toml`:

```toml
[create_vm]
title = "Новая Linux-ВМ"
description = "Создать постоянное Linux-окружение."
system = "Система"
cloud_image = "Облачный образ (готов к работе)"
own_iso = "Свой ISO (установка вручную)"
distribution = "Дистрибутив"
release = "Выпуск"
release_label = "%{distribution} %{release} LTS"
os_image = "Образ ОС"
os_image_hint = "Путь к ISO или VHDX..."
vm_name = "Имя ВМ"
hdd_size = "Размер диска"
ram_size = "Объём RAM"
cpu_cores = "Ядра CPU"
gpu = "GPU"
desktop = "Рабочий стол"
network = "Сеть"
guest = "Гость"
user_name = "Имя пользователя"
password = "Пароль"
password_optional = "Необязательно"
ssh = "SSH"
ssh_server = "Запустить SSH-сервер в госте"
ssh_key = "Создать пару ключей для этой ВМ и установить открытую половину"
port = "Порт"
private_key = "Закрытый ключ: %{path}"
private_key_note = "Закрытый ключ хранится в папке самой ВМ."
locale = "Локаль"
keyboard = "Раскладка клавиатуры"
timezone = "Часовой пояс"
guest_defaults_note = "Три настройки выше взяты с этого компьютера и применяются к гостю."
name_taken = "ВМ с таким именем уже существует."
no_gpu_adapter = "На этом хосте нет адаптера с разделением GPU, поэтому ВМ запустится без GPU."
```

- [ ] **Step 2: Translate the call sites**

Every literal named above, at `:599-925` and `:1241-1300`, becomes its key. `release_label` (`:1299`) builds `t!("create_vm.release_label", distribution = distribution, release = release)`. The `"Cancel"` button at `:814` reads `t!("common.cancel")`, `"Browse..."` at `:685` reads `t!("common.browse")`, `"Default"`, `"Mirror"` and `"None"` in the GPU combo read `gpu_mode_label`, and the network combo reads `network_mode_label`. `"MiB"` and `"GiB"` suffixes at `:696-705` stay as they are: both languages spell them the same when they stand alone as a unit.

`gpu_capability_warnings` (`:1396`) returns its text from `t!("create_vm.no_gpu_adapter")` and from the GPU availability message it is handed, which comes from `core` and stays English.

- [ ] **Step 3: Run the suite**

Run: `cargo test-windows -p vmlord-ui`
Expected: PASS, including `a_name_already_in_the_list_is_refused_before_the_backend_sees_it` (`:3189`), `a_small_desktop_vm_is_advised_against_rather_than_refused` (`:2799`), `the_domains_own_words_are_what_the_form_shows` (`:2925`) and the four GPU warning tests at `:3419-3471`.

- [ ] **Step 4: Commit**

```bash
git add crates/ui/locales crates/ui/src/lib.rs
git commit -m "TASK-18: Read the create dialog from the catalogue"
```

---

### Task 6: The edit and delete dialogs

**Files:**
- Modify: `crates/ui/locales/en-US.toml`, `crates/ui/locales/ru-RU.toml`
- Modify: `crates/ui/src/lib.rs:1030-1174` (`render_edit_vm_dialog`), `:1176-1230` (`render_delete_vm_dialog`), `:1303-1333` (`edit_vm_request`)

**Interfaces:**
- Consumes: `t!`, `[common]` from Task 2, `[gpu_mode]` and `[network_mode]` from Task 3.

- [ ] **Step 1: Extend the catalogues**

Append to `crates/ui/locales/en-US.toml`:

```toml
[edit_vm]
title = "Edit VM: %{name}"
description = "Changes are saved to the VM configuration and take effect the next time the VM starts."
ssh_port = "SSH Port"
save = "Save changes"
ram_invalid = "RAM must be an even number of MiB and at least 512 MiB."
cores_invalid = "CPU cores must be at least 1."
gpu_mode_unsupported = "The current GPU mode is not supported by the Rust UI yet."
network_mode_unsupported = "The current network mode is not supported by the Rust UI yet."

[delete_vm]
title = "Delete VM: %{name}"
description = "VM \"%{name}\" and its stored configuration will be removed. This cannot be undone."
delete_disks = "Delete virtual disks"
disks_deleted = "The VM's virtual disks are deleted with it. The image it was installed from is not touched."
disks_kept = "The virtual disks are kept, so the VM's directory stays in place and a new VM cannot reuse that name."
confirm = "Delete"
```

Append to `crates/ui/locales/ru-RU.toml`:

```toml
[edit_vm]
title = "Изменить ВМ: %{name}"
description = "Изменения сохраняются в конфигурации ВМ и вступают в силу при следующем запуске."
ssh_port = "Порт SSH"
save = "Сохранить изменения"
ram_invalid = "Объём RAM должен быть чётным числом МиБ и не меньше 512 МиБ."
cores_invalid = "Ядер CPU должно быть не меньше одного."
gpu_mode_unsupported = "Текущий режим GPU пока не поддерживается интерфейсом на Rust."
network_mode_unsupported = "Текущий режим сети пока не поддерживается интерфейсом на Rust."

[delete_vm]
title = "Удалить ВМ: %{name}"
description = "ВМ «%{name}» и её сохранённая конфигурация будут удалены. Это необратимо."
delete_disks = "Удалить виртуальные диски"
disks_deleted = "Виртуальные диски ВМ удаляются вместе с ней. Образ, с которого она устанавливалась, не затрагивается."
disks_kept = "Виртуальные диски сохраняются, поэтому папка ВМ остаётся на месте и новая ВМ не сможет занять это имя."
confirm = "Удалить"
```

- [ ] **Step 2: Translate the call sites**

`:1037` becomes `t!("edit_vm.title", name = name)`, `:1043` `t!("edit_vm.description")`, `:1118` `t!("edit_vm.ssh_port")`, `:1155` `t!("edit_vm.save")`, and the `"Cancel"` beside it `t!("common.cancel")`. The three combo boxes read `gpu_mode_label` and `network_mode_label` from Task 3. `edit_vm_request` (`:1303`) returns its four messages from `[edit_vm]`.

`:1182` becomes `t!("delete_vm.title", name = name)`, `:1189` `t!("delete_vm.description", name = name)`, `:1193-1197` the three `delete_vm` keys, `:1208` `t!("delete_vm.confirm")`.

- [ ] **Step 3: Run the suite**

Run: `cargo test-windows -p vmlord-ui`
Expected: PASS, including `edit_vm_request_rejects_odd_ram` (`:3387`), `edit_vm_request_rejects_a_port_nothing_can_connect_to` (`:3328`), `a_stopped_vm_says_why_its_ssh_port_cannot_be_edited` (`:3355`) and `a_password_vm_says_why_its_ssh_port_cannot_be_edited` (`:3374`).

- [ ] **Step 4: Commit**

```bash
git add crates/ui/locales crates/ui/src/lib.rs
git commit -m "TASK-18: Read the edit and delete dialogs from the catalogue"
```

---

### Task 7: The diagnostics panel, and the documentation

**Files:**
- Modify: `crates/ui/locales/en-US.toml`, `crates/ui/locales/ru-RU.toml`
- Modify: `crates/ui/src/lib.rs:2270-2287` (`render_diagnostics`)
- Modify: `ARCHITECTURE.md`, `AGENTS.md`

**Interfaces:**
- Consumes: `t!` from Task 2.

- [ ] **Step 1: Extend the catalogues**

Append to `crates/ui/locales/en-US.toml`:

```toml
[diagnostics]
title = "Log"
```

Append to `crates/ui/locales/ru-RU.toml`:

```toml
[diagnostics]
title = "Журнал"
```

- [ ] **Step 2: Translate the heading**

`crates/ui/src/lib.rs:2271` reads `t!("diagnostics.title")`. `diagnostic_line` (`:2255`) is left as it stands: the message inside a record comes from `vmlord_core::diagnostic!` and stays English, and so do the ` ({vm})` and ` [0x{code:08X}]` it is decorated with, which are an identifier and a status code.

- [ ] **Step 3: Verify no user-facing literal is left**

Run:

```bash
grep -n '"[A-Z][a-z]' crates/ui/src/lib.rs | sed -n '1,80p'
```

Expected: only widget id salts, `"VMLord"`, the language names `"English (US)"` and `"Русский"`, unit suffixes, and strings inside the test module. Anything else is a missed literal -- move it into both catalogues before continuing.

- [ ] **Step 4: Write the ARCHITECTURE.md section**

Add a section titled "Localization" saying: the catalogues are `crates/ui/locales/en-US.toml` and `crates/ui/locales/ru-RU.toml`, embedded by `rust-i18n` at compile time, so the shipped executable still needs nothing beside it; `Language::code` is the one place a locale tag is spelled, and both `settings.toml` and the i18n backend read it; `run` sets the locale from the settings and the settings dialog sets it again on save, which is enough because egui rebuilds each frame from the catalogue; translation stops at the UI boundary -- a `diagnostic!` record and the `Display` of a `core` error stay English, because a diagnostic is written to the log file in the same breath as it reaches the panel, and a log is more useful in one language than in the reader's; a fresh installation starts in English, since the language is chosen at install time; the counted noun in the workspace list goes through `plural_form`, and why one function is the right size of answer for one string.

- [ ] **Step 5: Add the AGENTS.md line**

Under "UI Rules":

```markdown
* New user-facing text in the UI goes through `t!` and is added to both
  catalogues under `crates/ui/locales/`.
```

- [ ] **Step 6: Run the whole suite**

Run: `cargo test-windows`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/locales crates/ui/src/lib.rs ARCHITECTURE.md AGENTS.md
git commit -m "TASK-18: Record how the UI chooses its language"
```
