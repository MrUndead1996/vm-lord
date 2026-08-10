# Cloud-config Generation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Собрать из контракта провизионинга два документа NoCloud — `user-data` и `meta-data`, — которые cloud-init прочитает внутри гостя.

**Architecture:** Новый крейт `crates/seed` (`vmlord-seed`) с плоским входом `SeedRequest` и инфалибельной `build(&SeedRequest) -> Seed`. Plaintext-пароль в крейт не приходит: только `$6$`-хеш (#56) и публичный ключ (#55). YAML печатается вручную по фиксированному шаблону, каждый пришедший извне скаляр — одинарно-кавычным скаляром; содержимое `/etc/default/keyboard` дополнительно экранируется по правилам шелла. `DistroProfile` получает поле `ssh_units`, чтобы выключение SSH-демона было данными профиля, а не литералом Ubuntu в генераторе.

**Tech Stack:** Rust 2024, workspace `vmlord`, `log`; в dev-зависимостях крейта `seed` — `serde_yaml_ng` 0.10.

**Спека:** `docs/superpowers/specs/2026-08-10-cloud-config-generation-design.md`

## Global Constraints

- Ветка задачи: `task-58-cloud-config` (уже создана, спека закоммичена).
- Префикс каждого коммита — `TASK-58: `, сообщение на английском, в конце `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
- Комментарии и документация в коде — на английском, как во всём репозитории; объясняют «почему», а не «что».
- Логирование только через фасад `log`, уровни DEBUG..ERROR, `TRACE` не используется.
- `unsafe` в `core` и `seed` запрещён (`unsafe_code = "deny"` в workspace-lints); `crates/seed` наследует `[lints] workspace = true`.
- Единственная новая зависимость во всём плане — `serde_yaml_ng = "0.10"` в `[dev-dependencies]` крейта `seed`. В продакшн-зависимостях новых крейтов нет.
- Секреты: в `crates/seed` не приходит ни plaintext-пароль, ни приватный ключ. `Seed` не реализует `Debug`.
- Кросс-проверка после каждой задачи: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets` (единственный способ собрать Windows-only крейты на WSL).
- Хостовые тесты после каждой задачи: `cargo test -p vmlord-core -p vmlord-seed`.
- `crates/platform`, `crates/ui`, `crates/legacy-backend`, `crates/vmlord` на WSL не запускаются — для них проверка это `cargo check` выше.

---

### Task 1: Профиль называет юниты SSH-демона

`DistroProfile` уже несёт `admin_group`, чтобы генератор не знал про различие `sudo`/`wheel`. Имя юнита SSH-демона — различие того же сорта: Debian-семейство держит `ssh.socket`/`ssh.service`, Fedora и SUSE — `sshd.service`. Поле добавляется здесь, чтобы Task 5 мог его напечатать.

**Files:**
- Modify: `crates/core/src/distro.rs:24-55` (поле структуры и значение в `ubuntu()`)
- Test: тесты внутри `crates/core/src/distro.rs`

**Interfaces:**
- Consumes: ничего.
- Produces: `DistroProfile.ssh_units: Vec<String>`; `ubuntu().ssh_units == ["ssh.socket", "ssh.service"]`.

- [ ] **Step 1: Написать падающий тест**

В конец модуля `mod tests` в `crates/core/src/distro.rs`:

```rust
    #[test]
    fn a_profile_names_the_units_that_carry_its_ssh_daemon() {
        assert_eq!(ubuntu().ssh_units, ["ssh.socket", "ssh.service"]);
    }
```

- [ ] **Step 2: Убедиться, что тест падает**

Run: `cargo test -p vmlord-core ssh_daemon`
Expected: FAIL — `no field 'ssh_units' on type 'DistroProfile'`.

- [ ] **Step 3: Добавить поле**

В `crates/core/src/distro.rs`, после `admin_group` в объявлении структуры:

```rust
    /// The systemd units that carry the SSH daemon, most specific first.
    ///
    /// Data rather than a literal in the seed generator: Debian-family systems
    /// socket-activate `ssh.socket` and keep `ssh.service` beside it, while
    /// Fedora and SUSE name both `sshd`. A VM created with SSH turned off has
    /// these disabled on the first boot.
    pub ssh_units: Vec<String>,
```

И в `ubuntu()`, после `admin_group`:

```rust
        ssh_units: vec!["ssh.socket".into(), "ssh.service".into()],
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-core -p vmlord-image`
Expected: PASS. Литералы `DistroProfile` в репозитории пишутся через `..ubuntu()` (`crates/core/src/distro.rs:112`, `crates/image/tests/resolve.rs:19`), поэтому новое поле их не ломает.

- [ ] **Step 5: Кросс-проверка**

Run: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets`
Expected: без ошибок.

- [ ] **Step 6: Коммит**

```bash
git add crates/core/src/distro.rs
git commit -m "$(printf 'TASK-58: Name the SSH units in the distribution profile\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>')"
```

---

### Task 2: Крейт `seed` и экранирование скаляров

Два читателя с разными правилами: YAML читает документ, шелл читает `/etc/default/keyboard` через `source`. Экранирование — отдельный модуль, потому что оба правила проверяются без единой строчки cloud-config.

**Files:**
- Create: `crates/seed/Cargo.toml`
- Create: `crates/seed/src/lib.rs`
- Create: `crates/seed/src/scalar.rs`
- Modify: `Cargo.toml:2-10` (список членов workspace)
- Test: тесты внутри `crates/seed/src/scalar.rs`

**Interfaces:**
- Consumes: ничего.
- Produces: крейт `vmlord-seed`; `pub(crate) fn scalar::yaml(value: &str) -> String`, `pub(crate) fn scalar::shell(value: &str) -> String`.

- [ ] **Step 1: Завести крейт**

`crates/seed/Cargo.toml`:

```toml
[package]
name = "vmlord-seed"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
log.workspace = true
vmlord-core = { path = "../core" }

[dev-dependencies]
# Production prints the document by hand; the tests read it back the way
# cloud-init's PyYAML does, so they assert on meaning rather than on bytes.
serde_yaml_ng = "0.10"

[lints]
workspace = true
```

В корневом `Cargo.toml` добавить `"crates/seed",` в `members`, сохраняя алфавитный порядок — после `"crates/platform"`.

`crates/seed/src/lib.rs` на этом шаге:

```rust
//! The NoCloud seed VMLord writes for cloud-init.

mod scalar;
```

`crates/seed/src/scalar.rs` — пустой файл.

- [ ] **Step 2: Написать падающие тесты**

`crates/seed/src/scalar.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{shell, yaml};

    #[test]
    fn a_plain_value_is_quoted() {
        assert_eq!(yaml("en_US.UTF-8"), "'en_US.UTF-8'");
    }

    /// A single-quoted YAML scalar has no escape sequences at all, so the one
    /// character that can end it is the only one that needs handling.
    #[test]
    fn an_apostrophe_is_doubled_rather_than_escaped() {
        assert_eq!(yaml("don't"), "'don''t'");
    }

    /// Every character that would otherwise close the scalar or introduce
    /// structure stays inside it.
    #[test]
    fn nothing_breaks_out_of_a_quoted_scalar() {
        for value in ["a: b", "- item", "#comment", "\"quoted\"", "{}", "[]", "*anchor"] {
            let quoted = yaml(value);
            assert!(quoted.starts_with('\''), "{quoted:?}");
            assert!(quoted.ends_with('\''), "{quoted:?}");
            assert_eq!(quoted.matches('\'').count(), 2, "{quoted:?}");
        }
    }

    #[test]
    fn a_plain_layout_passes_through_the_shell_untouched() {
        assert_eq!(shell("us"), "us");
    }

    /// `/etc/default/keyboard` is read with `source`, so a value is code until
    /// it is escaped: `$(...)` would run, and a quote would end the assignment.
    #[test]
    fn a_shell_value_cannot_run_a_command_or_end_its_assignment() {
        assert_eq!(shell("us$(id)"), "us\\$(id)");
        assert_eq!(shell("us\"; reboot #"), "us\\\"; reboot #");
        assert_eq!(shell("us`id`"), "us\\`id\\`");
        assert_eq!(shell("us\\"), "us\\\\");
    }
}
```

- [ ] **Step 3: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-seed`
Expected: FAIL — `cannot find function 'yaml' in this scope`.

- [ ] **Step 4: Написать реализацию**

В начало `crates/seed/src/scalar.rs`, перед `mod tests`:

```rust
//! Two readers, two sets of rules.
//!
//! The document is read as YAML, and `/etc/default/keyboard` inside it is later
//! read by shell scripts with `source`. A value safe for one is not thereby
//! safe for the other, so neither escaping stands in for the other.

/// Prints a value as a single-quoted YAML scalar.
///
/// Single quotes rather than double: inside them YAML has no escape sequences
/// whatsoever, so a value cannot mean anything but itself, and the only
/// character worth handling is the quote that ends the scalar.
pub(crate) fn yaml(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Escapes a value for a double-quoted shell assignment.
pub(crate) fn shell(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '"' | '`' | '$') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}
```

- [ ] **Step 5: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-seed`
Expected: PASS, 5 тестов.

- [ ] **Step 6: Кросс-проверка**

Run: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets`
Expected: без ошибок.

- [ ] **Step 7: Коммит**

```bash
git add Cargo.toml Cargo.lock crates/seed
git commit -m "$(printf 'TASK-58: Add the seed crate and its scalar escaping\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>')"
```

---

### Task 3: Вход генератора и `meta-data`

`meta-data` — два ключа, и он же вводит `SeedRequest`, который читают все последующие задачи.

**Files:**
- Modify: `crates/seed/src/lib.rs`
- Create: `crates/seed/src/meta_data.rs`
- Test: тесты внутри `crates/seed/src/meta_data.rs`

**Interfaces:**
- Consumes: `scalar::yaml` (Task 2); `vmlord_core::SshAccess`.
- Produces: `pub struct SeedRequest<'a>` с полями `vm_name: &'a str`, `instance_id: &'a str`, `username: &'a str`, `password_hash: Option<&'a str>`, `authorized_key: Option<&'a str>`, `ssh: SshAccess`, `locale: &'a str`, `keyboard: &'a str`, `timezone: &'a str`, `admin_group: &'a str`, `ssh_units: &'a [String]`; `pub(crate) fn meta_data::render(request: &SeedRequest<'_>) -> String`.

- [ ] **Step 1: Объявить вход**

`crates/seed/src/lib.rs` целиком:

```rust
//! The NoCloud seed VMLord writes for cloud-init: two documents, and the rules
//! for printing them.
//!
//! The request is flat rather than a borrowed `Provisioning` for one reason:
//! `Provisioning` carries the password in the clear, and this crate has no
//! business seeing it. What arrives here is the `$6$` hash and the public key,
//! both produced elsewhere, so "no plaintext password in the document" is a
//! property of the types rather than a lucky outcome checked afterwards.

mod meta_data;
mod scalar;

use vmlord_core::SshAccess;

/// Everything the two documents are printed from.
pub struct SeedRequest<'a> {
    /// Becomes `local-hostname`.
    pub vm_name: &'a str,
    /// Becomes `instance-id`. Formatted from the VM's `Uuid` by the caller,
    /// which keeps `uuid` out of this crate's dependencies.
    pub instance_id: &'a str,
    pub username: &'a str,
    /// The `$6$` SHA-512-crypt hash. `None` is a key-only login.
    pub password_hash: Option<&'a str>,
    /// The public key, in `authorized_keys` form.
    pub authorized_key: Option<&'a str>,
    pub ssh: SshAccess,
    pub locale: &'a str,
    pub keyboard: &'a str,
    pub timezone: &'a str,
    /// The group that grants administrative rights: `sudo` or `wheel`.
    pub admin_group: &'a str,
    /// The units that carry the SSH daemon, disabled when SSH is off.
    pub ssh_units: &'a [String],
}
```

- [ ] **Step 2: Написать падающий тест**

`crates/seed/src/meta_data.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::render;
    use crate::SeedRequest;
    use serde_yaml_ng::Value;
    use vmlord_core::SshAccess;

    fn request() -> SeedRequest<'static> {
        SeedRequest {
            vm_name: "my-vm",
            instance_id: "vmlord-4f1c0e5a",
            username: "dev",
            password_hash: Some("$6$rounds=4096$salt$hash"),
            authorized_key: Some("ssh-ed25519 AAAAC3Nz vmlord"),
            ssh: SshAccess::Enabled { deploy_key: true },
            locale: "en_US.UTF-8",
            keyboard: "us",
            timezone: "Europe/Moscow",
            admin_group: "sudo",
            ssh_units: &[],
        }
    }

    fn parsed(document: &str) -> Value {
        serde_yaml_ng::from_str(document).expect("cloud-init reads this with a YAML parser too")
    }

    #[test]
    fn meta_data_carries_the_instance_id_and_the_hostname() {
        let document = parsed(&render(&request()));

        assert_eq!(document["instance-id"], Value::from("vmlord-4f1c0e5a"));
        assert_eq!(document["local-hostname"], Value::from("my-vm"));
    }

    /// The identifier is a UUID today, but it is printed as a scalar rather
    /// than trusted to be one: a value that ended the scalar would move the
    /// hostname into the identifier.
    #[test]
    fn a_hostile_name_stays_inside_its_scalar() {
        let document = parsed(&render(&SeedRequest {
            vm_name: "vm'\nruncmd: ['reboot']",
            ..request()
        }));

        assert_eq!(document["local-hostname"], Value::from("vm'\nruncmd: ['reboot']"));
        assert_eq!(document.as_mapping().expect("a mapping").len(), 2);
    }
}
```

- [ ] **Step 3: Убедиться, что тест падает**

Run: `cargo test -p vmlord-seed meta_data`
Expected: FAIL — `cannot find function 'render' in this scope`.

- [ ] **Step 4: Написать реализацию**

В начало `crates/seed/src/meta_data.rs`, перед `mod tests`:

```rust
//! `meta-data`: who this instance is, and what it calls itself.

use crate::{SeedRequest, scalar};

/// Prints the document.
///
/// `instance-id` comes from the VM's identifier and never changes: the seed
/// stays attached for the life of the VM, and cloud-init reads it on every
/// boot, so a changing identifier would re-run the per-instance modules and
/// re-create the user on each start.
pub(crate) fn render(request: &SeedRequest<'_>) -> String {
    format!(
        "instance-id: {}\nlocal-hostname: {}\n",
        scalar::yaml(request.instance_id),
        scalar::yaml(request.vm_name),
    )
}
```

В `crates/seed/src/lib.rs` модуль `meta_data` уже объявлен на Step 1.

- [ ] **Step 5: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-seed`
Expected: PASS, 7 тестов.

- [ ] **Step 6: Кросс-проверка**

Run: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets`
Expected: без ошибок.

- [ ] **Step 7: Коммит**

```bash
git add crates/seed
git commit -m "$(printf 'TASK-58: Render the NoCloud meta-data document\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>')"
```

---

### Task 4: `user-data` для пользователя с паролем и ключом

Полный документ для самого населённого случая: пароль задан, ключ развёрнут, SSH включён. Варианты входа — следующая задача.

**Files:**
- Modify: `crates/seed/src/lib.rs` (объявление модуля `user_data`)
- Create: `crates/seed/src/user_data.rs`
- Test: тесты внутри `crates/seed/src/user_data.rs`

**Interfaces:**
- Consumes: `SeedRequest` (Task 3), `scalar::yaml`, `scalar::shell` (Task 2).
- Produces: `pub(crate) fn user_data::render(request: &SeedRequest<'_>) -> String`.

- [ ] **Step 1: Написать падающие тесты**

`crates/seed/src/user_data.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::render;
    use crate::SeedRequest;
    use serde_yaml_ng::Value;
    use vmlord_core::SshAccess;

    const HASH: &str = "$6$rounds=4096$salt$hash";
    const KEY: &str = "ssh-ed25519 AAAAC3Nz vmlord";

    fn request() -> SeedRequest<'static> {
        SeedRequest {
            vm_name: "my-vm",
            instance_id: "vmlord-4f1c0e5a",
            username: "dev",
            password_hash: Some(HASH),
            authorized_key: Some(KEY),
            ssh: SshAccess::Enabled { deploy_key: true },
            locale: "en_US.UTF-8",
            keyboard: "us",
            timezone: "Europe/Moscow",
            admin_group: "sudo",
            ssh_units: &[],
        }
    }

    fn parsed(document: &str) -> Value {
        serde_yaml_ng::from_str(document).expect("cloud-init reads this with a YAML parser too")
    }

    /// cloud-init recognises the format by this line, and a YAML parser reads
    /// it as a comment -- so nothing but a byte comparison can check it.
    #[test]
    fn the_first_line_is_the_marker_cloud_init_looks_for() {
        assert!(render(&request()).starts_with("#cloud-config\n"));
    }

    #[test]
    fn the_user_is_created_with_a_password_a_key_and_administrative_rights() {
        let document = parsed(&render(&request()));
        let user = &document["users"][0];

        assert_eq!(user["name"], Value::from("dev"));
        assert_eq!(user["shell"], Value::from("/bin/bash"));
        assert_eq!(user["groups"], Value::from(vec!["sudo"]));
        assert_eq!(user["sudo"], Value::from("ALL=(ALL) NOPASSWD:ALL"));
        assert_eq!(user["hashed_passwd"], Value::from(HASH));
        assert_eq!(user["lock_passwd"], Value::from(false));
        assert_eq!(user["ssh_authorized_keys"], Value::from(vec![KEY]));
        assert_eq!(document["ssh_pwauth"], Value::from(true));
    }

    /// The group comes from the distribution profile, so a profile naming
    /// `wheel` must reach the document unchanged.
    #[test]
    fn the_administrative_group_comes_from_the_profile() {
        let document = parsed(&render(&SeedRequest {
            admin_group: "wheel",
            ..request()
        }));

        assert_eq!(document["users"][0]["groups"], Value::from(vec!["wheel"]));
    }

    #[test]
    fn the_guest_gets_the_locale_and_the_timezone_it_was_asked_for() {
        let document = parsed(&render(&request()));

        assert_eq!(document["locale"], Value::from("en_US.UTF-8"));
        assert_eq!(document["timezone"], Value::from("Europe/Moscow"));
    }

    #[test]
    fn the_keyboard_layout_is_written_into_the_debian_configuration_file() {
        let document = parsed(&render(&SeedRequest {
            keyboard: "ru",
            ..request()
        }));
        let file = &document["write_files"][0];

        assert_eq!(file["path"], Value::from("/etc/default/keyboard"));
        assert_eq!(file["permissions"], Value::from("0644"));
        assert_eq!(
            file["content"],
            Value::from(
                "XKBMODEL=\"pc105\"\nXKBLAYOUT=\"ru\"\nXKBVARIANT=\"\"\n\
                 XKBOPTIONS=\"\"\nBACKSPACE=\"guess\"\n"
            )
        );
    }

    /// The file is read with `source`, so the layout is escaped for the shell
    /// on top of being inside a YAML block scalar.
    #[test]
    fn a_layout_that_would_run_a_command_arrives_escaped() {
        let document = parsed(&render(&SeedRequest {
            keyboard: "us$(reboot)",
            ..request()
        }));

        assert!(
            document["write_files"][0]["content"]
                .as_str()
                .expect("the file content is a string")
                .contains("XKBLAYOUT=\"us\\$(reboot)\"")
        );
    }

    /// Growing the root filesystem to `disk_gb` is a VMLord promise, so it is
    /// stated in the document rather than left to cloud-init's defaults.
    #[test]
    fn the_root_filesystem_is_grown_to_fill_the_disk() {
        let document = parsed(&render(&request()));

        assert_eq!(document["growpart"]["mode"], Value::from("auto"));
        assert_eq!(document["growpart"]["devices"], Value::from(vec!["/"]));
        assert_eq!(document["resize_rootfs"], Value::from(true));
    }

    /// A value with an apostrophe is the one that breaks naive quoting.
    #[test]
    fn a_value_with_an_apostrophe_survives_the_round_trip() {
        let document = parsed(&render(&SeedRequest {
            timezone: "Europe/O'Hare",
            ..request()
        }));

        assert_eq!(document["timezone"], Value::from("Europe/O'Hare"));
    }
}
```

- [ ] **Step 2: Убедиться, что тесты падают**

В `crates/seed/src/lib.rs` добавить `mod user_data;` рядом с `mod meta_data;`, затем:

Run: `cargo test -p vmlord-seed user_data`
Expected: FAIL — `cannot find function 'render' in this scope`.

- [ ] **Step 3: Написать реализацию**

В начало `crates/seed/src/user_data.rs`, перед `mod tests`:

```rust
//! `user-data`: what cloud-init is asked to do on the first boot.
//!
//! Printed by hand rather than serialised. The document is small and fixed, and
//! what it must be is known exactly -- including the `#cloud-config` line, which
//! is a comment to YAML and the format marker to cloud-init.

use crate::{SeedRequest, scalar};

/// The indentation a block scalar's content sits at inside `write_files`.
const FILE_INDENT: &str = "      ";

/// Prints the document.
pub(crate) fn render(request: &SeedRequest<'_>) -> String {
    let mut document = String::from("#cloud-config\nusers:\n");

    document.push_str(&format!("  - name: {}\n", scalar::yaml(request.username)));
    document.push_str("    shell: '/bin/bash'\n");
    document.push_str(&format!(
        "    groups: [{}]\n",
        scalar::yaml(request.admin_group)
    ));
    // cloud-init writes this into /etc/sudoers.d itself, so the rule holds
    // whatever the administrative group is called. It never asks for a
    // password: a key-only login has none to give.
    document.push_str("    sudo: 'ALL=(ALL) NOPASSWD:ALL'\n");
    match request.password_hash {
        Some(hash) => {
            document.push_str("    lock_passwd: false\n");
            document.push_str(&format!("    hashed_passwd: {}\n", scalar::yaml(hash)));
        }
        None => document.push_str("    lock_passwd: true\n"),
    }
    if let Some(key) = request.authorized_key {
        document.push_str("    ssh_authorized_keys:\n");
        document.push_str(&format!("      - {}\n", scalar::yaml(key)));
    }

    document.push_str(&format!("ssh_pwauth: {}\n", password_login_allowed(request)));
    document.push_str(&format!("locale: {}\n", scalar::yaml(request.locale)));
    document.push_str(&format!("timezone: {}\n", scalar::yaml(request.timezone)));
    document.push_str(&keyboard_file(request.keyboard));
    document.push_str("growpart:\n  mode: auto\n  devices: ['/']\nresize_rootfs: true\n");

    document
}

/// Whether the SSH daemon accepts a password.
///
/// Both halves matter: without a hash there is no password to accept, and with
/// SSH off the setting has nobody to apply to.
fn password_login_allowed(request: &SeedRequest<'_>) -> bool {
    matches!(request.ssh, vmlord_core::SshAccess::Enabled { .. }) && request.password_hash.is_some()
}

/// The `write_files` entry that sets the console keyboard layout.
///
/// `/etc/default/keyboard` is Debian-family: Fedora keeps the same setting in
/// `/etc/vconsole.conf` under different keys, which is a different mechanism
/// rather than a different value, so it waits for a second distribution.
///
/// The layout is escaped for the shell, not for YAML: this file is read with
/// `source`, where an unescaped `$` or quote is code.
fn keyboard_file(layout: &str) -> String {
    let layout = scalar::shell(layout);
    format!(
        "write_files:\n  - path: '/etc/default/keyboard'\n    permissions: '0644'\n    content: |\n\
         {FILE_INDENT}XKBMODEL=\"pc105\"\n\
         {FILE_INDENT}XKBLAYOUT=\"{layout}\"\n\
         {FILE_INDENT}XKBVARIANT=\"\"\n\
         {FILE_INDENT}XKBOPTIONS=\"\"\n\
         {FILE_INDENT}BACKSPACE=\"guess\"\n"
    )
}
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-seed`
Expected: PASS, 15 тестов.

- [ ] **Step 5: Кросс-проверка**

Run: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets`
Expected: без ошибок.

- [ ] **Step 6: Коммит**

```bash
git add crates/seed
git commit -m "$(printf 'TASK-58: Render the NoCloud user-data document\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>')"
```

---

### Task 5: Вход без пароля, без ключа и с выключенным SSH

Три оставшиеся строки таблицы «вход → документ» из спеки.

**Files:**
- Modify: `crates/seed/src/user_data.rs` (функция `render`, новая `disable_ssh`)
- Test: тесты внутри `crates/seed/src/user_data.rs`

**Interfaces:**
- Consumes: `user_data::render` (Task 4), `SeedRequest.ssh_units` (Task 3), `DistroProfile.ssh_units` (Task 1).
- Produces: ничего нового наружу — меняется только содержимое документа.

- [ ] **Step 1: Написать падающие тесты**

В `mod tests` файла `crates/seed/src/user_data.rs`:

```rust
    /// A VM reachable by key only: the account exists, password login does not.
    #[test]
    fn without_a_hash_the_account_has_no_password_at_all() {
        let document = parsed(&render(&SeedRequest {
            password_hash: None,
            ..request()
        }));
        let user = &document["users"][0];

        assert_eq!(user["lock_passwd"], Value::from(true));
        assert_eq!(user.get("hashed_passwd"), None);
        assert_eq!(document["ssh_pwauth"], Value::from(false));
    }

    #[test]
    fn without_a_key_the_user_has_no_authorized_keys_entry() {
        let document = parsed(&render(&SeedRequest {
            authorized_key: None,
            ..request()
        }));

        assert_eq!(document["users"][0].get("ssh_authorized_keys"), None);
    }

    /// A cloud image ships the SSH daemon enabled, so "SSH off" has to be an
    /// action: silence would leave the daemon running and the choice void.
    #[test]
    fn ssh_turned_off_disables_the_daemon_named_by_the_profile() {
        let units = ["ssh.socket".to_string(), "ssh.service".to_string()];
        let document = parsed(&render(&SeedRequest {
            ssh: SshAccess::Disabled,
            authorized_key: None,
            ssh_units: &units,
            ..request()
        }));

        assert_eq!(
            document["runcmd"][0],
            Value::from(vec![
                "systemctl",
                "disable",
                "--now",
                "ssh.socket",
                "ssh.service"
            ])
        );
        assert_eq!(document["ssh_pwauth"], Value::from(false));
    }

    #[test]
    fn ssh_left_on_disables_nothing() {
        let units = ["ssh.socket".to_string()];
        let document = parsed(&render(&SeedRequest {
            ssh_units: &units,
            ..request()
        }));

        assert_eq!(document.get("runcmd"), None);
    }

    /// The plaintext password never reaches this crate, and the private key is
    /// never handed to it. The test states that as a property of the output.
    #[test]
    fn no_secret_beyond_the_hash_and_the_public_key_appears_in_the_document() {
        let document = render(&request());

        assert!(!document.contains("hunter2"));
        assert!(!document.contains("PRIVATE KEY"));
        assert!(document.contains(HASH), "the hash is what the guest needs");
    }
```

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-seed user_data`
Expected: FAIL — `ssh_turned_off_disables_the_daemon_named_by_the_profile` падает на отсутствующем `runcmd`; остальные проходят, потому что Task 4 уже печатает ветки по `password_hash` и `authorized_key`.

- [ ] **Step 3: Дописать реализацию**

В `crates/seed/src/user_data.rs`, в конец `render`, перед `document`:

```rust
    if let Some(command) = disable_ssh(request) {
        document.push_str(&command);
    }
```

И новая функция после `password_login_allowed`:

```rust
/// The `runcmd` entry that stops the SSH daemon and keeps it stopped.
///
/// The unit names come from the profile rather than from here: Debian-family
/// systems socket-activate `ssh.socket`, Fedora and SUSE name both `sshd`. A
/// unit that does not exist on a given release makes `systemctl` return
/// non-zero, which `runcmd` does not treat as fatal.
fn disable_ssh(request: &SeedRequest<'_>) -> Option<String> {
    if request.ssh != vmlord_core::SshAccess::Disabled || request.ssh_units.is_empty() {
        return None;
    }

    let units = request
        .ssh_units
        .iter()
        .map(|unit| scalar::yaml(unit))
        .collect::<Vec<_>>()
        .join(", ");
    log::debug!("the seed disables the SSH daemon: {units}");
    Some(format!(
        "runcmd:\n  - ['systemctl', 'disable', '--now', {units}]\n"
    ))
}
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-seed`
Expected: PASS, 20 тестов.

- [ ] **Step 5: Кросс-проверка**

Run: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets`
Expected: без ошибок.

- [ ] **Step 6: Коммит**

```bash
git add crates/seed
git commit -m "$(printf 'TASK-58: Print the seed for a key-only login and for SSH turned off\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>')"
```

---

### Task 6: Публичный вход `build`

Один вызов, который #61 сделает вместо двух, и одна строка лога о том, что уехало в гостя.

**Files:**
- Modify: `crates/seed/src/lib.rs`
- Test: тесты внутри `crates/seed/src/lib.rs`

**Interfaces:**
- Consumes: `user_data::render` (Task 4), `meta_data::render` (Task 3).
- Produces: `pub struct Seed { pub user_data: String, pub meta_data: String }` (без `Debug`); `pub fn build(request: &SeedRequest<'_>) -> Seed`.

- [ ] **Step 1: Написать падающий тест**

В конец `crates/seed/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{SeedRequest, build};
    use vmlord_core::SshAccess;

    #[test]
    fn a_seed_carries_both_documents() {
        let seed = build(&SeedRequest {
            vm_name: "my-vm",
            instance_id: "vmlord-4f1c0e5a",
            username: "dev",
            password_hash: Some("$6$rounds=4096$salt$hash"),
            authorized_key: None,
            ssh: SshAccess::Enabled { deploy_key: false },
            locale: "en_US.UTF-8",
            keyboard: "us",
            timezone: "Europe/Moscow",
            admin_group: "sudo",
            ssh_units: &[],
        });

        assert!(seed.user_data.starts_with("#cloud-config\n"));
        assert!(seed.meta_data.contains("instance-id: 'vmlord-4f1c0e5a'"));
    }
}
```

- [ ] **Step 2: Убедиться, что тест падает**

Run: `cargo test -p vmlord-seed a_seed_carries`
Expected: FAIL — `cannot find function 'build' in this scope`.

- [ ] **Step 3: Написать реализацию**

В `crates/seed/src/lib.rs`, после объявления `SeedRequest`:

```rust
/// The two documents that go into the seed volume.
///
/// No `Debug`: `user_data` holds the password hash, and a hash has no business
/// in a log line.
pub struct Seed {
    pub user_data: String,
    pub meta_data: String,
}

/// Builds both documents.
///
/// Infallible by construction. Values arrive validated by
/// `Provisioning::validate`, which rejects control characters, and everything
/// else survives quoting, so there is no input this can refuse. Failure starts
/// in #59, where the documents meet a filesystem.
#[must_use]
pub fn build(request: &SeedRequest<'_>) -> Seed {
    log::debug!(
        "building a seed for VM \"{}\" ({}): user \"{}\", password {}, key {}, {}",
        request.vm_name,
        request.instance_id,
        request.username,
        if request.password_hash.is_some() {
            "hashed"
        } else {
            "unset"
        },
        if request.authorized_key.is_some() {
            "deployed"
        } else {
            "absent"
        },
        match request.ssh {
            SshAccess::Disabled => "SSH off",
            SshAccess::Enabled { .. } => "SSH on",
        }
    );

    Seed {
        user_data: user_data::render(request),
        meta_data: meta_data::render(request),
    }
}
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-seed`
Expected: PASS, 21 тест.

- [ ] **Step 5: Кросс-проверка и линты**

Run: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets`
Run: `cargo clippy -p vmlord-seed -p vmlord-core --all-targets`
Expected: без ошибок и без предупреждений.

- [ ] **Step 6: Коммит**

```bash
git add crates/seed
git commit -m "$(printf 'TASK-58: Build both seed documents in one call\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>')"
```

---

### Task 7: Документация

**Files:**
- Modify: `ARCHITECTURE.md` (раздел о крейтах и раздел про provisioning)

**Interfaces:**
- Consumes: всё, что построено в Tasks 1-6.
- Produces: ничего.

- [ ] **Step 1: Найти места правки**

Run: `grep -n "crates/image\|crates/core\|Provisioning\|DistroProfile" ARCHITECTURE.md | head -30`

Правятся два места: перечень крейтов (туда добавляется `vmlord-seed`) и раздел о provisioning-контракте, написанный в #57.

- [ ] **Step 2: Описать крейт**

В перечень крейтов, после `vmlord-platform`, добавить абзац в стиле соседних записей: `vmlord-seed` собирает документы NoCloud из контракта провизионинга; зависит от `vmlord-core`, не зависит ни от Windows, ни от файловой системы; `build(&SeedRequest) -> Seed` инфалибельна; ISO9660-writer поверх этих документов появится в #59.

- [ ] **Step 3: Описать документ**

В раздел о provisioning добавить: какие ключи печатаются (`users` с `sudo`-правилом `ALL=(ALL) NOPASSWD:ALL`, `ssh_pwauth`, `locale`, `timezone`, `write_files` для `/etc/default/keyboard`, `growpart`, `resize_rootfs`, `runcmd` при выключенном SSH); что `instance-id` берётся из `vm_id` и потому неизменен; что plaintext-пароль в крейт не попадает; что раскладка через `/etc/default/keyboard` — допущение Debian-семейства; что `DistroProfile.ssh_units` описывает юниты SSH-демона.

- [ ] **Step 4: Коммит**

```bash
git add ARCHITECTURE.md
git commit -m "$(printf 'TASK-58: Document the cloud-config generation\n\nCo-Authored-By: Claude Opus 5 <noreply@anthropic.com>')"
```

---

## Проверка перед завершением

- [ ] `cargo test -p vmlord-core -p vmlord-seed` — всё зелёное.
- [ ] `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets` — без ошибок.
- [ ] `cargo clippy -p vmlord-seed -p vmlord-core --all-targets` — без предупреждений.
- [ ] `git log --oneline` показывает семь коммитов с префиксом `TASK-58: `.
- [ ] Новых продакшн-зависимостей нет: в `crates/seed/Cargo.toml` только `log` и `vmlord-core`, `serde_yaml_ng` — в `[dev-dependencies]`.
