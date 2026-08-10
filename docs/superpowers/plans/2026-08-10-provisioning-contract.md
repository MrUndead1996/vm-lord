# Provisioning Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Выделить provisioning-контракт VMLord в отдельные типы `crates/core`, сделав «локальный носитель без provisioning» и «облачный образ с provisioning» двумя вариантами одного enum.

**Architecture:** `core::distro` получает `DistroProfile` из `crates/image` (поля становятся `String`, `const UBUNTU` — функцией `ubuntu()`). Новый `core::provisioning` вводит `VmSource`, `CloudImage`, `Provisioning`, `SshAccess` и непечатаемый `Password`, а `VmCreateRequest` меняет пять плоских полей на одно `source: VmSource`. Валидация имени пользователя переезжает из UI в домен; нативная платформа отвергает `CloudImage` до задачи #61, legacy-бэкенд получает пустые учётные данные, UI строит `LocalMedia`.

**Tech Stack:** Rust 2024, workspace `vmlord`, `log`, `serde`; тесты — встроенные `#[cfg(test)]` модули и `crates/*/tests`.

**Спека:** `docs/superpowers/specs/2026-08-10-provisioning-contract-design.md`

## Global Constraints

- Ветка задачи: `task-57-provisioning-contract` (уже создана, спека закоммичена).
- Префикс каждого коммита — `TASK-57: `, сообщение на английском, в конце `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
- Комментарии и документация в коде — на английском, как во всём репозитории; объясняют «почему», а не «что».
- Логирование только через фасад `log`, уровни DEBUG..ERROR, `TRACE` не используется.
- `unsafe` в `core`, `image`, `app`, `ui` запрещён (`unsafe_code = "deny"` в workspace-lints).
- Никаких новых зависимостей ни в одном `Cargo.toml`.
- Кросс-проверка после каждой задачи: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets` (единственный способ собрать Windows-only крейты на WSL).
- Хостовые тесты после каждой задачи: `cargo test -p vmlord-core -p vmlord-image`.
- `crates/platform`, `crates/ui`, `crates/legacy-backend`, `crates/vmlord` на WSL не запускаются — для них проверка это `cargo check` выше.

---

### Task 1: Профиль дистрибутива переезжает в `core`

**Files:**
- Create: `crates/core/src/distro.rs`
- Modify: `crates/core/src/lib.rs:3-9` (объявление и ре-экспорт модуля)
- Delete: `crates/image/src/distro.rs` — часть с `DistroProfile`, `UBUNTU` и методами URL; файл остаётся ради `validated_release`
- Modify: `crates/image/src/lib.rs:21-36`, `crates/image/src/resolve.rs:17-60`
- Test: тесты внутри `crates/core/src/distro.rs`, правка `crates/image/tests/resolve.rs:6,23`

**Interfaces:**
- Produces: `vmlord_core::distro::{DistroProfile, ubuntu}`; `DistroProfile { name: String, directory_template: String, file_name_template: String, checksum_file: String, default_user: String, admin_group: String }` с `#[derive(Clone, Debug, PartialEq, Eq)]` и методами `pub fn image_url(&self, release: &str) -> String`, `pub fn checksums_url(&self, release: &str) -> String`, `pub fn file_name(&self, release: &str) -> String`; `pub fn ubuntu() -> DistroProfile`. Ре-экспорт из `vmlord_core` корня и из `vmlord_image`.
- Consumes: ничего.

- [ ] **Step 1: Перенести файл целиком, затем вырезать лишнее**

```bash
git mv crates/image/src/distro.rs crates/core/src/distro.rs
```

Затем создать заново `crates/image/src/distro.rs`, оставив в нём только `validated_release`, её тесты и `use crate::error::ResolveError;`. Из `crates/core/src/distro.rs` убрать `validated_release`, её тесты и `use crate::error::ResolveError;`.

- [ ] **Step 2: Перевести профиль на владеющие строки**

В `crates/core/src/distro.rs`:

```rust
//! Which distribution to fetch, where its releases live, and what the guest
//! inside them looks like.
//!
//! A profile is a table of data, not a trait with one implementation per
//! distribution. Ubuntu and Fedora differ by a URL template, a default user, an
//! admin group and the name of a checksum file -- those are fields, not
//! behaviour, and five structs differing only in constants are exactly what
//! AGENTS.md means by unnecessary abstractions.
//!
//! The fields own their strings rather than borrowing `'static` ones: profiles
//! are to be read from a JSON file, and a parsed file yields no `&'static str`
//! short of leaking it.

/// The placeholder both templates carry.
const RELEASE_PLACEHOLDER: &str = "{release}";

/// Where a distribution publishes its cloud images, and what the guest inside
/// them looks like.
///
/// The URL is kept as two templates rather than one: the checksum file sits in
/// the same directory as the image, and a single template would have to have its
/// tail cut off to get at that directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DistroProfile {
    pub name: String,
    pub directory_template: String,
    pub file_name_template: String,
    pub checksum_file: String,
    /// The account cloud-init creates in the guest.
    pub default_user: String,
    /// The group that account must join to hold administrative rights.
    pub admin_group: String,
}

/// Ubuntu's official cloud images.
///
/// The directory is addressed by version number even though the server stores
/// it under the codename: `/releases/24.04/` answers 302 to `/releases/noble/`,
/// so a table of codenames would buy nothing and would need a line added for
/// every future release. The file name, in contrast, does carry the version
/// number rather than the codename -- verified on 24.04 and 22.04.
///
/// The architecture is baked into the template. Hyper-V here is x86_64, and a
/// field with one possible value is no better than an enum with one variant.
#[must_use]
pub fn ubuntu() -> DistroProfile {
    DistroProfile {
        name: "Ubuntu".into(),
        directory_template: "https://cloud-images.ubuntu.com/releases/{release}/release/".into(),
        file_name_template: "ubuntu-{release}-server-cloudimg-amd64.img".into(),
        checksum_file: "SHA256SUMS".into(),
        default_user: "ubuntu".into(),
        admin_group: "sudo".into(),
    }
}

impl DistroProfile {
    /// The URL of the image itself.
    #[must_use]
    pub fn image_url(&self, release: &str) -> String {
        format!("{}{}", self.directory(release), self.file_name(release))
    }

    /// The URL of the checksum file published beside it.
    #[must_use]
    pub fn checksums_url(&self, release: &str) -> String {
        format!("{}{}", self.directory(release), self.checksum_file)
    }

    /// The name the image carries inside the checksum file.
    #[must_use]
    pub fn file_name(&self, release: &str) -> String {
        self.file_name_template.replace(RELEASE_PLACEHOLDER, release)
    }

    fn directory(&self, release: &str) -> String {
        let directory = self.directory_template.replace(RELEASE_PLACEHOLDER, release);
        if directory.ends_with('/') {
            directory
        } else {
            format!("{directory}/")
        }
    }
}
```

Тесты в конце того же файла — три перенесённых, с `UBUNTU` заменённой на `ubuntu()`:

```rust
#[cfg(test)]
mod tests {
    use super::{DistroProfile, ubuntu};

    #[test]
    fn a_profile_builds_the_image_url_and_the_checksums_url_in_one_directory() {
        assert_eq!(
            ubuntu().image_url("24.04"),
            "https://cloud-images.ubuntu.com/releases/24.04/release/\
             ubuntu-24.04-server-cloudimg-amd64.img"
        );
        assert_eq!(
            ubuntu().checksums_url("24.04"),
            "https://cloud-images.ubuntu.com/releases/24.04/release/SHA256SUMS"
        );
        assert_eq!(
            ubuntu().file_name("22.04"),
            "ubuntu-22.04-server-cloudimg-amd64.img"
        );
    }

    #[test]
    fn a_directory_template_without_a_trailing_slash_still_joins_cleanly() {
        let profile = DistroProfile {
            directory_template: "http://127.0.0.1:9/{release}".into(),
            ..ubuntu()
        };

        assert_eq!(
            profile.checksums_url("24.04"),
            "http://127.0.0.1:9/24.04/SHA256SUMS",
            "a profile written by hand must not silently produce a glued-together URL"
        );
    }
}
```

- [ ] **Step 3: Объявить модуль в `core`**

В `crates/core/src/lib.rs` после строки `pub mod logging;` — в алфавитном порядке:

```rust
pub mod distro;
pub mod logging;
```

и в блоке ре-экспортов:

```rust
pub use distro::{DistroProfile, ubuntu};
```

- [ ] **Step 4: Прогнать тесты `core`**

Run: `cargo test -p vmlord-core`
Expected: PASS, среди них два новых теста профиля.

- [ ] **Step 5: Подключить `image` к перенесённому профилю**

В `crates/image/src/lib.rs` заменить `pub use distro::{DistroProfile, UBUNTU};` на:

```rust
pub use distro::validated_release;
pub use vmlord_core::{DistroProfile, ubuntu};
```

`validated_release` при этом становится `pub` вместо `pub(crate)` в `crates/image/src/distro.rs`: она осталась единственным жителем модуля, и вызывающий, строящий `CloudImage`, должен уметь проверить релиз до того, как он попадёт в домен.

В `crates/image/src/resolve.rs` заменить импорт `distro::{DistroProfile, validated_release}` на `distro::validated_release` плюс `vmlord_core::DistroProfile`, и сделать поля `ResolvedImage` владеющими:

```rust
pub struct ResolvedImage {
    pub url: String,
    pub sha256: String,
    pub default_user: String,
    pub admin_group: String,
}
```

В теле `resolve_image` соответственно:

```rust
    Ok(ResolvedImage {
        url,
        sha256,
        default_user: profile.default_user.clone(),
        admin_group: profile.admin_group.clone(),
    })
```

- [ ] **Step 6: Починить тесты `image`**

В `crates/image/tests/resolve.rs` строка 6 становится:

```rust
use vmlord_image::{DistroProfile, ResolveError, resolve_image, ubuntu};
```

а строка 23 — `..ubuntu()` вместо `..UBUNTU`. Все `&'static str`-литералы в этом файле, попадающие в поля профиля, получают `.into()`; ожидания на `default_user` / `admin_group` сравниваются со строкой как есть (`assert_eq!(resolved.default_user, "ubuntu")` продолжает работать: `String` сравним с `&str`).

- [ ] **Step 7: Прогнать хостовые тесты и кросс-проверку**

Run: `cargo test -p vmlord-core -p vmlord-image`
Expected: PASS

Run: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets`
Expected: `Finished` без ошибок.

- [ ] **Step 8: Коммит**

```bash
git add crates/core/src/distro.rs crates/core/src/lib.rs crates/image/src/distro.rs \
        crates/image/src/lib.rs crates/image/src/resolve.rs crates/image/tests/resolve.rs
git commit -m "TASK-57: Move the distribution profile into the domain

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: Модуль `core::provisioning`

Аддитивная задача: типы и валидация появляются целиком, но ещё никем не используются, поэтому весь workspace продолжает собираться.

**Files:**
- Create: `crates/core/src/provisioning.rs`
- Modify: `crates/core/src/lib.rs` (объявление модуля и ре-экспорт)
- Test: `#[cfg(test)]` модуль внутри `crates/core/src/provisioning.rs`

**Interfaces:**
- Consumes: `crate::distro::DistroProfile` из Task 1; `crate::RepositoryError`.
- Produces: `vmlord_core::{VmSource, CloudImage, Provisioning, SshAccess, Password}`; `Password::new(impl Into<String>) -> Password`, `Password::as_str(&self) -> &str`; `VmSource::validate(&self) -> Result<(), RepositoryError>`, `Provisioning::validate(&self) -> Result<(), RepositoryError>`.

- [ ] **Step 1: Написать падающие тесты**

Создать `crates/core/src/provisioning.rs`, начав с тестового модуля и хелпера. Реализация появится на шаге 3.

```rust
#[cfg(test)]
mod tests {
    use super::{CloudImage, Password, Provisioning, SshAccess, VmSource};
    use crate::distro::ubuntu;

    fn provisioning() -> Provisioning {
        Provisioning {
            username: "user".into(),
            password: Some(Password::new("secret")),
            ssh: SshAccess::Enabled { deploy_key: true },
            locale: "en_US.UTF-8".into(),
            keyboard: "us".into(),
            timezone: "Europe/Moscow".into(),
        }
    }

    fn cloud_source() -> VmSource {
        VmSource::CloudImage {
            image: CloudImage {
                profile: ubuntu(),
                release: "24.04".into(),
            },
            provisioning: provisioning(),
        }
    }

    #[test]
    fn a_fully_populated_cloud_source_is_accepted() {
        assert!(cloud_source().validate().is_ok());
    }

    #[test]
    fn local_media_needs_a_path() {
        assert!(
            VmSource::LocalMedia {
                path: "C:\\images\\ubuntu.iso".into()
            }
            .validate()
            .is_ok()
        );
        assert!(
            VmSource::LocalMedia { path: "  ".into() }
                .validate()
                .unwrap_err()
                .to_string()
                .contains("image path")
        );
    }

    #[test]
    fn a_cloud_image_needs_a_release() {
        let source = VmSource::CloudImage {
            image: CloudImage {
                profile: ubuntu(),
                release: " ".into(),
            },
            provisioning: provisioning(),
        };

        assert!(
            source
                .validate()
                .unwrap_err()
                .to_string()
                .contains("release")
        );
    }

    #[test]
    fn a_username_in_the_shape_linux_accepts_is_kept() {
        for candidate in ["user", "_svc", "ubuntu-1", "a", "u_2-x"] {
            let provisioning = Provisioning {
                username: candidate.into(),
                ..provisioning()
            };
            assert!(
                provisioning.validate().is_ok(),
                "{candidate:?} is a valid Linux user name"
            );
        }
    }

    #[test]
    fn a_username_linux_would_refuse_never_reaches_cloud_init() {
        for candidate in [
            "",
            "  ",
            "1user",
            "-user",
            "User",
            "user name",
            "user\n",
            "пользователь",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let provisioning = Provisioning {
                username: candidate.into(),
                ..provisioning()
            };
            assert!(
                provisioning
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("user name"),
                "{candidate:?} must be refused"
            );
        }
    }

    #[test]
    fn a_password_that_is_present_but_empty_is_a_mistake_not_a_key_only_login() {
        let provisioning = Provisioning {
            password: Some(Password::new("")),
            ..provisioning()
        };

        assert!(
            provisioning
                .validate()
                .unwrap_err()
                .to_string()
                .contains("password")
        );
    }

    #[test]
    fn a_key_only_login_needs_no_password() {
        let provisioning = Provisioning {
            password: None,
            ssh: SshAccess::Enabled { deploy_key: true },
            ..provisioning()
        };

        assert!(provisioning.validate().is_ok());
    }

    #[test]
    fn a_vm_nobody_can_log_into_is_refused() {
        let provisioning = Provisioning {
            password: None,
            ssh: SshAccess::Disabled,
            ..provisioning()
        };

        let message = provisioning.validate().unwrap_err().to_string();
        assert!(message.contains("password"), "got {message:?}");
        assert!(message.contains("SSH"), "got {message:?}");
    }

    #[test]
    fn ssh_without_a_deploy_key_is_a_valid_choice() {
        let provisioning = Provisioning {
            ssh: SshAccess::Enabled { deploy_key: false },
            ..provisioning()
        };

        assert!(provisioning.validate().is_ok());
    }

    #[test]
    fn a_password_only_login_is_a_valid_choice() {
        let provisioning = Provisioning {
            ssh: SshAccess::Disabled,
            ..provisioning()
        };

        assert!(provisioning.validate().is_ok());
    }

    #[test]
    fn locale_keyboard_and_timezone_must_be_present() {
        for provisioning in [
            Provisioning {
                locale: String::new(),
                ..provisioning()
            },
            Provisioning {
                keyboard: "  ".into(),
                ..provisioning()
            },
            Provisioning {
                timezone: String::new(),
                ..provisioning()
            },
        ] {
            assert!(provisioning.validate().is_err());
        }
    }

    /// These three are written into a YAML document and into
    /// `/etc/default/keyboard`; a newline in one of them is not a typo but an
    /// injection into the document.
    #[test]
    fn no_control_character_reaches_the_cloud_config_document() {
        for provisioning in [
            Provisioning {
                locale: "en_US.UTF-8\nruncmd:".into(),
                ..provisioning()
            },
            Provisioning {
                keyboard: "us\r".into(),
                ..provisioning()
            },
            Provisioning {
                timezone: "Europe/\u{7}Moscow".into(),
                ..provisioning()
            },
        ] {
            assert!(provisioning.validate().is_err());
        }
    }

    #[test]
    fn a_password_never_prints_itself() {
        let password = Password::new("hunter2");

        assert_eq!(format!("{password:?}"), "Password(<redacted>)");
        assert_eq!(password.as_str(), "hunter2");
    }

    #[test]
    fn a_whole_source_never_prints_a_password() {
        assert!(
            !format!("{:?}", cloud_source()).contains("secret"),
            "the request is logged with {{:?}} in several places"
        );
    }
}
```

- [ ] **Step 2: Убедиться, что тесты не собираются**

Run: `cargo test -p vmlord-core provisioning`
Expected: FAIL — `cannot find type Provisioning in this scope` и подобные: реализации ещё нет.

- [ ] **Step 3: Написать реализацию**

Вставить перед тестовым модулем в `crates/core/src/provisioning.rs`:

```rust
//! What VMLord promises to deliver into a Linux guest, and what it refuses to
//! promise.
//!
//! Installation media and cloud images are not two ways of doing one thing. A
//! local ISO means a human installs the system by hand; a cloud image means
//! cloud-init reads a seed VMLord wrote. Provisioning is meaningful only in the
//! second case, so it lives inside that variant rather than beside it: "a local
//! ISO with a password" is then not a state to be rejected at run time but one
//! that cannot be spelled.

use std::fmt;

use crate::{RepositoryError, distro::DistroProfile};

/// The longest user name Linux tools accept without complaint.
const MAX_USERNAME_LENGTH: usize = 32;

/// Where a new VM's system comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VmSource {
    /// Installation media. The guest system is installed by hand, so VMLord
    /// promises nothing about the user inside it.
    LocalMedia { path: String },
    /// A cloud image, provisioned by cloud-init from a seed VMLord writes.
    CloudImage {
        image: CloudImage,
        provisioning: Provisioning,
    },
}

/// Which release of which distribution to boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudImage {
    pub profile: DistroProfile,
    pub release: String,
}

/// The configuration cloud-init is asked to apply on the first boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provisioning {
    pub username: String,
    /// Absent means there is no password at all: the guest is reachable by key
    /// only, and the seed turns password authentication off.
    pub password: Option<Password>,
    pub ssh: SshAccess,
    pub locale: String,
    pub keyboard: String,
    pub timezone: String,
}

/// Whether the guest runs an SSH server, and whether VMLord puts a key in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshAccess {
    Disabled,
    Enabled { deploy_key: bool },
}

/// A password in the clear, on its way to being hashed.
///
/// Hashing happens as the seed is built; until then the plaintext travels
/// inside `VmCreateRequest`, which several call sites print with `{:?}`. The
/// manual `Debug` is what makes that harmless, and the absence of `Display`
/// keeps it from being printed by accident in any other way.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl Password {
    #[must_use]
    pub fn new(password: impl Into<String>) -> Self {
        Self(password.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Password(<redacted>)")
    }
}

impl VmSource {
    /// Checks the source, and the provisioning it carries, before any
    /// filesystem or Windows API side effect is attempted.
    pub fn validate(&self) -> Result<(), RepositoryError> {
        match self {
            Self::LocalMedia { path } => {
                if path.trim().is_empty() {
                    return Err(rejected("VM image path must not be empty"));
                }
                Ok(())
            }
            Self::CloudImage {
                image,
                provisioning,
            } => {
                if image.release.trim().is_empty() {
                    return Err(rejected("distribution release must not be empty"));
                }
                provisioning.validate()
            }
        }
    }
}

impl Provisioning {
    /// Checks the configuration the guest will be asked to apply.
    ///
    /// The rules come from the UI, which used to own them although AGENTS.md
    /// puts business logic outside that layer; they are not duplicated there.
    pub fn validate(&self) -> Result<(), RepositoryError> {
        validate_username(&self.username)?;

        if let Some(password) = &self.password
            && password.as_str().is_empty()
        {
            return Err(rejected(
                "a password must not be empty; leave it unset for a key-only login",
            ));
        }
        if self.password.is_none() && self.ssh == SshAccess::Disabled {
            return Err(rejected(
                "a VM with neither a password nor SSH cannot be logged into",
            ));
        }

        validate_guest_setting("locale", &self.locale)?;
        validate_guest_setting("keyboard layout", &self.keyboard)?;
        validate_guest_setting("timezone", &self.timezone)?;

        log::debug!(
            "provisioning user \"{}\" ({}, {}, {}), password {}, {}",
            self.username,
            self.locale,
            self.keyboard,
            self.timezone,
            if self.password.is_some() {
                "set"
            } else {
                "unset"
            },
            match self.ssh {
                SshAccess::Disabled => "SSH off",
                SshAccess::Enabled { deploy_key: false } => "SSH on",
                SshAccess::Enabled { deploy_key: true } => "SSH on with a deployed key",
            }
        );
        Ok(())
    }
}

/// Accepts the user names `useradd` accepts and cloud-init passes on unchanged.
fn validate_username(username: &str) -> Result<(), RepositoryError> {
    let shaped = |(index, byte): (usize, u8)| match index {
        0 => byte.is_ascii_lowercase() || byte == b'_',
        _ => byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-'),
    };

    if username.is_empty()
        || username.len() > MAX_USERNAME_LENGTH
        || !username.bytes().enumerate().all(shaped)
    {
        return Err(rejected(
            "the user name must be a lowercase Linux user name of up to 32 characters, \
             starting with a letter or an underscore",
        ));
    }
    Ok(())
}

/// Refuses a setting that is missing, and one that would break out of the
/// document it is written into.
fn validate_guest_setting(field: &str, value: &str) -> Result<(), RepositoryError> {
    if value.trim().is_empty() {
        return Err(rejected(format!("the {field} must not be empty")));
    }
    if value.chars().any(char::is_control) {
        return Err(rejected(format!(
            "the {field} must not contain control characters"
        )));
    }
    Ok(())
}

/// Builds the error and records the refusal in one place.
///
/// `warn` rather than `error`: a request that fails validation is a person
/// mistyping a field, not the system failing.
fn rejected(message: impl Into<String>) -> RepositoryError {
    let error = RepositoryError::new(message);
    log::warn!("rejected VM request: {error}");
    error
}
```

- [ ] **Step 4: Объявить модуль в `core`**

В `crates/core/src/lib.rs`:

```rust
pub mod distro;
pub mod logging;
pub mod progress;
pub mod provisioning;
pub mod settings;
```

и ре-экспорт рядом с остальными:

```rust
pub use provisioning::{CloudImage, Password, Provisioning, SshAccess, VmSource};
```

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test -p vmlord-core provisioning`
Expected: PASS, 14 тестов.

- [ ] **Step 6: Кросс-проверка**

Run: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets`
Expected: `Finished` без ошибок — модуль пока никем не используется.

- [ ] **Step 7: Коммит**

```bash
git add crates/core/src/provisioning.rs crates/core/src/lib.rs
git commit -m "TASK-57: Add the provisioning contract to the domain

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: `VmCreateRequest` переходит на `VmSource`

Задача правит домен и все три слоя разом: пять полей исчезают из запроса, и без правки платформы, legacy-бэкенда и UI workspace не соберётся. Промежуточных коммитов внутри задачи нет.

**Files:**
- Modify: `crates/core/src/lib.rs:15-55` (поля и `validate`), `crates/core/src/lib.rs:189-265` (тесты)
- Modify: `crates/platform/src/hcs_config.rs:10-101` и его тесты, `crates/platform/src/create.rs:126-145` и его тесты
- Modify: `crates/legacy-backend/src/windows.rs:244-275`
- Modify: `crates/ui/src/lib.rs:50-64,150-168,466-546,817-884`
- Не трогать: `crates/app` — там `VmCreateRequest` только упоминается в сигнатурах (`crates/app/src/lib.rs:209,518,594`, `crates/app/tests/update_vm.rs:24`), ни одного литерала нет
- Test: `crates/platform/tests/hyperv.rs` — все литералы запроса (14 штук)

**Interfaces:**
- Consumes: `vmlord_core::{VmSource, Provisioning, SshAccess, Password, CloudImage}` из Task 2.
- Produces: `VmCreateRequest { name, source: VmSource, ram_mb, disk_gb, cpu_cores, gpu_mode, network_mode }`; `pub(crate) fn local_media_path(request: &VmCreateRequest) -> Result<&str, RepositoryError>` в `crates/platform/src/hcs_config.rs`.

- [ ] **Step 1: Написать падающие тесты домена**

В `crates/core/src/lib.rs` заменить тестовый модуль на этот:

```rust
#[cfg(test)]
mod tests {
    use super::{GpuMode, NetworkMode, VmCreateRequest, VmSource};

    fn valid_request() -> VmCreateRequest {
        VmCreateRequest {
            name: "dev-linux".into(),
            source: VmSource::LocalMedia {
                path: "C:\\images\\ubuntu.iso".into(),
            },
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        }
    }

    #[test]
    fn accepts_a_fully_populated_request() {
        assert!(valid_request().validate().is_ok());
    }

    #[test]
    fn rejects_an_empty_name() {
        let request = VmCreateRequest {
            name: "  ".into(),
            ..valid_request()
        };
        assert!(request.validate().unwrap_err().to_string().contains("name"));
    }

    #[test]
    fn rejects_an_empty_image_path() {
        let request = VmCreateRequest {
            source: VmSource::LocalMedia {
                path: String::new(),
            },
            ..valid_request()
        };
        assert!(
            request
                .validate()
                .unwrap_err()
                .to_string()
                .contains("image path")
        );
    }

    #[test]
    fn rejects_provisioning_the_source_refuses() {
        use super::{CloudImage, Provisioning, SshAccess, distro::ubuntu};

        let request = VmCreateRequest {
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "Invalid".into(),
                    password: None,
                    ssh: SshAccess::Enabled { deploy_key: true },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                },
            },
            ..valid_request()
        };

        assert!(
            request
                .validate()
                .unwrap_err()
                .to_string()
                .contains("user name"),
            "the request must ask its source to validate itself"
        );
    }

    #[test]
    fn rejects_zero_ram_disk_or_cpu_cores() {
        assert!(
            VmCreateRequest {
                ram_mb: 0,
                ..valid_request()
            }
            .validate()
            .is_err()
        );
        assert!(
            VmCreateRequest {
                disk_gb: 0,
                ..valid_request()
            }
            .validate()
            .is_err()
        );
        assert!(
            VmCreateRequest {
                cpu_cores: 0,
                ..valid_request()
            }
            .validate()
            .is_err()
        );
    }
}
```

- [ ] **Step 2: Убедиться, что они падают**

Run: `cargo test -p vmlord-core`
Expected: FAIL — `struct VmCreateRequest has no field named source`.

- [ ] **Step 3: Поменять запрос**

В `crates/core/src/lib.rs`:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmCreateRequest {
    pub name: String,
    /// Where the system comes from, and -- for a cloud image -- what VMLord
    /// promises to configure inside it.
    pub source: VmSource,
    pub ram_mb: u32,
    pub disk_gb: u32,
    pub cpu_cores: u32,
    pub gpu_mode: GpuMode,
    pub network_mode: NetworkMode,
}
```

В `validate()` строки про `image_path` заменяются на делегирование, остальные проверки остаются как были:

```rust
        if self.name.trim().is_empty() {
            return Err(RepositoryError::new("VM name must not be empty"));
        }
        self.source.validate()?;
```

- [ ] **Step 4: Прогнать тесты домена**

Run: `cargo test -p vmlord-core`
Expected: PASS.

- [ ] **Step 5: Научить платформу читать источник**

В `crates/platform/src/hcs_config.rs` добавить рядом с `ensure_supported_network_mode`:

```rust
/// The path of the installer ISO, or a refusal naming the task that will teach
/// the pipeline to build a VM from a cloud image.
///
/// Same shape as `ensure_supported_network_mode`: an unsupported request is
/// refused in one place, with a message that says which task lifts the refusal.
pub(crate) fn local_media_path(request: &VmCreateRequest) -> Result<&str, RepositoryError> {
    match &request.source {
        VmSource::LocalMedia { path } => Ok(path),
        VmSource::CloudImage { .. } => {
            let error = RepositoryError::new(
                "the HCS backend cannot build a VM from a cloud image yet; \
                 seed generation and provisioning arrive with #61",
            );
            log::error!("{error}");
            Err(error)
        }
    }
}
```

Импорт в шапке файла становится:

```rust
use vmlord_core::{GpuMode, NetworkMode, RepositoryError, VmCreateRequest, VmSource};
```

В `HcsVmConfigBuilder::build` строка 52 (`path: PathBuf::from(&request.image_path)`) заменяется. Перед созданием `attachments`:

```rust
        let image_path = local_media_path(request)?;
```

и в самом attachment: `path: PathBuf::from(image_path),`. Doc-комментарий метода: `request.image_path` → `the installer ISO of its source`.

- [ ] **Step 6: Починить `create.rs`**

`crates/platform/src/create.rs:126-145` — проверка существования файла и `HcsGrantVmAccess`. Ввести путь один раз рядом со строкой 91, где строится конфигурация:

```rust
        let image_path = local_media_path(request)?;
```

и заменить оба обращения `&request.image_path` на `image_path`. Импорт на строке 11 становится:

```rust
    hcs_config::{HcsVmConfigBuilder, local_media_path},
```

- [ ] **Step 7: Починить legacy-бэкенд**

`crates/legacy-backend/src/windows.rs:251-253` и 265-269. AppSandbox получает путь и пустые учётные данные: под `LocalMedia` их в домене нет, а `CloudImage` этот бэкенд не поддерживает.

```rust
        // AppSandbox's own model is "installation media plus unattended
        // answers", which the domain no longer spells: a local medium means a
        // hand-installed system. The credentials therefore go over empty, and
        // iso-patch performs no unattended install. The legacy backend is
        // transitional (AGENTS.md), and #66 removes iso-patch altogether.
        let image_path = match &request.source {
            VmSource::LocalMedia { path } => wide_string(path),
            VmSource::CloudImage { .. } => {
                return Err(RepositoryError::new(
                    "the legacy AppSandbox backend cannot create a VM from a cloud image",
                ));
            }
        };
        let username = wide_string("");
        let password = wide_string("");
```

и в структуре `AsbVmConfig`: `ssh_enabled: 0,` и `ssh_deploy_key: 0,`. Импорт `VmSource` добавляется к существующему `use vmlord_core::{...}` в шапке файла.

- [ ] **Step 8: Починить UI**

В `crates/ui/src/lib.rs`:

1. Из `struct CreateVmForm` (строки 50-64) убрать `username`, `password`, `password_confirmation`, `ssh_enabled`, `ssh_deploy_key`; из `impl Default for CreateVmForm` (строки 150-168) — их инициализацию.
2. Из диалога убрать строки 517-534 (метки и поля Username/Password/Confirm) и блок 537-546 (строка Options с двумя чекбоксами).
3. В `create_vm_request` убрать проверки имени пользователя и пароля (строки 847-869) и собрать запрос так:

```rust
    Ok(VmCreateRequest {
        name: name.into(),
        source: VmSource::LocalMedia {
            path: form.image_path.trim().into(),
        },
        ram_mb: form.ram_mb,
        disk_gb: form.disk_gb,
        cpu_cores: form.cpu_cores,
        gpu_mode: form.gpu_mode,
        network_mode: form.network_mode,
    })
```

4. Добавить `VmSource` в импорт из `vmlord_core`.

Комментарий над формой, объясняющий пропажу полей:

```rust
/// The form a local installation medium needs. A medium means the system is
/// installed by hand, so there is no user, password or SSH choice to make here;
/// those widgets return with the cloud-image form in #65.
```

- [ ] **Step 9: Починить тесты платформы и приложения**

Во всех литералах `VmCreateRequest` (`crates/platform/src/hcs_config.rs`, `crates/platform/src/create.rs`, `crates/platform/tests/hyperv.rs`) пять полей заменяются одним. Например, в `crates/platform/src/create.rs:285-297`:

```rust
        let request = VmCreateRequest {
            name: "dev-linux".into(),
            source: VmSource::LocalMedia {
                path: image_path.to_string_lossy().into_owned(),
            },
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        };
```

Найти все места: `rg -n 'VmCreateRequest \{' crates`.

- [ ] **Step 10: Добавить тест на отказ по облачному образу**

В тестовый модуль `crates/platform/src/hcs_config.rs`:

```rust
    #[test]
    fn a_cloud_image_is_refused_with_the_task_that_will_support_it() {
        use vmlord_core::{CloudImage, Password, Provisioning, SshAccess, distro::ubuntu};

        let request = VmCreateRequest {
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "user".into(),
                    password: Some(Password::new("secret")),
                    ssh: SshAccess::Enabled { deploy_key: true },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                },
            },
            ..request()
        };

        let error = HcsVmConfigBuilder::build(&request, Path::new("C:\\vms\\dev\\system.vhdx"))
            .expect_err("the pipeline cannot provision a cloud image yet");

        assert!(error.to_string().contains("#61"), "got {error}");
    }
```

- [ ] **Step 11: Прогнать всё**

Run: `cargo test -p vmlord-core -p vmlord-image`
Expected: PASS.

Run: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets`
Expected: `Finished` без ошибок.

Run: `cargo clippy --target=x86_64-pc-windows-gnu --workspace --all-targets`
Expected: без предупреждений; если clippy указывает на неиспользуемое поле формы или лишний `clone`, исправить и повторить.

- [ ] **Step 12: Коммит**

```bash
git add -A
git commit -m "TASK-57: Carry the image source in the VM create request

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: Документация

**Files:**
- Modify: `ARCHITECTURE.md` — раздел `## Core` (строки 133-141), «Implemented scaffold» (около строк 585-592, абзац про UI) и «VM update contract» (строки 775-793), после которого добавляется новый подраздел

**Interfaces:**
- Consumes: типы из Task 2 и форму запроса из Task 3.
- Produces: ничего кодового.

- [ ] **Step 1: Описать контракт в ARCHITECTURE.md**

После подраздела «VM update contract» добавить:

```markdown
### VM creation contract

A VM's system comes from one of two sources, and they are different in kind:

* `VmSource::LocalMedia` is installation media. The system is installed by
  hand, so VMLord promises nothing about the user inside it.
* `VmSource::CloudImage` is a distribution's cloud image, provisioned by
  cloud-init from a seed VMLord writes. It carries the provisioning contract:
  user name, optional password, SSH access, locale, keyboard layout and
  timezone.

Provisioning lives inside the cloud variant rather than beside it, so "a local
medium with a password" is a state that cannot be spelled rather than one that
has to be rejected at run time. `core::provisioning` owns the types and their
validation, including the user-name rules the UI used to hold; `core::distro`
owns `DistroProfile`, the table of where a distribution publishes its images
and what the guest inside them looks like.

A password travels as `Password`, whose `Debug` prints `<redacted>` and which
has no `Display`: until the seed hashes it (#61), the plaintext sits inside a
request that several call sites log with `{:?}`.

The native backend refuses `CloudImage` with a message naming #61, the task
that will build a VM from one. The legacy AppSandbox backend is given empty
credentials for `LocalMedia`: its own model was "media plus unattended
answers", which the domain no longer spells. That is a deliberate loss on a
transitional path -- #66 removes the iso-patch dependency it belongs to.
```

- [ ] **Step 2: Поправить раздел `## Core`**

Заменить его тело на:

```markdown
Contains all virtualization logic.

This layer exposes safe Rust APIs.

It knows nothing about the UI.

Its modules today: `settings`, `logging`, `progress`, `distro` (distribution
profiles) and `provisioning` (what VMLord delivers into a Linux guest), plus
the request, summary and repository types.
```

- [ ] **Step 3: Поправить абзац про UI**

В «Implemented scaffold» предложение «can create Linux VMs from ISO images» дополнить: форма создания задаёт носитель, размеры и режимы, но не пользователя и пароль — они возвращаются вместе с формой облачного образа (#65).

- [ ] **Step 4: Проверить, что ничего не сломано**

Run: `cargo test -p vmlord-core -p vmlord-image`
Expected: PASS.

Run: `cargo check --target=x86_64-pc-windows-gnu --workspace --all-targets`
Expected: `Finished` без ошибок.

- [ ] **Step 5: Коммит**

```bash
git add ARCHITECTURE.md
git commit -m "TASK-57: Document the provisioning contract

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```
