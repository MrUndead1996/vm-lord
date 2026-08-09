# TASK-53: резолвер релизов Ubuntu — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** По версии Ubuntu (`"24.04"`) VMLord получает URL официального cloud-образа и его SHA256, разобрав опубликованный рядом `SHA256SUMS`, — то есть ровно те два значения, которых `fetch_image` из #50 ждёт от вызывающего.

**Architecture:** Три новых модуля в существующем крейте `vmlord-image`: `distro.rs` — профиль дистрибутива как таблица данных (шаблоны URL, дефолтный пользователь, админская группа) плюс проверка формы версии; `checksums.rs` — чистый разбор `SHA256SUMS`; `resolve.rs` — единственная функция, ходящая в сеть, поверх уже существующего `build_agent`. Ошибки — отдельный `ResolveError` в существующем `error.rs`.

**Tech Stack:** Rust 2024, `ureq` 3.4 (уже в зависимостях крейта), `log`, `std::net::TcpListener` для интеграционного теста.

**Спека:** `docs/superpowers/specs/2026-08-09-ubuntu-release-resolver-design.md`

## Global Constraints

- Ветка: `task-53-release-resolver` (уже создана от `origin/main`, спека в ней закоммичена).
- Коммиты: `TASK-53: <comment>`, автор задаётся переменными окружения `GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local`.
- Комментарии, doc-комментарии, имена тестов и сообщения об ошибках — на английском, как весь код репозитория. Doc-комментарий объясняет **почему**, а не пересказывает сигнатуру.
- Логирование через крейт `log`, уровни DEBUG..ERROR. `TRACE` не используется. Перед возвратом ошибки она пишется в `log::error!`.
- Никаких `anyhow`, `thiserror`, `Box<dyn Error>`: ошибки — руками написанное перечисление с `Display` и `std::error::Error`, по образцу `DownloadError` (`crates/image/src/error.rs`).
- `unsafe` запрещён: в `vmlord-image` действует `unsafe_code = "deny"` из `[workspace.lints.rust]`. Ни одна задача плана его не требует.
- Никакого async: ни `tokio`, ни `.await` в проекте нет и не появляется.
- Новых зависимостей не добавляется: `ureq` и `log` уже в `crates/image/Cargo.toml`.
- Тесты гоняются нативно под Linux: `cargo test -p vmlord-image`. Сборка под целевую платформу проверяется в конце: `cargo build --target=x86_64-pc-windows-gnu`.
- Ни один тест не ходит в сеть наружу. Единственный сетевой тест — на петлевом `TcpListener`.

### Факты, проверенные экспериментом до написания плана

Подтверждены запросами к `cloud-images.ubuntu.com` и запуском `ureq` на этой машине. Не переоткрывать и не «чинить» вопреки им.

1. `https://cloud-images.ubuntu.com/releases/24.04/release/SHA256SUMS` отвечает **302** на `.../releases/noble/release/SHA256SUMS`, и `ureq` идёт за редиректом сам: итоговый статус 200, тело 7843 байта. Поэтому таблицы кодовых имён в коде нет.
2. Имя файла в каталоге релиза содержит **номер версии**, а не кодовое имя: `ubuntu-24.04-server-cloudimg-amd64.img`. Проверено на 24.04 и 22.04.
3. `response.body_mut().with_config().limit(n).read_to_string()` компилируется и при превышении потолка даёт `Err` с текстом `the response body is larger than request limit: n`.
4. Формат строки — `<64 hex><пробел>*<имя>`; звёздочка означает бинарный режим. В боевом файле 66 строк.

---

## File Structure

**Создаются:**

- `crates/image/src/distro.rs` — `DistroProfile`, константа `UBUNTU`, `validated_release`, сборка URL образа и URL файла сумм.
- `crates/image/src/checksums.rs` — `parse_sha256sums` и разбор одной строки.
- `crates/image/src/resolve.rs` — `ResolvedImage`, `resolve_image`, чтение тела с потолком.
- `crates/image/tests/fixtures/ubuntu-24.04-SHA256SUMS` — срез настоящего файла Canonical.
- `crates/image/tests/resolve.rs` — интеграционный тест на петлевом сервере.

**Изменяются:**

- `crates/image/src/error.rs` — добавляется `ResolveError`.
- `crates/image/src/lib.rs` — новые модули и реэкспорты.
- `crates/image/tests/support/mod.rs` — метод `base_url`.
- `ARCHITECTURE.md` — раздел о резолвере релизов.

**Разделение ровно по границам тестируемости:** `distro.rs` и `checksums.rs` — чистые функции без единого сетевого вызова, `resolve.rs` — тонкая оболочка, в которой нечего проверять, кроме склейки и статусов.

---

### Task 1: `ResolveError` и профиль дистрибутива

**Files:**
- Create: `crates/image/src/distro.rs`
- Modify: `crates/image/src/error.rs` (в конец файла), `crates/image/src/lib.rs`

**Interfaces:**
- Consumes: ничего.
- Produces:
  - `pub enum ResolveError { InvalidRelease(String), Http(String), UnexpectedStatus { status: u16 }, MalformedChecksums { url: String }, ImageNotListed { file_name: String, url: String } }` с `Debug`, `Display`, `std::error::Error`
  - `pub struct DistroProfile` с полями `name`, `directory_template`, `file_name_template`, `checksum_file`, `default_user`, `admin_group` — все `&'static str`
  - `pub const UBUNTU: DistroProfile`
  - `pub(crate) fn validated_release(release: &str) -> Result<&str, ResolveError>`
  - методы `pub(crate) fn image_url(&self, release: &str) -> String`, `pub(crate) fn checksums_url(&self, release: &str) -> String`, `pub(crate) fn file_name(&self, release: &str) -> String`

- [ ] **Step 1: Написать падающие тесты**

Создать `crates/image/src/distro.rs` и **сразу** добавить `mod distro;` в список модулей `crates/image/src/lib.rs` в алфавитном порядке: без этой строки файл не компилируется вовсе, и следующий шаг покажет «0 tests» вместо падения.

Положить в новый файл **только** тесты:

```rust
#[cfg(test)]
mod tests {
    use super::{DistroProfile, UBUNTU, validated_release};
    use crate::error::ResolveError;

    #[test]
    fn a_release_version_is_accepted_in_the_shape_canonical_publishes() {
        for candidate in ["24.04", "22.04", "24.10", "100.04"] {
            assert_eq!(validated_release(candidate).unwrap(), candidate);
        }
    }

    #[test]
    fn anything_that_is_not_a_version_is_refused_before_it_reaches_a_url() {
        for candidate in [
            "",
            "noble",
            "24",
            "24.4",
            "24.04.1",
            "24.04 ",
            " 24.04",
            "24.04/../..",
            "../../etc",
            "2x.04",
        ] {
            assert!(
                matches!(
                    validated_release(candidate),
                    Err(ResolveError::InvalidRelease(_))
                ),
                "{candidate:?} must not be pasted into a URL"
            );
        }
    }

    #[test]
    fn a_profile_builds_the_image_url_and_the_checksums_url_in_one_directory() {
        assert_eq!(
            UBUNTU.image_url("24.04"),
            "https://cloud-images.ubuntu.com/releases/24.04/release/\
             ubuntu-24.04-server-cloudimg-amd64.img"
        );
        assert_eq!(
            UBUNTU.checksums_url("24.04"),
            "https://cloud-images.ubuntu.com/releases/24.04/release/SHA256SUMS"
        );
        assert_eq!(
            UBUNTU.file_name("22.04"),
            "ubuntu-22.04-server-cloudimg-amd64.img"
        );
    }

    #[test]
    fn a_directory_template_without_a_trailing_slash_still_joins_cleanly() {
        let profile = DistroProfile {
            directory_template: "http://127.0.0.1:9/{release}",
            ..UBUNTU
        };

        assert_eq!(
            profile.checksums_url("24.04"),
            "http://127.0.0.1:9/24.04/SHA256SUMS",
            "a profile written by hand must not silently produce a glued-together URL"
        );
    }
}
```

- [ ] **Step 2: Запустить и убедиться, что тесты падают**

Run: `cargo test -p vmlord-image --lib distro`
Expected: ошибка компиляции — `cannot find type DistroProfile in this scope` и `cannot find function validated_release in this scope`.

- [ ] **Step 3: Добавить `ResolveError` в `crates/image/src/error.rs`**

Дописать в конец файла:

```rust
/// A failure working out which image a release means.
///
/// Separate from `DownloadError` on purpose: the two have different callers and
/// tell the user different things. "the server published no checksum list for
/// 24.04" and "the image that arrived does not match its checksum" are
/// different accidents, and merging them would force every caller to match
/// variants that cannot occur where it stands.
#[derive(Debug)]
pub enum ResolveError {
    /// The caller supplied something that is not a release version.
    InvalidRelease(String),
    /// The transport failed: connection refused, TLS rejected, body cut short.
    Http(String),
    UnexpectedStatus {
        status: u16,
    },
    /// The body arrived but is not a list of checksums -- typically an HTML
    /// error page served with status 200.
    MalformedChecksums {
        url: String,
    },
    /// The list is a list, but this distribution does not publish that image.
    ImageNotListed {
        file_name: String,
        url: String,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRelease(value) => {
                write!(formatter, "{value:?} is not a release version like \"24.04\"")
            }
            Self::Http(message) => write!(formatter, "the release lookup failed: {message}"),
            Self::UnexpectedStatus { status } => write!(
                formatter,
                "the image server answered with status {status} for the checksum list"
            ),
            Self::MalformedChecksums { url } => {
                write!(formatter, "{url} is not a list of checksums")
            }
            Self::ImageNotListed { file_name, url } => {
                write!(formatter, "{url} lists no image named {file_name}")
            }
        }
    }
}

impl std::error::Error for ResolveError {}
```

`use` в шапке файла уже содержит `fmt`, добавлять ничего не нужно.

- [ ] **Step 4: Написать `crates/image/src/distro.rs` над тестами**

Вставить перед блоком `#[cfg(test)]`:

```rust
//! Which distribution to fetch, where its releases live, and what the guest
//! inside them looks like.
//!
//! A profile is a table of data, not a trait with one implementation per
//! distribution. Ubuntu and Fedora differ by a URL template, a default user, an
//! admin group and the name of a checksum file -- those are fields, not
//! behaviour, and five structs differing only in constants are exactly what
//! AGENTS.md means by unnecessary abstractions.

use crate::error::ResolveError;

/// The placeholder both templates carry.
const RELEASE_PLACEHOLDER: &str = "{release}";

/// Where a distribution publishes its cloud images, and what the guest inside
/// them looks like.
///
/// The URL is kept as two templates rather than one: the checksum file sits in
/// the same directory as the image, and a single template would have to have its
/// tail cut off to get at that directory.
pub struct DistroProfile {
    pub name: &'static str,
    pub directory_template: &'static str,
    pub file_name_template: &'static str,
    pub checksum_file: &'static str,
    /// The account cloud-init creates in the guest.
    pub default_user: &'static str,
    /// The group that account must join to hold administrative rights.
    pub admin_group: &'static str,
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
pub const UBUNTU: DistroProfile = DistroProfile {
    name: "Ubuntu",
    directory_template: "https://cloud-images.ubuntu.com/releases/{release}/release/",
    file_name_template: "ubuntu-{release}-server-cloudimg-amd64.img",
    checksum_file: "SHA256SUMS",
    default_user: "ubuntu",
    admin_group: "sudo",
};

impl DistroProfile {
    /// The URL of the image itself.
    pub(crate) fn image_url(&self, release: &str) -> String {
        format!("{}{}", self.directory(release), self.file_name(release))
    }

    /// The URL of the checksum file published beside it.
    pub(crate) fn checksums_url(&self, release: &str) -> String {
        format!("{}{}", self.directory(release), self.checksum_file)
    }

    /// The name the image carries inside the checksum file.
    pub(crate) fn file_name(&self, release: &str) -> String {
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

/// Accepts a release version of two or three digits, a dot and two digits, and
/// refuses everything else.
///
/// The string is pasted straight into a URL, which makes it
/// attacker-influenced input in the same sense the extension taken from a URL is
/// in `cache_file_name`: unchecked, `../..` walks the request into another
/// directory of the same server. Codenames are refused on purpose -- the server
/// redirects a version number to its codename by itself, and accepting both
/// would give one release two spellings that resolve to different file names.
pub(crate) fn validated_release(release: &str) -> Result<&str, ResolveError> {
    let (year, month) = release
        .split_once('.')
        .ok_or_else(|| ResolveError::InvalidRelease(release.to_owned()))?;

    let digits = |part: &str, longest: usize| {
        (2..=longest).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit())
    };
    if digits(year, 3) && digits(month, 2) {
        Ok(release)
    } else {
        Err(ResolveError::InvalidRelease(release.to_owned()))
    }
}
```

- [ ] **Step 5: Дописать экспорты в `crates/image/src/lib.rs`**

`mod distro;` там уже стоит с шага 1. Дописать к реэкспортам:

```rust
pub use distro::{DistroProfile, UBUNTU};
pub use error::{DownloadError, ResolveError};
```

(строка `pub use error::DownloadError;` при этом заменяется на строку с двумя именами).

- [ ] **Step 6: Запустить тесты**

Run: `cargo test -p vmlord-image --lib distro`
Expected: PASS, 4 теста.

- [ ] **Step 7: Коммит**

```bash
git add crates/image/src/distro.rs crates/image/src/error.rs crates/image/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-53: Describe where a distribution publishes its images"
```

---

### Task 2: Разбор `SHA256SUMS`

**Files:**
- Create: `crates/image/src/checksums.rs`, `crates/image/tests/fixtures/ubuntu-24.04-SHA256SUMS`
- Modify: `crates/image/src/lib.rs`

**Interfaces:**
- Consumes: `ResolveError` из Task 1.
- Produces: `pub(crate) fn parse_sha256sums(text: &str, file_name: &str, url: &str) -> Result<String, ResolveError>` — возвращает сумму в нижнем регистре.

- [ ] **Step 1: Положить фикстуру**

Создать `crates/image/tests/fixtures/ubuntu-24.04-SHA256SUMS` ровно с этим содержимым (строки скопированы дословно из настоящего файла Canonical; полный файл на 66 строк в репозитории не нужен, а этот срез содержит все интересные случаи — наш образ, его манифесты и однокоренные имена, чужие архитектуры):

```
0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe *ubuntu-24.04-server-cloudimg-amd64.img
137cdec6444a55a85c5b0ee93e77e23901f50155c55147a0a2d02025551f1b1f *ubuntu-24.04-server-cloudimg-riscv64.img
17eee7e96207f937b4399219df056953755af4dfcd8df49a7e30ebe993099b8d *ubuntu-24.04-server-cloudimg-amd64-root.manifest
17eee7e96207f937b4399219df056953755af4dfcd8df49a7e30ebe993099b8d *ubuntu-24.04-server-cloudimg-amd64.squashfs.manifest
198e71366f7e54008f8c0ff3235cbf9fb0a86c8ea32bcfd534075e5e912ec78e *ubuntu-24.04-server-cloudimg-amd64-azure.vhd.tar.gz
477a75368f11b7928d123e2d9f47e3eea140e38ff6ea7a066cb60c5d2be7ac2d *ubuntu-24.04-server-cloudimg-s390x.img
4827bf92014f23c87476dff65117110ef6c29f810905af16fb09514b019a4e4a *ubuntu-24.04-server-cloudimg-amd64-azure.vhd.manifest
4881b54323d62bb2a791a48c5bfa841492e55cf7a27af18b047edc904d595051 *ubuntu-24.04-server-cloudimg-amd64-lxd.tar.xz
497466a2fb464cbed05e28f67d93f9fa7d98217e623dd1433a4faa7f820a0778 *ubuntu-24.04-server-cloudimg-ppc64el.img
4c5ea96ac403e38105b9f02536b8ff6ee38ec1d2c301048f78b2329b6c037130 *ubuntu-24.04-server-cloudimg-amd64.squashfs
6fa20bf5cdab0b64003d2b6587a9f7343ee541234e5119078a7004b0df3d0b6d *ubuntu-24.04-server-cloudimg-amd64.manifest
8fdafa961e9de4f26747e89a122093ed772565e80bddb45bc39e2eb57df07988 *ubuntu-24.04-server-cloudimg-amd64.vmdk
915b4be62933475c3fb5f5031aa2e159294db95fb32aaa9e8b317aadcb6c065d *ubuntu-24.04-server-cloudimg-amd64-root.tar.xz
aa6da05756e85ea6dde4836b841fecb10cfd1ba3bcea320189d9af945db70476 *ubuntu-24.04-server-cloudimg-arm64.img
f4fb065998208f8183436d76899db5ca81cf6d02d790f5d42eff00b587827c54 *ubuntu-24.04-server-cloudimg-amd64.tar.gz
f822f1c5ff5ccaa617ffb70630c00af593493578a3938b0fc21ebb3806b9f2f1 *ubuntu-24.04-server-cloudimg-amd64.ova
```

- [ ] **Step 2: Написать падающие тесты**

Создать `crates/image/src/checksums.rs` и **сразу** добавить `mod checksums;` в список модулей `crates/image/src/lib.rs` в алфавитном порядке — по той же причине, что и в Task 1. Реэкспорта у модуля нет: функция `pub(crate)`.

Положить в новый файл **только** тесты:

```rust
#[cfg(test)]
mod tests {
    use super::parse_sha256sums;
    use crate::error::ResolveError;

    /// The real thing, trimmed: the same file the resolver meets in production.
    const FIXTURE: &str = include_str!("../tests/fixtures/ubuntu-24.04-SHA256SUMS");

    const URL: &str = "http://example.test/SHA256SUMS";
    const IMAGE: &str = "ubuntu-24.04-server-cloudimg-amd64.img";

    #[test]
    fn the_published_file_gives_up_the_checksum_of_the_image_we_asked_for() {
        assert_eq!(
            parse_sha256sums(FIXTURE, IMAGE, URL).unwrap(),
            "0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe"
        );
    }

    #[test]
    fn a_name_is_matched_whole_and_never_by_its_beginning() {
        let error = parse_sha256sums(FIXTURE, "ubuntu-24.04-server-cloudimg-amd64", URL)
            .expect_err("a dozen lines start with this, and none of them is an image");

        assert!(
            matches!(error, ResolveError::ImageNotListed { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn another_architecture_keeps_its_own_checksum() {
        assert_eq!(
            parse_sha256sums(FIXTURE, "ubuntu-24.04-server-cloudimg-arm64.img", URL).unwrap(),
            "aa6da05756e85ea6dde4836b841fecb10cfd1ba3bcea320189d9af945db70476"
        );
    }

    #[test]
    fn the_binary_mode_marker_is_optional() {
        let text = "\
0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe  plain.img
137cdec6444a55a85c5b0ee93e77e23901f50155c55147a0a2d02025551f1b1f *starred.img
";

        assert_eq!(
            parse_sha256sums(text, "plain.img", URL).unwrap(),
            "0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe",
            "coreutils writes the asterisk only for binary mode"
        );
        assert_eq!(
            parse_sha256sums(text, "starred.img", URL).unwrap(),
            "137cdec6444a55a85c5b0ee93e77e23901f50155c55147a0a2d02025551f1b1f"
        );
    }

    #[test]
    fn a_checksum_is_reported_lowercase_however_it_was_published() {
        let text = "0533B0655C32E68B31D792ECD6CCFCA95ABDBC536C4446874FE0513BD4140FFE *upper.img\n";

        assert_eq!(
            parse_sha256sums(text, "upper.img", URL).unwrap(),
            "0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe",
            "the caller feeds this straight to fetch_image, which wants lowercase"
        );
    }

    #[test]
    fn carriage_returns_do_not_become_part_of_the_name() {
        let text =
            "0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe *dos.img\r\n";

        assert!(parse_sha256sums(text, "dos.img", URL).is_ok());
    }

    #[test]
    fn a_body_that_is_not_a_checksum_list_is_told_apart_from_a_missing_image() {
        let html = "<!DOCTYPE html>\n<html><body>404 Not Found</body></html>\n";

        let error = parse_sha256sums(html, IMAGE, URL)
            .expect_err("an error page served with status 200 is not a checksum list");

        assert!(
            matches!(error, ResolveError::MalformedChecksums { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn an_empty_body_is_malformed_rather_than_an_empty_answer() {
        assert!(matches!(
            parse_sha256sums("", IMAGE, URL),
            Err(ResolveError::MalformedChecksums { .. })
        ));
    }

    #[test]
    fn a_line_whose_first_field_is_not_a_sha256_is_not_a_checksum() {
        let text = "\
deadbeef *short.img
0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe *good.img
";

        assert!(
            matches!(
                parse_sha256sums(text, "short.img", URL),
                Err(ResolveError::ImageNotListed { .. })
            ),
            "a truncated sum is not a sum, so that line names nothing"
        );
        assert!(parse_sha256sums(text, "good.img", URL).is_ok());
    }
}
```

- [ ] **Step 3: Запустить и убедиться, что тесты падают**

Run: `cargo test -p vmlord-image --lib checksums`
Expected: ошибка компиляции — `cannot find function parse_sha256sums`.

- [ ] **Step 4: Написать реализацию над тестами**

Вставить в `crates/image/src/checksums.rs` перед блоком `#[cfg(test)]`:

```rust
//! Reading a distribution's published list of checksums.

use crate::error::ResolveError;

/// The number of hex characters in a SHA256.
const CHECKSUM_LENGTH: usize = 64;

/// Finds the checksum `file_name` is published with. `url` names the file only
/// so failures can say which one.
///
/// Two failures are told apart on purpose. Nothing parsed at all means the body
/// is not a checksum list -- typically an HTML error page served with status
/// 200. Lines parsed but none of them ours means the distribution does not
/// publish that image. To the user these are different pieces of news, and
/// merging them would throw away the only useful half of the diagnosis.
pub(crate) fn parse_sha256sums(
    text: &str,
    file_name: &str,
    url: &str,
) -> Result<String, ResolveError> {
    let mut parsed = 0usize;
    let mut unreadable = 0usize;
    let mut found = None;

    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        match parse_line(line) {
            Some((checksum, name)) => {
                parsed += 1;
                if name == file_name && found.is_none() {
                    found = Some(checksum.to_ascii_lowercase());
                }
            }
            None => unreadable += 1,
        }
    }

    if parsed == 0 {
        return Err(ResolveError::MalformedChecksums {
            url: url.to_owned(),
        });
    }
    if unreadable > 0 {
        log::warn!("{unreadable} lines of {url} are not checksum entries and were skipped");
    }
    log::debug!("{url} lists {parsed} checksums");

    found.ok_or_else(|| ResolveError::ImageNotListed {
        file_name: file_name.to_owned(),
        url: url.to_owned(),
    })
}

/// Splits one line into its checksum and the name it belongs to.
///
/// The name is everything after the first run of whitespace, so a name with a
/// space in it survives; a leading `*` marks binary mode and is not part of the
/// name. The checksum must be 64 hex characters, which is what tells a checksum
/// line from a line of prose.
fn parse_line(line: &str) -> Option<(&str, &str)> {
    let (checksum, rest) = line.split_once(char::is_whitespace)?;
    if checksum.len() != CHECKSUM_LENGTH || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }

    let name = rest.trim_start();
    // `str::lines` leaves the carriage return of a CRLF file behind, and a name
    // ending in one matches nothing.
    let name = name.strip_prefix('*').unwrap_or(name).trim_end();
    (!name.is_empty()).then_some((checksum, name))
}
```

- [ ] **Step 5: Запустить тесты**

Run: `cargo test -p vmlord-image --lib checksums`
Expected: PASS, 9 тестов.

- [ ] **Step 6: Коммит**

```bash
git add crates/image/src/checksums.rs crates/image/src/lib.rs crates/image/tests/fixtures/
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-53: Read a checksum out of a distribution's published list"
```

---

### Task 3: Резолв релиза по сети

**Files:**
- Create: `crates/image/src/resolve.rs`, `crates/image/tests/resolve.rs`
- Modify: `crates/image/src/lib.rs`, `crates/image/tests/support/mod.rs`

**Interfaces:**
- Consumes: `DistroProfile`, `UBUNTU`, `validated_release` (Task 1); `parse_sha256sums` (Task 2); `build_agent` из `crate::http` (существует).
- Produces:
  - `pub struct ResolvedImage { pub url: String, pub sha256: String, pub default_user: &'static str, pub admin_group: &'static str }` с `Clone, Debug, PartialEq, Eq`
  - `pub fn resolve_image(profile: &DistroProfile, release: &str) -> Result<ResolvedImage, ResolveError>`
  - `pub fn base_url(&self) -> &str` у `TestServer` — URL сервера, оканчивающийся слэшем

- [ ] **Step 1: Добавить `base_url` в петлевой сервер**

В `crates/image/tests/support/mod.rs` добавить поле и метод. В `struct TestServer` — поле `base_url: String`; в `start`, рядом с построением `url`:

```rust
let base_url = format!("http://{}/", listener.local_addr().unwrap());
```

и вернуть `Self { url, base_url, ranges }`. Метод рядом с `url`:

```rust
/// The server's root, ending in a slash: the directory a profile points at.
///
/// The server answers every path the same way, so a resolver asking for
/// `<base>/SHA256SUMS` gets the body the test handed in.
pub fn base_url(&self) -> &str {
    &self.base_url
}
```

- [ ] **Step 2: Написать падающий интеграционный тест**

Создать `crates/image/tests/resolve.rs`:

```rust
//! Resolving a release against a server that behaves like Canonical's.

mod support;

use support::{Behaviour, TestServer};
use vmlord_image::{DistroProfile, ResolveError, UBUNTU, resolve_image};

const FIXTURE: &str = include_str!("fixtures/ubuntu-24.04-SHA256SUMS");

/// A profile pointing at the loopback server instead of the internet.
///
/// The profile is the seam: because the base URL is data, the test needs no
/// stubbed-out HTTP client, and the code under test is the same code that runs
/// in production.
///
/// The template is leaked because the port is only known at runtime while the
/// field is `&'static str`. A handful of leaked strings in a test binary that
/// exits seconds later is the cheaper end of the trade against making every
/// profile own its strings for the sake of the tests.
fn profile_for(server: &TestServer) -> DistroProfile {
    DistroProfile {
        directory_template: Box::leak(format!("{}{{release}}/", server.base_url()).into_boxed_str()),
        ..UBUNTU
    }
}

#[test]
fn a_release_resolves_to_the_image_url_and_the_checksum_published_beside_it() {
    let server = TestServer::start(FIXTURE.as_bytes().to_vec(), Behaviour::IgnoresRange);
    let profile = profile_for(&server);

    let resolved = resolve_image(&profile, "24.04").expect("the fixture lists this image");

    assert_eq!(
        resolved.url,
        format!(
            "{}24.04/ubuntu-24.04-server-cloudimg-amd64.img",
            server.base_url()
        )
    );
    assert_eq!(
        resolved.sha256,
        "0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe"
    );
    assert_eq!(resolved.default_user, "ubuntu");
    assert_eq!(resolved.admin_group, "sudo");
}

#[test]
fn a_server_without_that_release_is_reported_as_the_status_it_sent() {
    let server = TestServer::start(Vec::new(), Behaviour::NotFound);
    let profile = profile_for(&server);

    let error = resolve_image(&profile, "24.04").expect_err("404 is not a checksum list");

    assert!(
        matches!(error, ResolveError::UnexpectedStatus { status: 404 }),
        "got {error:?}"
    );
}

#[test]
fn a_release_of_the_wrong_shape_never_reaches_the_network() {
    let server = TestServer::start(FIXTURE.as_bytes().to_vec(), Behaviour::IgnoresRange);
    let profile = profile_for(&server);

    let error = resolve_image(&profile, "../../etc").expect_err("that is not a release");

    assert!(
        matches!(error, ResolveError::InvalidRelease(_)),
        "got {error:?}"
    );
    assert!(
        server.ranges_seen().is_empty(),
        "the request must be refused before a socket is opened"
    );
}
```

- [ ] **Step 3: Запустить и убедиться, что тест падает**

Run: `cargo test -p vmlord-image --test resolve`
Expected: ошибка компиляции — `unresolved import vmlord_image::resolve_image`.

- [ ] **Step 4: Написать `crates/image/src/resolve.rs`**

```rust
//! Turning a distribution release into a URL and the checksum to expect.

use crate::{
    checksums::parse_sha256sums,
    distro::{DistroProfile, validated_release},
    error::ResolveError,
    http::build_agent,
};

/// The largest checksum file that will be read into memory.
///
/// Without a ceiling a server is free to answer this request with a gigabyte,
/// straight into the worker thread's memory. The real file is under eight
/// kilobytes.
const MAX_CHECKSUMS_BYTES: u64 = 1024 * 1024;

/// Where to get a release's image, what it must hash to, and what the guest
/// inside it looks like.
///
/// `sha256` is lowercase hex, which is what `ImageDownloadRequest` expects, so
/// the caller feeds one into the other without a converter in between.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedImage {
    pub url: String,
    pub sha256: String,
    pub default_user: &'static str,
    pub admin_group: &'static str,
}

/// Works out which image a release means, by reading the checksum file the
/// distribution publishes beside it.
///
/// One request of a few kilobytes, so there is no progress reporting and no
/// cancellation flag here -- both belong to the download that follows. The list
/// is deliberately not cached: it is the thing that says what is current, and a
/// month-old copy would point at a build that has been withdrawn.
pub fn resolve_image(
    profile: &DistroProfile,
    release: &str,
) -> Result<ResolvedImage, ResolveError> {
    let release = validated_release(release).inspect_err(|error| log::error!("{error}"))?;
    let file_name = profile.file_name(release);
    let checksums_url = profile.checksums_url(release);

    log::debug!("looking up {file_name} in {checksums_url}");
    let published = fetch_text(&checksums_url).inspect_err(|error| log::error!("{error}"))?;
    let sha256 = parse_sha256sums(&published, &file_name, &checksums_url)
        .inspect_err(|error| log::error!("{error}"))?;

    let url = profile.image_url(release);
    log::info!("{} {release} resolves to {url} ({sha256})", profile.name);
    Ok(ResolvedImage {
        url,
        sha256,
        default_user: profile.default_user,
        admin_group: profile.admin_group,
    })
}

/// Fetches a small text file, refusing a body too large to be one.
fn fetch_text(url: &str) -> Result<String, ResolveError> {
    let agent = build_agent();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|source| ResolveError::Http(format!("requesting {url} failed: {source}")))?;

    let status = response.status().as_u16();
    if status != 200 {
        return Err(ResolveError::UnexpectedStatus { status });
    }

    response
        .body_mut()
        .with_config()
        .limit(MAX_CHECKSUMS_BYTES)
        .read_to_string()
        .map_err(|source| ResolveError::Http(format!("reading {url} failed: {source}")))
}
```

`build_agent` уже `pub(crate)` и уже настроен на `http_status_as_error(false)` — поэтому 404 приходит сюда как ответ со статусом, а не как ошибка транспорта, и превращается в `UnexpectedStatus`, а не в `Http`. Ничего в `http.rs` менять не нужно.

- [ ] **Step 5: Подключить модуль и экспорты в `crates/image/src/lib.rs`**

Добавить `mod resolve;` в алфавитном порядке и дописать к реэкспортам:

```rust
pub use resolve::{ResolvedImage, resolve_image};
```

- [ ] **Step 6: Запустить тесты**

Run: `cargo test -p vmlord-image`
Expected: PASS — три теста в `resolve`, плюс все ранее существовавшие.

- [ ] **Step 7: Коммит**

```bash
git add crates/image/src/resolve.rs crates/image/src/lib.rs crates/image/tests/resolve.rs crates/image/tests/support/mod.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-53: Resolve an Ubuntu release into an image and its checksum"
```

---

### Task 4: Документация и проверка сборки под Windows

**Files:**
- Modify: `ARCHITECTURE.md` (раздел «Image download», после абзаца про лок на `.part`), `crates/image/src/lib.rs` (шапка крейта)

**Interfaces:**
- Consumes: всё предыдущее.
- Produces: ничего кодового.

- [ ] **Step 1: Дописать в `ARCHITECTURE.md` подраздел после «Image download»**

```markdown
### Release resolution

`vmlord-image` also works out *which* image a release means. A `DistroProfile`
is a table of data -- two URL templates, the name of the checksum file, the
guest's default user and its admin group -- rather than a trait with one
implementation per distribution, because that is what actually differs between
Ubuntu and Fedora. `resolve_image` validates the release version, reads the
checksum file published beside the image, and returns the image URL together
with the SHA256 the download must produce, in the lowercase hex the downloader
expects.

Releases are addressed by version number rather than codename. The server
answers `/releases/24.04/` with a 302 to `/releases/noble/`, so a table of
codenames would need a line added for every future release and buy nothing. The
version string is checked against a strict shape before it is pasted into a URL:
it is attacker-influenced input, and unchecked it walks the request into another
directory. The architecture is baked into the file name template, since Hyper-V
here is x86_64.

A body that parses as no checksum line at all is reported apart from a body that
parses but does not list the image: the first means the server sent something
else -- typically an HTML error page with status 200 -- and the second means the
distribution publishes no such build. The checksum list is never cached: it is
what says which build is current.
```

- [ ] **Step 2: Обновить шапку `crates/image/src/lib.rs`**

Дописать к doc-комментарию крейта абзац:

```rust
//! Which image a release means is worked out here too: a `DistroProfile` says
//! where a distribution publishes its images, and `resolve_image` reads the
//! checksum file published beside one to learn what it must hash to.
```

- [ ] **Step 3: Прогнать полную проверку**

```bash
cargo test -p vmlord-image
cargo clippy --workspace --all-targets -- -D warnings
cargo build --target=x86_64-pc-windows-gnu
```

Expected: тесты зелёные, clippy молчит, сборка под Windows проходит. Ни один тест не открывает сокет наружу — сетевой тест ходит только на `127.0.0.1`.

- [ ] **Step 4: Коммит**

```bash
git add ARCHITECTURE.md crates/image/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-53: Document the release resolver"
```

---

## Проверка плана против спеки

| Требование спеки | Задача |
| --- | --- |
| `DistroProfile` как данные, константа `UBUNTU` | Task 1 |
| Два шаблона URL (каталог и имя файла) | Task 1 |
| Жёсткая проверка формы версии, отказ от кодовых имён | Task 1 |
| `ResolveError` отдельно от `DownloadError` | Task 1 |
| Разбор `SHA256SUMS`, звёздочка, сравнение имени целиком | Task 2 |
| Различение `MalformedChecksums` и `ImageNotListed` | Task 2 |
| Фикстура настоящего файла, обрезанная | Task 2 |
| `resolve_image`, `ResolvedImage` с гостевыми полями | Task 3 |
| Потолок тела 1 МиБ | Task 3 |
| Петлевой тест: успех и 404 | Task 3 |
| Логи DEBUG..ERROR | Task 1 (ERROR), Task 2 (DEBUG, WARN), Task 3 (DEBUG, INFO, ERROR) |
| Обновление `ARCHITECTURE.md` | Task 4 |
