# UI localization design

## Goal

Task #18 gives VMLord a second interface language. The desktop shell reads its
text from message catalogues instead of literals, ships an `ru-RU` catalogue
beside the `en-US` one, and the settings dialog switches between them while the
application runs.

## Scope

Text in VMLord lives in three layers: the labels and buttons of `vmlord-ui`,
the diagnostics raised through `vmlord_core::diagnostic!`, and the `Display`
implementations of the error types in `vmlord-core` that the UI renders through
`error.to_string()`.

This task translates the first layer alone.

The other two stay in English. Translating them would mean an error no longer
carries its own text: `core` would have to return a code with parameters and
the UI would have to render it, which is a rewrite of `error.rs` and of all 66
`diagnostic!` call sites. A diagnostic is also written to the log file at the
same moment it reaches the panel, and a log read by a developer is more useful
in one language than in the user's.

The language is not guessed from the Windows locale. `Language::EnUs` stays
`#[default]`, so a fresh installation starts in English; picking a language at
install time is a separate concern from picking one at run time.

## The catalogues

`crates/ui/locales/en-US.toml` and `crates/ui/locales/ru-RU.toml` hold the
text, and `crates/ui/src/lib.rs` opens with

```rust
use rust_i18n::t;

rust_i18n::i18n!("locales", fallback = "en-US");
```

`rust-i18n = "3"` is a dependency of `vmlord-ui` alone.
The macro reads both files at compile time and embeds them, so the shipped
`vmlord.exe` still needs nothing beside it. Nothing outside `vmlord-ui`
depends on `rust-i18n`: `core`, `app` and `platform` do not know that the
application has languages, which is the "UI rules" boundary of **AGENTS.md**
observed in the direction that is easy to forget.

TOML rather than YAML -- `rust-i18n` accepts either. The project already speaks
TOML in `settings.toml`, `vmlord-core` already depends on `toml 0.8`, and the
catalogue parity test below reads the two files back with that same parser
rather than a second one.

Keys are namespaced by the screen that shows them: `common.*` for what every
dialog repeats (Cancel, Save, `Browse...`, Yes, No, Unknown), then `app.*`,
`vm_table.*`, `create_vm.*`, `edit_vm.*`, `delete_vm.*`, `settings.*` and
`diagnostics.*`. The last one covers the panel's own chrome -- its heading,
its level filter, its clear button -- and not the messages inside it, which are
what the scope leaves in English.

A string that interpolates moves to a named placeholder rather than keeping its
positional `format!`: `format!("Edit VM: {name}")` becomes
`t!("edit_vm.title", name = name)`. Russian puts the parts of that sentence in
a different order, and a named placeholder is what lets the catalogue say so.

The validation messages of `SettingsForm::settings` ("VM storage path is
required.") are UI text and are translated with the rest. The text of an error
that arrived from `core` is not.

## Choosing the language

`Language` in `crates/core/src/settings.rs` gains

```rust
#[serde(rename = "ru-RU")]
RuRu,
```

and a `code(self) -> &'static str` returning the locale tag, which is the only
thing the UI needs to drive `rust-i18n` and the only place the tag is spelled.

`vmlord_ui::run` calls `rust_i18n::set_locale` from `application.settings()`
before it opens the window. Settings that failed to load leave the locale at
`en-US`, which is where the fallback already points.

The settings dialog is where the language changes. The `Submit` arm of
`SettingsDialogAction` already hands the rebuilt `AppSettings` to
`WorkspaceApp::update_settings`; on success it now also calls `set_locale`. egui redraws the
whole frame from the catalogue on every pass, so the interface changes language
under the Save button with no restart and no reload.

The combo box gains its second entry, and `language_label` returns "English
(US)" and "Русский". Those two are not translated: a user who cannot read the
language currently on screen has to be able to find their own in that list.

## Plural forms

Russian inflects a counted noun three ways -- 1 ядро, 2 ядра, 5 ядер -- and
`rust-i18n` carries no plural rules. Only one string in the UI counts an
inflected noun: `"{} cores"`, at `crates/ui/src/lib.rs:1613` and `:1939`. MiB
and GiB do not inflect, and the disk and memory figures are the only other
numbers on screen.

So a function, not a rule engine: `cores_label(count)` picks between
`common.cores_one`, `common.cores_few` and `common.cores_many` by the Russian
rule. English is served by the same three keys because the English rule -- one
against everything else -- is a coarsening of the Russian one, so `en-US.toml`
repeats itself in the last two.

## Tests

`Language` serializes to `"ru-RU"` and loads back, tested in `settings.rs`
beside the settings tests already there.

The catalogues agree on their keys. A test in `vmlord-ui` parses both TOML
files with `toml` -- a dev-dependency, the same version `vmlord-core` uses --
and asserts each file's key set contains the other's. A translation forgotten
in a pull request fails the build instead of quietly falling back to English.

`cores_label` returns the right form for 1, 2, 5, 11 and 21 under `ru-RU`.

The suite runs through `cargo test-windows`.

## Documentation

**ARCHITECTURE.md** gains a localization section: where the catalogues live,
why translation stops at the UI boundary, and why the log file stays English.

**AGENTS.md** gains one line under "UI Rules": new user-facing text in the UI
is written through `t!` and added to both catalogues.
