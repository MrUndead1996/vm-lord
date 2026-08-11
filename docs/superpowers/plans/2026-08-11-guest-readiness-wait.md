# TASK-63: Guest readiness wait — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** создание VM становится полным циклом — собрать диск, зарегистрировать компьют-систему, стартовать VM и дождаться, пока cloud-init внутри гостя доложит о завершении, — и только после этого сборка снимается со списка.

**Architecture:** новый модуль `guest_ready` ждёт гостя в три фазы (адрес → порт 22 → `cloud-init status --wait`), каждая со своим таймаутом и своим исходом; транспорт — `ssh.exe` из состава Windows, запускаемый дочерним процессом за швом. Новый модуль `cycle` склеивает создание, старт и ожидание в одну операцию с правилами отката. Поток сборки возвращает `Com1Session` на главный поток через уже существующий `BuildRegistry::reap`.

**Tech Stack:** Rust 2024, `std` (`process::Command`, `net::TcpStream`, `time::Instant`), `windows` crate, никакого async-рантайма и никаких новых внешних зависимостей.

## Global Constraints

- Спека: `docs/superpowers/specs/2026-08-11-guest-readiness-wait-design.md`. Ветка: `task-63-guest-readiness`.
- Сборка проекта — `cargo build --target=x86_64-pc-windows-gnu`. Тесты — `cargo test -p vmlord-core` и `cargo test -p vmlord-platform`.
- Каждый коммит с префиксом `TASK-63: `, тело — как в примерах ниже, с `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
- Никаких новых зависимостей в `Cargo.toml`. Никакого async.
- Логирование через `log` на уровнях `DEBUG`..`ERROR`; `TRACE` не используется.
- Комментарии и doc-комментарии — на английском, как весь код проекта; они объясняют *почему*, а не *что*.
- `cargo fmt --all -- --check` и `cargo clippy --target=x86_64-pc-windows-gnu --all-targets -- -D warnings` должны проходить перед каждым коммитом.
- Таймауты по умолчанию: адрес 90 с, порт 22 — 300 с, `cloud-init status --wait` — 1200 с, одно соединение ssh — 10 с.

## File Structure

| Файл | Ответственность |
|---|---|
| `crates/core/src/settings.rs` (изменить) | `GuestReadinessTimeouts` в `AppSettings`, группа `[guest_readiness]` в `settings.toml` |
| `crates/core/src/progress.rs` (изменить) | шаги `BuildStep::Starting` и `BuildStep::AwaitingGuest` |
| `crates/ui/src/lib.rs` (изменить) | подписи новых шагов в списке VM |
| `crates/platform/src/layout.rs` (изменить) | путь `cloud-init-status.log`, чтение хвоста `com1.log` |
| `crates/platform/src/guest_ready.rs` (создать) | ожидание готовности: разбор исходов, три фазы, швы, production-реализации |
| `crates/platform/src/event.rs` (изменить) | `unsafe impl Send for WindowsEvent` |
| `crates/platform/src/start.rs`, `force_stop.rs`, `delete.rs`, `cleanup.rs` (изменить) | `Send + Sync` на швах пайплайнов |
| `crates/platform/src/build.rs` (изменить) | слот исхода у сборки и `take_started()` |
| `crates/platform/src/cycle.rs` (создать) | оркестрация создание → старт → ожидание и правила отката |
| `crates/platform/src/repository.rs` (изменить) | владение циклом, приём `Com1Session` из reap, диагностики |
| `crates/vmlord/src/main.rs` (изменить) | передача таймаутов из настроек в репозиторий |
| `crates/platform/tests/hyperv.rs` (изменить) | `#[ignore]`-тест на живой VM |
| `ARCHITECTURE.md` (изменить) | описание полного цикла создания |

---

### Task 1: Таймауты в настройках

**Files:**
- Modify: `crates/core/src/settings.rs`
- Test: `crates/core/src/settings.rs` (модуль `tests` в конце файла)

**Interfaces:**
- Produces: `vmlord_core::GuestReadinessTimeouts { address_secs: u64, ssh_port_secs: u64, cloud_init_secs: u64, connect_timeout_secs: u64 }`, реализует `Default`; поле `AppSettings::guest_readiness: GuestReadinessTimeouts`.

**Важно:** поле `guest_readiness` должно стоять **последним** в `AppSettings`. TOML требует, чтобы все скалярные значения были записаны до таблиц; поле-структура сериализуется в таблицу `[guest_readiness]`, и любое скалярное поле после неё сломает `save`.

- [ ] **Step 1: Написать падающий тест**

Добавить в `mod tests` файла `crates/core/src/settings.rs`:

```rust
#[test]
fn readiness_timeouts_have_defaults_and_survive_a_round_trip() {
    let defaults = GuestReadinessTimeouts::default();

    assert_eq!(defaults.address_secs, 90);
    assert_eq!(defaults.ssh_port_secs, 300);
    assert_eq!(defaults.cloud_init_secs, 1200);
    assert_eq!(defaults.connect_timeout_secs, 10);

    let directory = std::env::temp_dir().join(format!(
        "vmlord-settings-readiness-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let store = SettingsStore::new(directory.join("settings.toml"));
    let mut settings = store.load_or_create().unwrap();
    settings.guest_readiness.cloud_init_secs = 60;
    store.save(&settings).unwrap();

    assert_eq!(store.load_or_create().unwrap().guest_readiness.cloud_init_secs, 60);
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_settings_file_written_before_the_timeouts_existed_still_loads() {
    // `#[serde(default)]`, как у `image_cache_path`: старый файл читается без
    // миграции и получает дефолтные таймауты.
    let directory = std::env::temp_dir().join(format!(
        "vmlord-settings-legacy-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("settings.toml");
    std::fs::write(
        &path,
        "vm_storage_path = \"C:\\\\VMs\"\n\
         language = \"en-US\"\n\
         log_file_path = \"C:\\\\VMs\\\\vmlord.log\"\n\
         log_level = \"info\"\n",
    )
    .unwrap();

    let settings = SettingsStore::new(&path).load_or_create().unwrap();

    assert_eq!(settings.guest_readiness, GuestReadinessTimeouts::default());
    std::fs::remove_dir_all(&directory).ok();
}
```

Добавить `GuestReadinessTimeouts` в `use super::{...}` этого модуля.

- [ ] **Step 2: Убедиться, что тест падает**

Run: `cargo test -p vmlord-core settings::tests::readiness_timeouts_have_defaults_and_survive_a_round_trip`
Expected: FAIL, компиляция не проходит — `cannot find type GuestReadinessTimeouts`.

- [ ] **Step 3: Реализовать**

В `crates/core/src/settings.rs` добавить после `LogLevel`:

```rust
/// How long each phase of waiting for a freshly created guest may take.
///
/// Settings rather than constants because the numbers are about the user's
/// network and hardware, not about VMLord: the first boot of a cloud image
/// installs the packages the seed asked for, and ten minutes of that is
/// ordinary on a slow link.
///
/// `#[serde(default)]` on the struct fills in a field a `settings.toml`
/// written before it existed does not have, and the attribute on the field in
/// `AppSettings` fills in the whole group -- the same treatment
/// `image_cache_path` gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuestReadinessTimeouts {
    /// Waiting for HNS to give the VM's endpoint an address.
    pub address_secs: u64,
    /// Waiting for port 22 to open once the address exists.
    pub ssh_port_secs: u64,
    /// Waiting for `cloud-init status --wait` to return.
    pub cloud_init_secs: u64,
    /// One SSH connection attempt (`ConnectTimeout`).
    pub connect_timeout_secs: u64,
}

impl Default for GuestReadinessTimeouts {
    fn default() -> Self {
        Self {
            address_secs: 90,
            ssh_port_secs: 300,
            cloud_init_secs: 1200,
            connect_timeout_secs: 10,
        }
    }
}
```

В `AppSettings` добавить поле последним:

```rust
    /// Timeouts for the readiness wait that ends a VM's creation.
    ///
    /// Last in the struct on purpose: TOML demands that every value precede
    /// every table, and this field is a table.
    #[serde(default)]
    pub guest_readiness: GuestReadinessTimeouts,
```

В `default_settings` добавить `guest_readiness: GuestReadinessTimeouts::default(),`, а в двух местах модуля `tests`, где `AppSettings` конструируется целиком (строки около 331 и 351), — то же поле. В `crates/core/src/lib.rs` расширить реэкспорт: `pub use settings::{AppSettings, GuestReadinessTimeouts, Language, LogLevel, SettingsError, SettingsStore};`.

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-core settings::`
Expected: PASS.

- [ ] **Step 5: Проверить, что остальное не сломалось, и закоммитить**

Run:

```bash
cargo test -p vmlord-core
cargo fmt --all -- --check
```

```bash
git add crates/core/src/settings.rs crates/core/src/lib.rs
git commit -m "TASK-63: Add guest readiness timeouts to settings"
```

---

### Task 2: Шаги сборки `Starting` и `AwaitingGuest`

**Files:**
- Modify: `crates/core/src/progress.rs:148-158`
- Modify: `crates/ui/src/lib.rs:1757-1760`
- Test: `crates/ui/src/lib.rs` (модуль `tests`)

**Interfaces:**
- Consumes: ничего.
- Produces: `BuildStep::Starting`, `BuildStep::AwaitingGuest` — используются в задачах 9 и 10.

- [ ] **Step 1: Написать падающий тест**

В `mod tests` файла `crates/ui/src/lib.rs` добавить (рядом с существующими тестами про подписи шагов):

```rust
#[test]
fn the_new_build_steps_have_labels_of_their_own() {
    // Шаги после регистрации: VM уже создана, но сборка не окончена, пока
    // гость не доложил о готовности.
    assert_eq!(
        build_step_label(BuildStep::Starting),
        "Building: starting the VM"
    );
    assert_eq!(
        build_step_label(BuildStep::AwaitingGuest),
        "Building: waiting for the guest"
    );
}
```

Если функция, содержащая `match` из строк 1757-1760, называется иначе или является внутренней частью большего выражения, вынести этот `match` в свободную функцию `fn build_step_label(step: BuildStep) -> &'static str` в том же файле и вызвать её из прежнего места — тест обращается именно к ней.

- [ ] **Step 2: Убедиться, что тест падает**

Run: `cargo test -p vmlord-ui the_new_build_steps_have_labels_of_their_own`
Expected: FAIL — `no variant named Starting found for enum BuildStep`.

- [ ] **Step 3: Реализовать**

В `crates/core/src/progress.rs` в `enum BuildStep` добавить после `Registering`:

```rust
    /// Starting the VM that has just been created. Creation does not end at a
    /// registered compute system: a VM nobody has ever started is not known to
    /// work.
    Starting,
    /// Waiting for the guest's cloud-init to report that it has finished. The
    /// last step, and usually the longest one: the first boot installs the
    /// packages the seed asked for.
    AwaitingGuest,
```

В `crates/ui/src/lib.rs` дополнить `match`:

```rust
            BuildStep::Starting => "Building: starting the VM",
            BuildStep::AwaitingGuest => "Building: waiting for the guest",
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run:

```bash
cargo test -p vmlord-ui the_new_build_steps_have_labels_of_their_own
cargo test -p vmlord-core progress::
```

Expected: PASS.

- [ ] **Step 5: Закоммитить**

```bash
git add crates/core/src/progress.rs crates/ui/src/lib.rs
git commit -m "TASK-63: Add the starting and awaiting-guest build steps"
```

---

### Task 3: Разбор исхода ssh и хвост текста

**Files:**
- Create: `crates/platform/src/guest_ready.rs`
- Modify: `crates/platform/src/lib.rs` (объявить модуль)
- Test: `crates/platform/src/guest_ready.rs` (модуль `tests`)

**Interfaces:**
- Produces:
  - `pub(crate) enum GuestReady { Ready, Degraded { detail: String } }`
  - `pub(crate) enum ReadinessFailure { NoSshClient, NoAddress, Unreachable { last_error: String }, CloudInitFailed { detail: String }, TimedOut, Cancelled }` с `impl std::fmt::Display`
  - `pub(crate) fn outcome(exit_code: Option<i32>, transcript_tail: &str) -> Result<GuestReady, ReadinessFailure>`
  - `pub(crate) fn tail(text: &str, lines: usize) -> String`

- [ ] **Step 1: Написать падающие тесты**

Создать `crates/platform/src/guest_ready.rs` с заголовком модуля и тестами:

```rust
//! Waiting until a freshly created guest is actually ready to be used.

#[cfg(test)]
mod tests {
    use super::{GuestReady, ReadinessFailure, outcome, tail};

    #[test]
    fn a_zero_exit_means_the_guest_is_ready() {
        assert!(matches!(outcome(Some(0), "status: done"), Ok(GuestReady::Ready)));
    }

    #[test]
    fn a_two_exit_means_ready_but_degraded_and_keeps_the_detail() {
        // cloud-init returns 2 when a module failed while the system still came
        // up. One broken module must not turn a working VM into a failed build.
        let Ok(GuestReady::Degraded { detail }) =
            outcome(Some(2), "status: degraded done\nerror: cc_package_update failed")
        else {
            panic!("exit code 2 must report a degraded but ready guest");
        };

        assert!(detail.contains("cc_package_update"), "{detail}");
    }

    #[test]
    fn a_one_exit_is_a_cloud_init_failure_and_keeps_the_detail() {
        let Err(ReadinessFailure::CloudInitFailed { detail }) =
            outcome(Some(1), "status: error\nerror: no such file")
        else {
            panic!("exit code 1 must report a cloud-init failure");
        };

        assert!(detail.contains("no such file"), "{detail}");
    }

    #[test]
    fn exit_code_255_is_ssh_itself_failing_not_cloud_init() {
        // 255 is OpenSSH's own code: it never reached the command, so nothing
        // is known about cloud-init.
        let Err(ReadinessFailure::Unreachable { last_error }) =
            outcome(Some(255), "Permission denied (publickey).")
        else {
            panic!("exit code 255 must report an unreachable guest");
        };

        assert!(last_error.contains("Permission denied"), "{last_error}");
    }

    #[test]
    fn no_exit_code_means_the_child_was_killed_at_its_deadline() {
        assert!(matches!(outcome(None, ""), Err(ReadinessFailure::TimedOut)));
    }

    #[test]
    fn an_unknown_exit_code_names_itself_rather_than_being_swallowed() {
        let Err(ReadinessFailure::CloudInitFailed { detail }) = outcome(Some(42), "") else {
            panic!("an unrecognised exit code must still fail the wait");
        };

        assert!(detail.contains("42"), "{detail}");
    }

    #[test]
    fn the_tail_keeps_the_last_lines_and_nothing_before_them() {
        let text = (1..=10).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");

        let kept = tail(&text, 3);

        assert_eq!(kept, "line 8\nline 9\nline 10");
    }

    #[test]
    fn a_text_shorter_than_the_tail_is_kept_whole_and_trimmed() {
        assert_eq!(tail("only one\n", 40), "only one");
    }

    #[test]
    fn every_failure_names_its_cause_rather_than_a_code() {
        // The style `error.rs` sets: the message says what happened, not which
        // number happened.
        let messages = [
            ReadinessFailure::NoSshClient.to_string(),
            ReadinessFailure::NoAddress.to_string(),
            ReadinessFailure::Unreachable { last_error: "refused".into() }.to_string(),
            ReadinessFailure::CloudInitFailed { detail: "boom".into() }.to_string(),
            ReadinessFailure::TimedOut.to_string(),
            ReadinessFailure::Cancelled.to_string(),
        ];

        assert!(messages[0].contains("OpenSSH"), "{}", messages[0]);
        assert!(messages[1].contains("address"), "{}", messages[1]);
        assert!(messages[2].contains("refused"), "{}", messages[2]);
        assert!(messages[3].contains("boom"), "{}", messages[3]);
        assert!(messages[4].contains("did not finish"), "{}", messages[4]);
        assert!(messages[5].contains("cancelled"), "{}", messages[5]);
    }
}
```

В `crates/platform/src/lib.rs` добавить `mod guest_ready;` рядом с остальными объявлениями модулей (в алфавитном порядке, после `force_stop`).

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-platform guest_ready::`
Expected: FAIL — `cannot find function outcome in this scope`.

- [ ] **Step 3: Реализовать**

Дописать в `crates/platform/src/guest_ready.rs` перед модулем тестов:

```rust
use std::fmt;

/// How many lines of a transcript or console log are worth carrying into a
/// diagnostic. Enough for the failing unit and its context, short enough to
/// read in a message box.
pub(crate) const DIAGNOSTIC_TAIL_LINES: usize = 40;

/// A guest that has finished booting, with or without complaints.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GuestReady {
    Ready,
    /// cloud-init finished, but some module of it failed. The guest is usable;
    /// the detail says what is missing from it.
    Degraded { detail: String },
}

/// Every way waiting for a guest can end badly, each naming its own cause.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadinessFailure {
    NoSshClient,
    NoAddress,
    Unreachable { last_error: String },
    CloudInitFailed { detail: String },
    TimedOut,
    Cancelled,
}

impl fmt::Display for ReadinessFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSshClient => write!(
                formatter,
                "Windows has no OpenSSH client at \
                 %SystemRoot%\\System32\\OpenSSH\\ssh.exe; install the \
                 \"OpenSSH Client\" optional feature"
            ),
            Self::NoAddress => write!(
                formatter,
                "the VM started but was never given an address on the VMLord network"
            ),
            Self::Unreachable { last_error } => write!(
                formatter,
                "the guest did not accept an SSH connection: {last_error}"
            ),
            Self::CloudInitFailed { detail } => {
                write!(formatter, "cloud-init failed inside the guest: {detail}")
            }
            Self::TimedOut => write!(
                formatter,
                "cloud-init did not finish configuring the guest in time"
            ),
            Self::Cancelled => write!(formatter, "waiting for the guest was cancelled"),
        }
    }
}

/// Turns what `ssh.exe` left behind into what is known about the guest.
///
/// `cloud-init status --wait` answers `0` when it is done, `1` when it failed
/// and `2` when it finished degraded. `255` is OpenSSH's own code, meaning the
/// command was never run, so it says nothing about cloud-init and everything
/// about the connection. No code at all means the child was killed -- which is
/// what this module does at a deadline.
pub(crate) fn outcome(
    exit_code: Option<i32>,
    transcript_tail: &str,
) -> Result<GuestReady, ReadinessFailure> {
    let detail = || {
        let text = transcript_tail.trim();
        if text.is_empty() {
            "no output".to_owned()
        } else {
            text.to_owned()
        }
    };

    match exit_code {
        Some(0) => Ok(GuestReady::Ready),
        Some(2) => Ok(GuestReady::Degraded { detail: detail() }),
        Some(1) => Err(ReadinessFailure::CloudInitFailed { detail: detail() }),
        Some(255) => Err(ReadinessFailure::Unreachable {
            last_error: detail(),
        }),
        Some(other) => Err(ReadinessFailure::CloudInitFailed {
            detail: format!("the SSH command exited with code {other}: {}", detail()),
        }),
        None => Err(ReadinessFailure::TimedOut),
    }
}

/// The last `lines` lines of `text`, trimmed.
pub(crate) fn tail(text: &str, lines: usize) -> String {
    let kept: Vec<&str> = text.trim_end().lines().collect();
    let start = kept.len().saturating_sub(lines);
    kept[start..].join("\n").trim().to_owned()
}
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-platform guest_ready::`
Expected: PASS, восемь тестов.

- [ ] **Step 5: Закоммитить**

```bash
cargo fmt --all -- --check
git add crates/platform/src/guest_ready.rs crates/platform/src/lib.rs
git commit -m "TASK-63: Read a guest readiness outcome from an ssh exit code"
```

---

### Task 4: Вызов `ssh.exe` и его отсутствие

**Files:**
- Modify: `crates/platform/src/guest_ready.rs`
- Modify: `crates/platform/src/layout.rs`
- Test: оба файла, модули `tests`

**Interfaces:**
- Consumes: `ReadinessFailure` из задачи 3.
- Produces:
  - `pub(crate) struct SshInvocation { pub(crate) program: PathBuf, pub(crate) args: Vec<OsString>, pub(crate) transcript: PathBuf }`
  - `pub(crate) fn ssh_invocation(client: &Path, vm_directory: &Path, username: &str, ip: IpAddr, connect_timeout: Duration) -> SshInvocation`
  - `pub(crate) fn ssh_client_path() -> Option<PathBuf>`
  - `layout::cloud_init_status_log_path(vm_directory: &Path) -> PathBuf`

- [ ] **Step 1: Написать падающие тесты**

В `crates/platform/src/layout.rs`, в `mod tests`:

```rust
    #[test]
    fn the_readiness_transcript_lives_beside_the_serial_log() {
        // Both are records of what the VM did rather than parts of what it is,
        // so they sit beside `config.json` rather than under `disks/`.
        let directory = vm_directory(Path::new("/vms"), "dev-linux").unwrap();

        assert_eq!(
            cloud_init_status_log_path(&directory),
            PathBuf::from("/vms")
                .join("dev-linux")
                .join("cloud-init-status.log")
        );
    }
```

Дописать `cloud_init_status_log_path` в `use super::{...}` этого модуля.

В `crates/platform/src/guest_ready.rs`, в `mod tests`:

```rust
    use std::{ffi::OsString, net::IpAddr, path::Path, time::Duration};

    use super::{ssh_invocation, SshInvocation};

    fn arguments(invocation: &SshInvocation) -> Vec<String> {
        invocation
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_ssh_invocation_can_neither_prompt_nor_touch_the_users_known_hosts() {
        let invocation = ssh_invocation(
            Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe"),
            Path::new(r"C:\VMs\dev-linux"),
            "machi",
            "172.22.42.7".parse::<IpAddr>().unwrap(),
            Duration::from_secs(10),
        );
        let args = arguments(&invocation);

        assert_eq!(
            invocation.program,
            Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe")
        );
        // No prompt can hang a build, and no key of the user's agent can be
        // tried in place of the VM's own.
        assert!(args.contains(&"BatchMode=yes".to_owned()), "{args:?}");
        assert!(args.contains(&"IdentitiesOnly=yes".to_owned()), "{args:?}");
        assert!(
            args.contains(&"StrictHostKeyChecking=accept-new".to_owned()),
            "{args:?}"
        );
        assert!(
            args.contains(&r"UserKnownHostsFile=C:\VMs\dev-linux\known_hosts".to_owned()),
            "the VM's host key must not be written into the user's own known_hosts: {args:?}"
        );
        assert!(args.contains(&"ConnectTimeout=10".to_owned()), "{args:?}");
        assert!(
            args.contains(&r"C:\VMs\dev-linux\keys\id_ed25519".to_owned()),
            "{args:?}"
        );
        assert!(args.contains(&"machi".to_owned()), "{args:?}");
        assert!(args.contains(&"172.22.42.7".to_owned()), "{args:?}");
        assert_eq!(
            args.last().map(String::as_str),
            Some("cloud-init status --wait --long"),
            "{args:?}"
        );
        assert_eq!(
            invocation.transcript,
            Path::new(r"C:\VMs\dev-linux\cloud-init-status.log")
        );
    }

    #[test]
    fn the_ip_and_the_user_are_separate_arguments_not_a_joined_string() {
        // `-l user host` rather than `user@host`: a username that contains an
        // `@` would otherwise be split in the wrong place by ssh.
        let invocation = ssh_invocation(
            Path::new("ssh.exe"),
            Path::new(r"C:\VMs\dev"),
            "a@b",
            "10.0.0.2".parse::<IpAddr>().unwrap(),
            Duration::from_secs(5),
        );
        let args = arguments(&invocation);
        let user_flag = args.iter().position(|argument| argument == "-l").unwrap();

        assert_eq!(args[user_flag + 1], "a@b");
        assert_eq!(args[user_flag + 2], "10.0.0.2");
    }
```

- [ ] **Step 2: Убедиться, что тесты падают**

Run:

```bash
cargo test -p vmlord-platform layout::tests::the_readiness_transcript_lives_beside_the_serial_log
cargo test -p vmlord-platform guest_ready::tests::the_ssh_invocation
```

Expected: FAIL — функций не существует.

- [ ] **Step 3: Реализовать**

В `crates/platform/src/layout.rs` добавить рядом с `com1_log_path`:

```rust
/// The name of the transcript `cloud-init status --wait` writes through.
pub(crate) const CLOUD_INIT_STATUS_LOG_FILE_NAME: &str = "cloud-init-status.log";

/// Returns the path of the transcript the readiness wait captures.
///
/// Beside `com1.log`, and for the same reason: it records what the VM did, not
/// what it is made of. Its existence also means the wait got as far as running
/// something in the guest, which the serial log alone does not say.
pub(crate) fn cloud_init_status_log_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join(CLOUD_INIT_STATUS_LOG_FILE_NAME)
}
```

В `crates/platform/src/guest_ready.rs` добавить:

```rust
use std::{
    ffi::OsString,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::layout;

/// Where Windows keeps its OpenSSH client.
const SSH_CLIENT_RELATIVE_PATH: &str = r"System32\OpenSSH\ssh.exe";

/// What the guest is asked, once a connection to it is possible.
///
/// `--wait` blocks until cloud-init is done; `--long` makes the answer say
/// which module failed rather than only that one did.
const READINESS_COMMAND: &str = "cloud-init status --wait --long";

/// Everything needed to run one readiness command, decided without running it.
///
/// Separate from running it so that the decisions -- which key, which
/// known-hosts file, which timeout -- are testable without a guest, a network,
/// or an `ssh.exe`.
pub(crate) struct SshInvocation {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    /// Where the child's output goes. A file rather than a pipe: `--wait`
    /// prints a dot a second for as long as twenty minutes, and a pipe nobody
    /// drains fills up and deadlocks the child against a parent that is
    /// polling it.
    pub(crate) transcript: PathBuf,
}

/// Windows' own OpenSSH client, if this installation has one.
///
/// Optional Windows features can be removed, so its absence is a state to
/// report rather than a panic: `ReadinessFailure::NoSshClient` names the
/// feature a person has to install.
pub(crate) fn ssh_client_path() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot")?;
    let path = PathBuf::from(root).join(SSH_CLIENT_RELATIVE_PATH);
    path.is_file().then_some(path)
}

/// Builds the command that asks the guest whether cloud-init has finished.
pub(crate) fn ssh_invocation(
    client: &Path,
    vm_directory: &Path,
    username: &str,
    ip: IpAddr,
    connect_timeout: Duration,
) -> SshInvocation {
    let option = |text: String| [OsString::from("-o"), OsString::from(text)];
    let mut args = vec![
        OsString::from("-i"),
        layout::ssh_key_path(vm_directory).into_os_string(),
    ];
    args.extend(option("BatchMode=yes".to_owned()));
    args.extend(option("IdentitiesOnly=yes".to_owned()));
    args.extend(option("StrictHostKeyChecking=accept-new".to_owned()));
    args.extend(option(format!(
        "UserKnownHostsFile={}",
        vm_directory.join("known_hosts").display()
    )));
    args.extend(option(format!(
        "ConnectTimeout={}",
        connect_timeout.as_secs()
    )));
    args.push(OsString::from("-l"));
    args.push(OsString::from(username));
    args.push(OsString::from(ip.to_string()));
    args.push(OsString::from(READINESS_COMMAND));

    SshInvocation {
        program: client.to_path_buf(),
        args,
        transcript: layout::cloud_init_status_log_path(vm_directory),
    }
}
```

`layout::ssh_key_path` уже существует; сделать его видимым для `guest_ready`, если он `pub(crate)` — он уже такой.

- [ ] **Step 4: Убедиться, что тесты проходят**

Run:

```bash
cargo test -p vmlord-platform layout::tests
cargo test -p vmlord-platform guest_ready::
```

Expected: PASS.

- [ ] **Step 5: Закоммитить**

```bash
cargo fmt --all -- --check
git add crates/platform/src/guest_ready.rs crates/platform/src/layout.rs
git commit -m "TASK-63: Build the ssh invocation the readiness wait runs"
```

---

### Task 5: Три фазы ожидания на швах

**Files:**
- Modify: `crates/platform/src/guest_ready.rs`
- Test: там же

**Interfaces:**
- Consumes: `GuestReady`, `ReadinessFailure`, `SshInvocation`, `outcome` из задач 3-4.
- Produces:
  - `pub(crate) struct ReadinessTimeouts { pub(crate) address: Duration, pub(crate) ssh_port: Duration, pub(crate) cloud_init: Duration, pub(crate) connect: Duration }` с `From<GuestReadinessTimeouts>` и `Default`
  - `pub(crate) enum SshRun { Exited { code: Option<i32>, transcript_tail: String }, Cancelled }`
  - `pub(crate) struct GuestReadiness` c `production(timeouts)`, `for_test(...)` и
    `wait(&self, mapping: &VmComputeSystemMapping, vm_directory: &Path, username: &str, monitor: &BuildMonitor) -> Result<GuestReady, ReadinessFailure>`

- [ ] **Step 1: Написать падающие тесты**

В `mod tests` файла `guest_ready.rs`:

```rust
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    };

    use uuid::Uuid;
    use vmlord_core::{BuildMonitor, BuildStep, NetworkMode};

    use super::{GuestReadiness, ReadinessTimeouts, SshRun};
    use crate::metadata::VmComputeSystemMapping;

    fn mapping() -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: Uuid::nil(),
            vm_name: "dev-linux".to_owned(),
            hcs_compute_system_id: "vmlord-test".to_owned(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: NetworkMode::Nat,
        }
    }

    fn monitor() -> BuildMonitor {
        BuildMonitor::new(BuildStep::AwaitingGuest)
    }

    /// A clock that only moves when something sleeps on it, so a twenty-minute
    /// timeout costs a test no time at all.
    #[derive(Clone, Default)]
    struct FakeClock(Arc<AtomicU64>);

    impl FakeClock {
        fn elapsed(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::Relaxed))
        }

        fn sleeper(&self) -> impl Fn(Duration) + Send + Sync + use<> {
            let millis = Arc::clone(&self.0);
            move |duration: Duration| {
                millis.fetch_add(duration.as_millis() as u64, Ordering::Relaxed);
            }
        }
    }

    fn timeouts() -> ReadinessTimeouts {
        ReadinessTimeouts {
            address: Duration::from_secs(90),
            ssh_port: Duration::from_secs(300),
            cloud_init: Duration::from_secs(1200),
            connect: Duration::from_secs(10),
        }
    }

    #[test]
    fn a_guest_that_answers_at_once_is_ready() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some("10.0.0.2".parse().unwrap())),
            |_, _| Ok(()),
            |_, _, _| {
                Ok(SshRun::Exited {
                    code: Some(0),
                    transcript_tail: "status: done".to_owned(),
                })
            },
            {
                let clock = clock.clone();
                move || clock.elapsed()
            },
            clock.sleeper(),
        );

        let ready = readiness
            .wait(&mapping(), Path::new(r"C:\VMs\dev-linux"), "machi", &monitor())
            .unwrap();

        assert_eq!(ready, GuestReady::Ready);
    }

    #[test]
    fn an_address_that_never_arrives_fails_on_its_own_timeout() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(None),
            |_, _| panic!("the port must not be probed before an address exists"),
            |_, _, _| panic!("ssh must not run before an address exists"),
            {
                let clock = clock.clone();
                move || clock.elapsed()
            },
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), Path::new(r"C:\VMs\dev-linux"), "machi", &monitor())
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::NoAddress);
        // The address timeout and no more: the later phases have their own.
        assert!(clock.elapsed() >= Duration::from_secs(90), "{:?}", clock.elapsed());
        assert!(clock.elapsed() < Duration::from_secs(120), "{:?}", clock.elapsed());
    }

    #[test]
    fn a_port_that_never_opens_reports_the_last_connection_error() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some("10.0.0.2".parse().unwrap())),
            |_, _| Err("connection refused".to_owned()),
            |_, _, _| panic!("ssh must not run before port 22 opens"),
            {
                let clock = clock.clone();
                move || clock.elapsed()
            },
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), Path::new(r"C:\VMs\dev-linux"), "machi", &monitor())
            .unwrap_err();

        assert_eq!(
            failure,
            ReadinessFailure::Unreachable {
                last_error: "connection refused".to_owned()
            }
        );
        assert!(clock.elapsed() >= Duration::from_secs(300), "{:?}", clock.elapsed());
    }

    #[test]
    fn a_cancelled_build_stops_the_wait_in_every_phase() {
        for phase in ["address", "port", "ssh"] {
            let clock = FakeClock::default();
            let monitor = monitor();
            let cancelling = {
                let monitor = monitor.clone();
                move || monitor.cancel()
            };
            let readiness = GuestReadiness::for_test(
                timeouts(),
                {
                    let cancelling = cancelling.clone();
                    move |_| {
                        if phase == "address" {
                            cancelling();
                            return Ok(None);
                        }
                        Ok(Some("10.0.0.2".parse().unwrap()))
                    }
                },
                {
                    let cancelling = cancelling.clone();
                    move |_, _| {
                        if phase == "port" {
                            cancelling();
                            return Err("not yet".to_owned());
                        }
                        Ok(())
                    }
                },
                move |_, _, _| {
                    assert_eq!(phase, "ssh", "ssh ran in the {phase} phase");
                    Ok(SshRun::Cancelled)
                },
                {
                    let clock = clock.clone();
                    move || clock.elapsed()
                },
                clock.sleeper(),
            );

            let failure = readiness
                .wait(&mapping(), Path::new(r"C:\VMs\dev-linux"), "machi", &monitor)
                .unwrap_err();

            assert_eq!(failure, ReadinessFailure::Cancelled, "phase {phase}");
        }
    }

    #[test]
    fn a_killed_ssh_child_is_a_timeout_not_a_cloud_init_failure() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some("10.0.0.2".parse().unwrap())),
            |_, _| Ok(()),
            |_, _, _| {
                Ok(SshRun::Exited {
                    code: None,
                    transcript_tail: "....".to_owned(),
                })
            },
            {
                let clock = clock.clone();
                move || clock.elapsed()
            },
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), Path::new(r"C:\VMs\dev-linux"), "machi", &monitor())
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::TimedOut);
    }

    #[test]
    fn an_address_lookup_that_fails_outright_is_reported_as_no_address() {
        // HNS refusing to answer is not the same as an address that has not
        // arrived yet, but from the guest's side it is the same outcome, and
        // the log has already recorded the underlying error.
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Err(RepositoryError::new("HNS is unavailable")),
            |_, _| Ok(()),
            |_, _, _| panic!("ssh must not run without an address"),
            {
                let clock = clock.clone();
                move || clock.elapsed()
            },
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), Path::new(r"C:\VMs\dev-linux"), "machi", &monitor())
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::NoAddress);
    }

    #[test]
    fn a_missing_ssh_client_is_reported_before_anything_is_waited_for() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test_without_client(timeouts(), {
            let clock = clock.clone();
            move || clock.elapsed()
        });

        let failure = readiness
            .wait(&mapping(), Path::new(r"C:\VMs\dev-linux"), "machi", &monitor())
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::NoSshClient);
        assert_eq!(clock.elapsed(), Duration::ZERO, "nothing may be waited for");
    }
```

Дополнить `use super::{...}` именами `GuestReady`, `ReadinessFailure`, `RepositoryError` (последний — из `vmlord_core`).

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-platform guest_ready::`
Expected: FAIL — `GuestReadiness` не существует.

- [ ] **Step 3: Реализовать**

Дописать в `guest_ready.rs`:

```rust
use std::net::SocketAddr;

use vmlord_core::{BuildMonitor, GuestReadinessTimeouts, RepositoryError};

use crate::metadata::VmComputeSystemMapping;

/// How often an unfinished phase looks again.
///
/// Two seconds rather than a tighter loop: nothing here becomes true faster
/// than a guest boots, and every poll of the address costs an HNS call.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The port a Linux guest answers SSH on. Fixed, because the seed does not
/// move it.
const SSH_PORT: u16 = 22;

/// The timeouts of a wait, as durations rather than as settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadinessTimeouts {
    pub(crate) address: Duration,
    pub(crate) ssh_port: Duration,
    pub(crate) cloud_init: Duration,
    pub(crate) connect: Duration,
}

impl From<GuestReadinessTimeouts> for ReadinessTimeouts {
    fn from(settings: GuestReadinessTimeouts) -> Self {
        Self {
            address: Duration::from_secs(settings.address_secs),
            ssh_port: Duration::from_secs(settings.ssh_port_secs),
            cloud_init: Duration::from_secs(settings.cloud_init_secs),
            connect: Duration::from_secs(settings.connect_timeout_secs),
        }
    }
}

impl Default for ReadinessTimeouts {
    fn default() -> Self {
        GuestReadinessTimeouts::default().into()
    }
}

/// How a run of `ssh.exe` ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SshRun {
    Exited {
        code: Option<i32>,
        transcript_tail: String,
    },
    /// The build was cancelled while the command was running, and the child was
    /// killed for it. Distinct from a deadline, which is an `Exited` with no
    /// code, because the two mean different things to the caller.
    Cancelled,
}

type AddressSource =
    Box<dyn Fn(&VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError> + Send + Sync>;
/// Answers `Err(reason)` while the port is not open yet; the reason is what the
/// failure reports if the port never opens.
type PortProbe = Box<dyn Fn(IpAddr, Duration) -> Result<(), String> + Send + Sync>;
type SshRunner = Box<
    dyn Fn(&SshInvocation, Duration, &BuildMonitor) -> Result<SshRun, RepositoryError>
        + Send
        + Sync,
>;
/// Elapsed time since the wait's own start, so tests can move it by hand.
type Clock = Box<dyn Fn() -> Duration + Send + Sync>;
type Sleeper = Box<dyn Fn(Duration) + Send + Sync>;

/// Waits until a guest is ready, or says why it is not.
///
/// Every seam is `Send + Sync`: the wait runs on the build thread.
pub(crate) struct GuestReadiness {
    timeouts: ReadinessTimeouts,
    /// `None` when this Windows installation has no OpenSSH client. Resolved
    /// once, at construction, so that the wait fails immediately instead of
    /// spending twenty minutes on a guest it could never have asked.
    client: Option<PathBuf>,
    address: AddressSource,
    port: PortProbe,
    ssh: SshRunner,
    now: Clock,
    sleep: Sleeper,
}

impl GuestReadiness {
    /// A readiness backed by HNS, a real TCP socket and Windows' OpenSSH.
    #[must_use]
    pub(crate) fn production(timeouts: ReadinessTimeouts) -> Self {
        Self {
            timeouts,
            client: ssh_client_path(),
            address: Box::new(endpoint_address),
            port: Box::new(probe_port),
            ssh: Box::new(run_ssh),
            now: Box::new(elapsed_since_start()),
            sleep: Box::new(std::thread::sleep),
        }
    }

    /// Waits for the guest of `mapping`, in three phases with a timeout each.
    pub(crate) fn wait(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
        username: &str,
        monitor: &BuildMonitor,
    ) -> Result<GuestReady, ReadinessFailure> {
        let Some(client) = self.client.clone() else {
            log::error!("{}", ReadinessFailure::NoSshClient);
            return Err(ReadinessFailure::NoSshClient);
        };

        let ip = self.wait_for_address(mapping, monitor)?;
        log::debug!(
            "VM \"{}\" has address {ip}; waiting for it to answer on port {SSH_PORT}",
            mapping.vm_name
        );
        self.wait_for_port(mapping, ip, monitor)?;
        log::debug!(
            "VM \"{}\" answers on port {SSH_PORT}; asking cloud-init whether it has finished",
            mapping.vm_name
        );

        let invocation = ssh_invocation(
            &client,
            vm_directory,
            username,
            ip,
            self.timeouts.connect,
        );
        let run = (self.ssh)(&invocation, self.timeouts.cloud_init, monitor).map_err(|error| {
            log::error!(
                "could not ask VM \"{}\" whether cloud-init has finished: {error}",
                mapping.vm_name
            );
            ReadinessFailure::Unreachable {
                last_error: error.to_string(),
            }
        })?;

        let result = match run {
            SshRun::Cancelled => Err(ReadinessFailure::Cancelled),
            SshRun::Exited {
                code,
                transcript_tail,
            } => outcome(code, &transcript_tail),
        };
        match &result {
            Ok(GuestReady::Ready) => {
                log::info!("VM \"{}\" reports that cloud-init is done", mapping.vm_name);
            }
            Ok(GuestReady::Degraded { detail }) => log::warn!(
                "VM \"{}\" is up but cloud-init finished degraded: {detail}",
                mapping.vm_name
            ),
            Err(failure) => log::error!("VM \"{}\" is not ready: {failure}", mapping.vm_name),
        }
        result
    }

    /// Phase one: the endpoint has to be given an address.
    fn wait_for_address(
        &self,
        mapping: &VmComputeSystemMapping,
        monitor: &BuildMonitor,
    ) -> Result<IpAddr, ReadinessFailure> {
        let deadline = (self.now)() + self.timeouts.address;
        loop {
            if monitor.is_cancelled() {
                return Err(ReadinessFailure::Cancelled);
            }
            match (self.address)(mapping) {
                Ok(Some(ip)) => return Ok(ip),
                Ok(None) => {}
                Err(error) => log::debug!(
                    "the address of VM \"{}\" is not readable yet: {error}",
                    mapping.vm_name
                ),
            }
            if (self.now)() >= deadline {
                return Err(ReadinessFailure::NoAddress);
            }
            (self.sleep)(POLL_INTERVAL);
        }
    }

    /// Phase two: something has to answer on port 22.
    ///
    /// A closed port and a refused connection are the same fact here -- the
    /// guest has not raised sshd yet -- and the last reason is kept so the
    /// failure can quote it.
    fn wait_for_port(
        &self,
        mapping: &VmComputeSystemMapping,
        ip: IpAddr,
        monitor: &BuildMonitor,
    ) -> Result<(), ReadinessFailure> {
        let deadline = (self.now)() + self.timeouts.ssh_port;
        let mut last_error = "the guest never answered".to_owned();
        loop {
            if monitor.is_cancelled() {
                return Err(ReadinessFailure::Cancelled);
            }
            match (self.port)(ip, self.timeouts.connect) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    log::debug!(
                        "VM \"{}\" does not answer on {ip}:{SSH_PORT} yet: {error}",
                        mapping.vm_name
                    );
                    last_error = error;
                }
            }
            if (self.now)() >= deadline {
                return Err(ReadinessFailure::Unreachable { last_error });
            }
            (self.sleep)(POLL_INTERVAL);
        }
    }
}
```

И тестовые конструкторы (в том же `impl`, под `#[cfg(test)]`):

```rust
    #[cfg(test)]
    fn for_test(
        timeouts: ReadinessTimeouts,
        address: impl Fn(&VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError>
        + Send
        + Sync
        + 'static,
        port: impl Fn(IpAddr, Duration) -> Result<(), String> + Send + Sync + 'static,
        ssh: impl Fn(&SshInvocation, Duration, &BuildMonitor) -> Result<SshRun, RepositoryError>
        + Send
        + Sync
        + 'static,
        now: impl Fn() -> Duration + Send + Sync + 'static,
        sleep: impl Fn(Duration) + Send + Sync + 'static,
    ) -> Self {
        Self {
            timeouts,
            client: Some(PathBuf::from(r"C:\Windows\System32\OpenSSH\ssh.exe")),
            address: Box::new(address),
            port: Box::new(port),
            ssh: Box::new(ssh),
            now: Box::new(now),
            sleep: Box::new(sleep),
        }
    }

    #[cfg(test)]
    fn for_test_without_client(
        timeouts: ReadinessTimeouts,
        now: impl Fn() -> Duration + Send + Sync + 'static,
    ) -> Self {
        Self {
            timeouts,
            client: None,
            address: Box::new(|_| panic!("nothing may be asked without an ssh client")),
            port: Box::new(|_, _| panic!("nothing may be probed without an ssh client")),
            ssh: Box::new(|_, _, _| panic!("ssh cannot run without an ssh client")),
            now: Box::new(now),
            sleep: Box::new(|_| panic!("nothing may be waited for without an ssh client")),
        }
    }
```

Production-реализации швов (`endpoint_address`, `probe_port`, `run_ssh`, `elapsed_since_start`) — задача 6. Чтобы задача 5 компилировалась и её тесты шли, добавить их временными заглушками в конце файла:

```rust
fn endpoint_address(
    _mapping: &VmComputeSystemMapping,
) -> Result<Option<IpAddr>, RepositoryError> {
    unimplemented!("wired in the next task")
}

fn probe_port(_ip: IpAddr, _timeout: Duration) -> Result<(), String> {
    unimplemented!("wired in the next task")
}

fn run_ssh(
    _invocation: &SshInvocation,
    _deadline: Duration,
    _monitor: &BuildMonitor,
) -> Result<SshRun, RepositoryError> {
    unimplemented!("wired in the next task")
}

fn elapsed_since_start() -> impl Fn() -> Duration + Send + Sync {
    let start = std::time::Instant::now();
    move || start.elapsed()
}
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-platform guest_ready::`
Expected: PASS, все тесты задач 3-5.

- [ ] **Step 5: Закоммитить**

```bash
cargo fmt --all -- --check
git add crates/platform/src/guest_ready.rs
git commit -m "TASK-63: Wait for a guest in three phases with a timeout each"
```

---

### Task 6: Production-швы ожидания

**Files:**
- Modify: `crates/platform/src/guest_ready.rs`
- Test: там же

**Interfaces:**
- Consumes: `SshInvocation`, `SshRun`, `tail`, `DIAGNOSTIC_TAIL_LINES`.
- Produces: `pub(crate) fn com1_tail(vm_directory: &Path) -> Option<String>`, рабочие `endpoint_address`, `probe_port`, `run_ssh`.

- [ ] **Step 1: Написать падающие тесты**

```rust
    #[test]
    fn the_com1_tail_is_the_end_of_the_log_and_nothing_more() {
        let directory = std::env::temp_dir().join(format!(
            "vmlord-com1-tail-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let lines = (1..=100).map(|n| format!("boot line {n}")).collect::<Vec<_>>();
        std::fs::write(
            crate::layout::com1_log_path(&directory),
            lines.join("\n"),
        )
        .unwrap();

        let captured = super::com1_tail(&directory).unwrap();

        assert!(captured.contains("boot line 100"), "{captured}");
        assert!(
            !captured.contains("boot line 59"),
            "only the last {} lines belong in a diagnostic: {captured}",
            super::DIAGNOSTIC_TAIL_LINES
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_vm_without_a_serial_log_has_no_tail_rather_than_an_error() {
        // A VM whose console never opened still has to produce a diagnostic
        // about its readiness; the missing log is simply nothing to add.
        assert_eq!(
            super::com1_tail(Path::new(r"C:\VMs\never-existed")),
            None
        );
    }

    #[test]
    fn a_probe_of_a_port_nobody_listens_on_fails_rather_than_hangs() {
        // 127.0.0.1:9 is the discard port, which nothing serves on a developer
        // machine: the probe must come back with a reason, not block.
        let error = super::probe_port("127.0.0.1".parse().unwrap(), Duration::from_millis(200));

        assert!(error.is_err(), "an unserved port must not report success");
    }
```

Заметь: `probe_port` в проде ходит на порт 22; для теста нужна параметризация. Изменить сигнатуру на `fn probe_port_at(ip: IpAddr, port: u16, timeout: Duration) -> Result<(), String>` и оставить `fn probe_port(ip, timeout)` тонкой обёрткой над ней с `SSH_PORT`; тест выше вызывает `probe_port` — заменить в нём вызов на `super::probe_port_at("127.0.0.1".parse().unwrap(), 9, Duration::from_millis(200))`.

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-platform guest_ready::tests::the_com1_tail`
Expected: FAIL — `com1_tail` не существует.

- [ ] **Step 3: Реализовать**

Заменить заглушки на:

```rust
use std::{
    fs::File,
    io::Read,
    net::TcpStream,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
};

use crate::hcn_endpoint::HcnEndpoint;

/// Keeps a helper's console window from appearing.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How often a running `ssh.exe` is looked at.
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The address HNS has given the VM's endpoint, if it has one yet.
///
/// The endpoint is where a guest's address is known on the host side: HNS
/// assigns it and VMLord's DHCP server offers the guest that one and no other.
fn endpoint_address(mapping: &VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError> {
    let Some(endpoint_id) = mapping.endpoint_id else {
        return Ok(None);
    };
    let Some(endpoint) = HcnEndpoint::open_if_present(endpoint_id)? else {
        return Ok(None);
    };
    let Some(address) = endpoint.address()? else {
        return Ok(None);
    };
    match address.ip_address.parse() {
        Ok(ip) => Ok(Some(ip)),
        Err(error) => {
            log::debug!(
                "HNS reported \"{}\" as the address of VM \"{}\", which is not an IP address: \
                 {error}",
                address.ip_address,
                mapping.vm_name
            );
            Ok(None)
        }
    }
}

/// Whether anything answers a TCP connection at `ip:port`.
fn probe_port_at(ip: IpAddr, port: u16, timeout: Duration) -> Result<(), String> {
    TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout)
        .map(drop)
        .map_err(|error| error.to_string())
}

fn probe_port(ip: IpAddr, timeout: Duration) -> Result<(), String> {
    probe_port_at(ip, SSH_PORT, timeout)
}

/// Runs one readiness command, killing it at `deadline` or on cancellation.
///
/// The child's output goes to a file rather than a pipe: `--wait` prints for as
/// long as it runs, and a pipe nobody is draining fills and deadlocks it
/// against this loop.
fn run_ssh(
    invocation: &SshInvocation,
    deadline: Duration,
    monitor: &BuildMonitor,
) -> Result<SshRun, RepositoryError> {
    let transcript = File::create(&invocation.transcript).map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to open the readiness transcript {}: {error}",
            invocation.transcript.display()
        ));
        log::error!("{error}");
        error
    })?;
    let errors = transcript.try_clone().map_err(|error| {
        RepositoryError::new(format!(
            "failed to capture the errors of the readiness command: {error}"
        ))
    })?;

    log::debug!(
        "running {} to wait for cloud-init; its output goes to {}",
        invocation.program.display(),
        invocation.transcript.display()
    );
    let mut child = Command::new(&invocation.program)
        .args(&invocation.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(transcript))
        .stderr(Stdio::from(errors))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|error| {
            let error = RepositoryError::new(format!(
                "failed to run {}: {error}",
                invocation.program.display()
            ));
            log::error!("{error}");
            error
        })?;

    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(SshRun::Exited {
                    code: status.code(),
                    transcript_tail: transcript_tail(&invocation.transcript),
                });
            }
            Ok(None) => {}
            Err(error) => {
                return Err(RepositoryError::new(format!(
                    "failed to wait for {}: {error}",
                    invocation.program.display()
                )));
            }
        }

        if monitor.is_cancelled() {
            log::warn!("the readiness command is being killed because the build was cancelled");
            kill(&mut child);
            return Ok(SshRun::Cancelled);
        }
        if started.elapsed() >= deadline {
            log::error!(
                "the readiness command did not finish within {} seconds; killing it",
                deadline.as_secs()
            );
            kill(&mut child);
            return Ok(SshRun::Exited {
                code: None,
                transcript_tail: transcript_tail(&invocation.transcript),
            });
        }
        std::thread::sleep(CHILD_POLL_INTERVAL);
    }
}

/// Kills a child and reaps it, so no zombie handle is left behind.
fn kill(child: &mut std::process::Child) {
    if let Err(error) = child.kill() {
        log::warn!("the readiness command could not be killed: {error}");
        return;
    }
    if let Err(error) = child.wait() {
        log::warn!("the killed readiness command could not be reaped: {error}");
    }
}

/// The end of a file, or an empty string when there is nothing to read.
fn file_tail(path: &Path) -> Option<String> {
    let mut text = String::new();
    File::open(path)
        .ok()?
        .read_to_string(&mut text)
        .ok()?;
    Some(tail(&text, DIAGNOSTIC_TAIL_LINES))
}

fn transcript_tail(path: &Path) -> String {
    file_tail(path).unwrap_or_default()
}

/// The end of the VM's serial console log, for a diagnostic about a guest that
/// never became ready. A VM with no log yet simply has nothing to add.
pub(crate) fn com1_tail(vm_directory: &Path) -> Option<String> {
    let captured = file_tail(&layout::com1_log_path(vm_directory))?;
    (!captured.is_empty()).then_some(captured)
}

fn elapsed_since_start() -> impl Fn() -> Duration + Send + Sync {
    let start = std::time::Instant::now();
    move || start.elapsed()
}
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run:

```bash
cargo test -p vmlord-platform guest_ready::
cargo clippy --target=x86_64-pc-windows-gnu -p vmlord-platform --all-targets -- -D warnings
```

Expected: PASS, без предупреждений.

- [ ] **Step 5: Закоммитить**

```bash
cargo fmt --all -- --check
git add crates/platform/src/guest_ready.rs
git commit -m "TASK-63: Back the readiness wait with HNS, TCP and ssh.exe"
```

---

### Task 7: Пайплайны становятся потокобезопасными

**Files:**
- Modify: `crates/platform/src/start.rs:51-54`, `crates/platform/src/force_stop.rs:19-20`, `crates/platform/src/delete.rs`, `crates/platform/src/cleanup.rs:37`, `crates/platform/src/event.rs:25`
- Test: `crates/platform/src/start.rs` (модуль `tests`)

**Interfaces:**
- Produces: `VmStartPipeline`, `VmForceStopPipeline`, `VmDeletionPipeline` и `Com1Session` пригодны к передаче на другой поток — на это опираются задачи 8-10.

- [ ] **Step 1: Написать падающий тест**

В `mod tests` файла `crates/platform/src/start.rs`:

```rust
    #[test]
    fn the_pipelines_a_build_thread_needs_can_be_moved_to_it() {
        // Creating a VM now starts it and waits for its guest, all on the build
        // thread, so everything that cycle owns has to be able to go there.
        const fn assert_send_sync<T: Send + Sync>() {}
        const fn assert_send<T: Send>() {}

        assert_send_sync::<VmStartPipeline>();
        assert_send_sync::<crate::force_stop::VmForceStopPipeline>();
        assert_send_sync::<crate::delete::VmDeletionPipeline>();
        assert_send::<crate::com1_terminal::Com1Session>();
    }
```

- [ ] **Step 2: Убедиться, что тест падает**

Run: `cargo test -p vmlord-platform the_pipelines_a_build_thread_needs_can_be_moved_to_it`
Expected: FAIL — `*mut c_void` cannot be sent between threads safely / `Box<dyn Fn...>` is not `Send`.

- [ ] **Step 3: Реализовать**

В `crates/platform/src/start.rs` добавить `+ Send + Sync` в три алиаса:

```rust
type AccessGranter = Box<dyn Fn(&str, &Path) -> Result<(), RepositoryError> + Send + Sync>;
type SystemStarter = Box<dyn Fn(&str, &str) -> Result<(), HcsStartFailure> + Send + Sync>;
type EndpointProvider = Box<
    dyn Fn(&str, Option<Uuid>, EndpointPolicy) -> Result<VmNetworkAdapter, RepositoryError>
        + Send
        + Sync,
>;
```

и те же границы в `impl ... + 'static` параметрах `for_test`. То же в `force_stop.rs` (`AdapterDetacher`, `SystemTerminator`), в `delete.rs` (его два алиаса) и в `cleanup.rs` для `EndpointTeardown`. Проверить, что `dhcp::DhcpRegistrar` уже `Send + Sync`; если нет — добавить так же.

В `crates/platform/src/event.rs` после определения `WindowsEvent`:

```rust
// SAFETY: a Windows event is a kernel object referred to by a handle that is
// valid process-wide; the API is explicitly built for signalling one thread
// from another, and this type owns its handle rather than sharing it. Moving
// the owner to another thread is therefore sound. `Sync` is deliberately not
// claimed: nothing here needs a shared reference from two threads at once.
unsafe impl Send for WindowsEvent {}
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run:

```bash
cargo test -p vmlord-platform the_pipelines_a_build_thread_needs_can_be_moved_to_it
cargo test -p vmlord-platform
```

Expected: PASS, ничего не сломано.

- [ ] **Step 5: Закоммитить**

```bash
cargo fmt --all -- --check
git add crates/platform/src/start.rs crates/platform/src/force_stop.rs crates/platform/src/delete.rs crates/platform/src/cleanup.rs crates/platform/src/event.rs
git commit -m "TASK-63: Let a build thread own the start, stop and delete pipelines"
```

---

### Task 8: Слот исхода у сборки

**Files:**
- Modify: `crates/platform/src/build.rs`
- Test: там же

**Interfaces:**
- Consumes: `Com1Session` (Send, задача 7), `VmComputeSystemMapping`.
- Produces:
  - `pub(crate) struct StartedVm { pub(crate) mapping: VmComputeSystemMapping, pub(crate) session: Com1Session }`
  - `BuildRegistry::start<F>` с `F: FnOnce(&BuildMonitor) -> Option<StartedVm> + Send + 'static`
  - `pub(crate) fn take_started(&self) -> Vec<StartedVm>`

- [ ] **Step 1: Написать падающий тест**

В `mod tests` файла `build.rs`:

```rust
    #[test]
    fn a_build_hands_its_started_vm_to_whoever_reaps_it() {
        // The session and the compute-system handle live on the main thread,
        // so the build thread hands them back rather than holding them.
        let registry = BuildRegistry::default();
        let handed = Arc::new(AtomicBool::new(false));

        registry
            .start(request("dev"), {
                let handed = Arc::clone(&handed);
                move |_| {
                    handed.store(true, Ordering::Relaxed);
                    Some(super::StartedVm {
                        mapping: mapping("dev"),
                        session: crate::com1_terminal::Com1Session::for_test("dev"),
                    })
                }
            })
            .unwrap();

        while !handed.load(Ordering::Relaxed) {
            std::thread::yield_now();
        }
        // `reap` runs inside every query; the outcome must survive it rather
        // than be dropped, because dropping a session cancels its reader.
        registry.reap();
        let started = registry.take_started();

        assert_eq!(started.len(), 1);
        assert_eq!(started[0].mapping.vm_name, "dev");
        assert!(
            registry.take_started().is_empty(),
            "an outcome is handed over once"
        );
    }

    #[test]
    fn a_build_that_hands_back_nothing_leaves_nothing_to_collect() {
        let registry = BuildRegistry::default();

        registry.start(request("rolled-back"), |_| None).unwrap();
        registry.cancel_all_and_join();

        assert!(registry.take_started().is_empty());
    }
```

Вспомогательные `fn mapping(name: &str) -> VmComputeSystemMapping` (по образцу из `guest_ready.rs`) и `Com1Session::for_test(vm_name: &str) -> Com1Session` добавить: первое — в тестовый модуль `build.rs`, второе — в `com1_terminal.rs` под `#[cfg(test)] pub(crate) fn for_test`, создающий сессию с новыми `WindowsEvent` и `cancellations: None`.

- [ ] **Step 2: Убедиться, что тест падает**

Run: `cargo test -p vmlord-platform build::tests::a_build_hands_its_started_vm`
Expected: FAIL — `StartedVm` не существует.

- [ ] **Step 3: Реализовать**

В `build.rs`:

```rust
/// A VM whose creation got as far as starting it: what the main thread has to
/// take over from the build thread.
///
/// The COM1 session and the held compute system belong to the repository, and
/// the repository is `&mut self` -- so the build thread parks them here and a
/// reap hands them over.
pub(crate) struct StartedVm {
    pub(crate) mapping: VmComputeSystemMapping,
    pub(crate) session: Com1Session,
}
```

В `struct Build` добавить `outcome: Arc<Mutex<Option<StartedVm>>>`, в `BuildRegistry` — `started: Mutex<Vec<StartedVm>>`. Сигнатура `start`:

```rust
    pub(crate) fn start<F>(&self, request: VmCreateRequest, build: F) -> Result<(), RepositoryError>
    where
        F: FnOnce(&BuildMonitor) -> Option<StartedVm> + Send + 'static,
```

и в теле потока:

```rust
                move || {
                    let _finish = Finish(finished);
                    if let Some(started) = build(&monitor) {
                        *outcome.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            Some(started);
                    }
                }
```

где `outcome` — клон `Arc`, положенный в `Build`. Порядок важен: результат кладётся до того, как `Finish` пометит сборку законченной, — иначе `reap` может забрать пустой слот.

В `reap` перед удалением сборки забирать её исход:

```rust
        for name in done {
            if let Some(mut build) = builds.remove(&name) {
                if let Some(started) = build
                    .outcome
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    self.started
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(started);
                }
                if let Some(worker) = build.worker.take()
                    && worker.join().is_err()
                {
                    log::error!("the thread creating VM \"{name}\" panicked");
                }
            }
        }
```

То же — в `cancel_all_and_join`. И новый метод:

```rust
    /// Hands over the VMs that builds have started since this was last called.
    ///
    /// A separate call rather than a return value of `reap`, because `reap`
    /// runs inside every query -- including ones taking `&self` -- and a
    /// returned session that a caller dropped would silently cancel a running
    /// VM's console reader.
    pub(crate) fn take_started(&self) -> Vec<StartedVm> {
        self.reap();
        std::mem::take(
            &mut *self
                .started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
```

`#[derive(Default)]` на `BuildRegistry` продолжает работать: `Mutex<Vec<_>>` имеет `Default`.

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-platform build::`
Expected: PASS. Существующие вызовы `builds.start(...)` в `repository.rs` перестанут компилироваться — исправить их временно, вернув `None` в конце замыкания; задача 9 заменит тело целиком.

- [ ] **Step 5: Закоммитить**

```bash
cargo fmt --all -- --check
cargo test -p vmlord-platform
git add crates/platform/src/build.rs crates/platform/src/com1_terminal.rs crates/platform/src/repository.rs
git commit -m "TASK-63: Hand a started VM from the build thread to the repository"
```

---

### Task 9: Полный цикл создания

**Files:**
- Create: `crates/platform/src/cycle.rs`
- Modify: `crates/platform/src/lib.rs`
- Test: `crates/platform/src/cycle.rs`

**Interfaces:**
- Consumes: `VmCreationPipeline`, `VmStartPipeline`, `GuestReadiness`, `VmForceStopPipeline`, `VmDeletionPipeline`, `StartedVm`, `com1_tail`.
- Produces:
  - `pub(crate) enum CycleOutcome { Ready, Degraded { detail: String }, NotReady { reason: String }, Failed { reason: String }, Cancelled }`
  - `pub(crate) struct CycleReport { pub(crate) outcome: CycleOutcome, pub(crate) started: Option<StartedVm> }`
  - `pub(crate) struct VmBuildCycle` с `production(cloud_disk, com1, timeouts)`, `for_test(...)` и
    `run(&self, store: &MetadataStore, request: &VmCreateRequest, vm_directory: &Path, monitor: &BuildMonitor) -> CycleReport`

- [ ] **Step 1: Написать падающие тесты**

Создать `crates/platform/src/cycle.rs`. Тестовый модуль строит цикл из `for_test`-пайплайнов; готовый шаблон:

```rust
#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use vmlord_core::{BuildMonitor, BuildStep, RepositoryError};

    use super::{CycleOutcome, VmBuildCycle};
    use crate::guest_ready::{GuestReady, ReadinessFailure};

    /// What the fake pipelines recorded, in order.
    #[derive(Clone, Default)]
    struct Calls(Arc<Mutex<Vec<String>>>);

    impl Calls {
        fn record(&self, call: &str) {
            self.0.lock().unwrap().push(call.to_owned());
        }

        fn taken(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    #[test]
    fn a_ready_guest_ends_the_cycle_with_the_vm_running() { /* см. ниже */ }
}
```

Тесты, которые должны быть в этом модуле (каждый — отдельный `#[test]`):

1. `a_ready_guest_ends_the_cycle_with_the_vm_running` — создание, старт и ожидание успешны: `outcome == CycleOutcome::Ready`, `started.is_some()`, порядок вызовов `["create", "start", "wait"]`, и ни `force_stop`, ни `delete` не вызывались. Отдельно проверить, что монитор прошёл через `BuildStep::Starting` и `BuildStep::AwaitingGuest` (снять `monitor.snapshot().step` из фейкового старта и фейкового ожидания).
2. `a_degraded_guest_still_leaves_a_usable_vm` — `wait` возвращает `Ok(GuestReady::Degraded { detail })`: `CycleOutcome::Degraded { detail }` с сохранённым текстом, `started.is_some()`, отката не было.
3. `a_start_that_fails_rolls_the_whole_creation_back` — `start` возвращает `Err`: `CycleOutcome::Failed { reason }` содержит текст ошибки старта, `started.is_none()`, вызван `delete` и **не** вызван `force_stop` (стартовать было нечего).
4. `a_guest_that_never_becomes_ready_keeps_its_vm_and_its_evidence` — `wait` возвращает `Err(ReadinessFailure::TimedOut)`: `CycleOutcome::NotReady { reason }`, где `reason` содержит и текст `ReadinessFailure::TimedOut`, и строку из подложенного `com1.log`; `started.is_some()`; ни `force_stop`, ни `delete` не вызывались. Файл `com1.log` для этого положить в `vm_directory` в самом тесте.
5. `a_cancelled_wait_stops_and_removes_the_vm_it_had_started` — `wait` возвращает `Err(ReadinessFailure::Cancelled)`: `CycleOutcome::Cancelled`, `started.is_none()`, вызваны и `force_stop`, и `delete`, именно в этом порядке.
6. `a_cancellation_between_creation_and_start_removes_what_was_created` — `monitor.cancel()` вызван фейковым `create`: `start` не вызывался, `delete` вызван, `force_stop` — нет, исход `Cancelled`.
7. `a_creation_that_fails_needs_no_rollback_of_its_own` — `create` возвращает `Err`: `CycleOutcome::Failed`, ни `force_stop`, ни `delete` не вызывались (создание откатывает себя само).
8. `a_rollback_that_fails_is_reported_but_does_not_change_the_outcome` — `delete` возвращает `Err` при отмене: исход остаётся `Cancelled` (в лог уходит ERROR, но цикл не меняет своего ответа).

Каждому тесту нужен `MetadataStore` во временном каталоге — по образцу `temp_store` из `repository.rs:995`; скопировать этот помощник в тестовый модуль `cycle.rs`.

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-platform cycle::`
Expected: FAIL — модуля `cycle` не существует.

- [ ] **Step 3: Реализовать**

`crates/platform/src/lib.rs`: добавить `mod cycle;`.

`crates/platform/src/cycle.rs`:

```rust
//! Creating a VM from end to end: build it, start it, and wait until its guest
//! says it is ready.
//!
//! Creation used to end at a registered compute system, which is the moment
//! VMLord stops working and well before the VM does anything. A build that
//! ends there reports success for a guest whose cloud-init may still fail
//! minutes later, so the cycle carries on to the only fact worth reporting:
//! the guest answered.

use std::path::Path;

use vmlord_core::{BuildMonitor, BuildStep, RepositoryError, VmCreateRequest};

use crate::{
    build::StartedVm,
    com1_terminal::Com1Launcher,
    create::{CloudDiskImporter, VmCreationPipeline},
    delete::VmDeletionPipeline,
    force_stop::VmForceStopPipeline,
    guest_ready::{GuestReady, GuestReadiness, ReadinessFailure, ReadinessTimeouts, com1_tail},
    metadata::MetadataStore,
    start::VmStartPipeline,
};

/// How a creation ended, in the terms the repository turns into diagnostics.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CycleOutcome {
    Ready,
    /// The guest is up, but cloud-init finished degraded.
    Degraded { detail: String },
    /// The VM exists and runs, but its guest never reported readiness. It is
    /// deliberately left in place: its serial log is the only account of what
    /// went wrong, and removing the VM would remove it.
    NotReady { reason: String },
    /// Nothing usable was created; whatever had been built is gone.
    Failed { reason: String },
    Cancelled,
}

/// Whether a rollback has a running VM to stop first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Running {
    Yes,
    No,
}

/// The result of a cycle, and whatever the main thread has to take over.
pub(crate) struct CycleReport {
    pub(crate) outcome: CycleOutcome,
    /// `None` when the VM was rolled back: there is nothing left to hold.
    pub(crate) started: Option<StartedVm>,
}

/// Creates a VM, starts it, and waits for its guest.
pub(crate) struct VmBuildCycle {
    creation: VmCreationPipeline,
    start: VmStartPipeline,
    readiness: GuestReadiness,
    force_stop: VmForceStopPipeline,
    deletion: VmDeletionPipeline,
}

impl VmBuildCycle {
    #[must_use]
    pub(crate) fn production(
        cloud_disk: CloudDiskImporter,
        com1: Com1Launcher,
        timeouts: ReadinessTimeouts,
    ) -> Self {
        Self {
            creation: VmCreationPipeline::production(cloud_disk),
            start: VmStartPipeline::production(com1),
            readiness: GuestReadiness::production(timeouts),
            force_stop: VmForceStopPipeline::production(),
            deletion: VmDeletionPipeline::production(),
        }
    }

    /// Replaces the wait's timeouts with the user's own.
    ///
    /// A setter rather than an argument of `production`, because the importer
    /// the cycle owns is a boxed closure that cannot be cloned: rebuilding the
    /// whole cycle to change four durations would mean taking the importer
    /// apart. Only the readiness is rebuilt, and it is stateless.
    pub(crate) fn set_timeouts(&mut self, timeouts: ReadinessTimeouts) {
        self.readiness = GuestReadiness::production(timeouts);
    }

    pub(crate) fn run(
        &self,
        store: &MetadataStore,
        request: &VmCreateRequest,
        vm_directory: &Path,
        monitor: &BuildMonitor,
    ) -> CycleReport {
        let mapping = match self.creation.create(store, request, vm_directory, monitor) {
            Ok(mapping) => mapping,
            Err(error) => {
                // The creation pipeline rolls itself back, so there is nothing
                // here left to undo.
                return CycleReport {
                    outcome: CycleOutcome::Failed {
                        reason: error.to_string(),
                    },
                    started: None,
                };
            }
        };

        if monitor.is_cancelled() {
            return self.roll_back(store, request, vm_directory, Running::No);
        }

        monitor.report(BuildStep::Starting);
        let session = match self.start.start(store, &request.name, vm_directory) {
            Ok(session) => session,
            Err(error) => {
                log::error!(
                    "VM \"{}\" was created but could not be started: {error}",
                    request.name
                );
                let CycleReport { started, .. } =
                    self.roll_back(store, request, vm_directory, Running::No);
                debug_assert!(started.is_none());
                return CycleReport {
                    outcome: CycleOutcome::Failed {
                        reason: error.to_string(),
                    },
                    started: None,
                };
            }
        };

        // Re-read: starting the VM is what gives it an endpoint, and the
        // address the wait polls hangs off that.
        let mapping = store
            .find_by_vm_name(&request.name)
            .ok()
            .flatten()
            .unwrap_or(mapping);

        monitor.report(BuildStep::AwaitingGuest);
        let started = StartedVm { mapping, session };
        match self.readiness.wait(
            &started.mapping,
            vm_directory,
            &request.provisioning.username,
            monitor,
        ) {
            Ok(GuestReady::Ready) => CycleReport {
                outcome: CycleOutcome::Ready,
                started: Some(started),
            },
            Ok(GuestReady::Degraded { detail }) => CycleReport {
                outcome: CycleOutcome::Degraded { detail },
                started: Some(started),
            },
            Err(ReadinessFailure::Cancelled) => {
                // Dropping the session with the VM is right: the console it
                // reads is about to stop existing.
                drop(started);
                self.roll_back(store, request, vm_directory, Running::Yes)
            }
            Err(failure) => {
                let mut reason = failure.to_string();
                if let Some(tail) = com1_tail(vm_directory) {
                    reason.push_str("\n\nThe end of the VM's serial console:\n");
                    reason.push_str(&tail);
                }
                CycleReport {
                    outcome: CycleOutcome::NotReady { reason },
                    started: Some(started),
                }
            }
        }
    }

    /// Undoes a creation that has already produced a VM.
    ///
    /// `running` says whether the VM was started, and so whether it has to be
    /// stopped before it can be removed. Failures here are logged and not
    /// propagated: the cycle's answer is that the creation was undone, and a
    /// residue that could not be cleared is a warning about the host rather
    /// than a different outcome for the build.
    fn roll_back(
        &self,
        store: &MetadataStore,
        request: &VmCreateRequest,
        vm_directory: &Path,
        running: Running,
    ) -> CycleReport {
        log::warn!(
            "rolling back the creation of VM \"{}\" because it was cancelled or could not be \
             started",
            request.name
        );
        if running == Running::Yes
            && let Err(error) = self.force_stop.force_stop(store, &request.name)
        {
            log::error!(
                "VM \"{}\" could not be stopped while its creation was rolled back: {error}",
                request.name
            );
        }
        if let Err(error) = self
            .deletion
            .delete(store, &request.name, vm_directory, true)
        {
            log::error!(
                "VM \"{}\" could not be removed while its creation was rolled back: {error}",
                request.name
            );
        }
        CycleReport {
            outcome: CycleOutcome::Cancelled,
            started: None,
        }
    }
}
```

Плюс `#[cfg(test)] fn for_test(...)`, принимающий пять уже собранных частей (`VmCreationPipeline`, `VmStartPipeline`, `GuestReadiness`, `VmForceStopPipeline`, `VmDeletionPipeline`), — тесты собирают каждую через её собственный `for_test`. Для этого пометить `for_test` соответствующих пайплайнов как `pub(crate)` (сейчас они приватные, а вызываются из другого модуля).

Обрати внимание: `roll_back` возвращает `CycleOutcome::Cancelled`, поэтому в ветке неудачного старта его исход перезаписывается на `Failed` — так «не смогли стартовать» не выглядит как «пользователь отменил».

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-platform cycle::`
Expected: PASS, восемь тестов.

- [ ] **Step 5: Закоммитить**

```bash
cargo fmt --all -- --check
cargo clippy --target=x86_64-pc-windows-gnu -p vmlord-platform --all-targets -- -D warnings
git add crates/platform/src/cycle.rs crates/platform/src/lib.rs crates/platform/src/create.rs crates/platform/src/start.rs crates/platform/src/force_stop.rs crates/platform/src/delete.rs
git commit -m "TASK-63: Create a VM, start it and wait for its guest as one cycle"
```

---

### Task 10: Репозиторий, настройки и живой тест

**Files:**
- Modify: `crates/platform/src/repository.rs`
- Modify: `crates/vmlord/src/main.rs`
- Modify: `crates/platform/tests/hyperv.rs`
- Modify: `ARCHITECTURE.md`
- Test: `crates/platform/src/repository.rs`, `crates/platform/tests/hyperv.rs`

**Interfaces:**
- Consumes: всё предыдущее.
- Produces: `HcsVmRepository::with_readiness_timeouts(self, timeouts: GuestReadinessTimeouts) -> Self`.

- [ ] **Step 1: Написать падающие тесты**

В `mod tests` файла `repository.rs`:

```rust
    #[test]
    fn a_started_vm_handed_over_by_a_build_is_held_and_its_console_kept() {
        // The build thread cannot touch `com1_sessions` or hold a compute
        // system: both live behind `&mut self`. Taking them over on refresh is
        // what makes a VM created in the background indistinguishable from one
        // started by hand.
        let (root, _store) = temp_store("handover");
        let mut repository = HcsVmRepository::new(&root, no_cloud_images());
        let session = crate::com1_terminal::Com1Session::for_test("dev");
        let vm_id = session.vm_id;

        repository.adopt_started(vec![crate::build::StartedVm {
            mapping: mapping_for_test("dev", vm_id),
            session,
        }]);

        assert!(repository.com1_sessions.contains(vm_id));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_readiness_timeouts_come_from_the_settings() {
        let (root, _store) = temp_store("timeouts");
        let repository = HcsVmRepository::new(&root, no_cloud_images())
            .with_readiness_timeouts(vmlord_core::GuestReadinessTimeouts {
                address_secs: 1,
                ssh_port_secs: 2,
                cloud_init_secs: 3,
                connect_timeout_secs: 4,
            });

        assert_eq!(
            repository.readiness_timeouts,
            crate::guest_ready::ReadinessTimeouts {
                address: std::time::Duration::from_secs(1),
                ssh_port: std::time::Duration::from_secs(2),
                cloud_init: std::time::Duration::from_secs(3),
                connect: std::time::Duration::from_secs(4),
            }
        );
        std::fs::remove_dir_all(&root).ok();
    }
```

`mapping_for_test`, `no_cloud_images` и `temp_store` уже есть или строятся по образцу существующих тестов файла; `adopt_started` — новый приватный метод, который тест вызывает напрямую (он в том же модуле).

В `crates/platform/tests/hyperv.rs` добавить:

```rust
/// Creates a real Ubuntu cloud-image VM and waits for its guest to report that
/// cloud-init has finished.
///
/// Ignored: it needs Hyper-V, an elevated process, a network, and several
/// minutes. Run it by hand:
/// `cargo test -p vmlord-platform --test hyperv a_created_vm_becomes_ready -- --ignored --nocapture`
#[test]
#[ignore]
fn a_created_vm_becomes_ready_before_its_build_finishes() {
    // ... по образцу существующих #[ignore]-тестов файла:
    // 1. собрать репозиторий с реальным импортёром облачного образа;
    // 2. create_vm с Ubuntu LTS, username и SSH-ключом;
    // 3. крутить list_vms, пока VM числится Building, не дольше 30 минут;
    // 4. убедиться, что диагностик уровня Error не появилось;
    // 5. убедиться, что VM в списке Running и с адресом;
    // 6. в конце -- delete_vm, чтобы тест не оставлял за собой VM.
}
```

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo test -p vmlord-platform repository::tests::the_readiness_timeouts_come_from_the_settings`
Expected: FAIL — метода `with_readiness_timeouts` нет.

- [ ] **Step 3: Реализовать**

В `HcsVmRepository`:
- заменить поле `creation: Arc<VmCreationPipeline>` на `cycle: Arc<VmBuildCycle>`, добавить `readiness_timeouts: ReadinessTimeouts`;
- в `new` построить цикл: `VmBuildCycle::production(cloud_disk, com1_launcher.clone(), ReadinessTimeouts::default())`;
- добавить строитель:

```rust
    /// Replaces the readiness timeouts with the user's own.
    ///
    /// A builder rather than an argument of `new`: the timeouts are the only
    /// thing about a repository that comes from settings, and every existing
    /// caller -- the tests included -- means the defaults.
    #[must_use]
    pub fn with_readiness_timeouts(mut self, timeouts: GuestReadinessTimeouts) -> Self {
        let timeouts = ReadinessTimeouts::from(timeouts);
        self.readiness_timeouts = timeouts;
        match Arc::get_mut(&mut self.cycle) {
            // The only caller is the composition root, before any build thread
            // exists, so this reference is the only one.
            Some(cycle) => cycle.set_timeouts(timeouts),
            None => log::error!(
                "the readiness timeouts were left at their defaults: a build is already \
                 running with the cycle they would have changed"
            ),
        }
        self
    }
```

Цикл целиком здесь не пересобирается: `CloudDiskImporter` — неклонируемый бокс, и добраться до него, не разбирая цикл, нельзя. Меняется только `GuestReadiness` — она без состояния, поэтому замена её целиком дешевле любой альтернативы. Метод `VmBuildCycle::set_timeouts` добавлен в задаче 9.

- в `create_vm` заменить тело замыкания:

```rust
        let cycle = Arc::clone(&self.cycle);
        let store = self.store.clone();
        let diagnostics = Arc::clone(&self.diagnostics);
        let name = request.name.clone();
        self.builds.start(request.clone(), move |monitor| {
            let report = cycle.run(&store, &request, &vm_directory, monitor);
            match report.outcome {
                CycleOutcome::Ready => {
                    log::info!("VM \"{name}\" finished building and its guest is ready");
                }
                CycleOutcome::Degraded { detail } => push_shared_diagnostic(
                    &diagnostics,
                    DiagnosticLevel::Warning,
                    format!("VM \"{name}\" is up, but cloud-init finished degraded: {detail}"),
                ),
                CycleOutcome::NotReady { reason } => push_shared_diagnostic(
                    &diagnostics,
                    DiagnosticLevel::Error,
                    format!(
                        "VM \"{name}\" was created and started, but never became ready: {reason}"
                    ),
                ),
                CycleOutcome::Failed { reason } => push_shared_diagnostic(
                    &diagnostics,
                    DiagnosticLevel::Error,
                    format!("Failed to create VM \"{name}\": {reason}"),
                ),
                CycleOutcome::Cancelled => push_shared_diagnostic(
                    &diagnostics,
                    DiagnosticLevel::Info,
                    format!("Creating VM \"{name}\" was cancelled"),
                ),
            }
            report.started
        })
```

- добавить приём исходов, вызывая его там же, где уже вызывается `self.builds.reap()` (строка ~732):

```rust
    /// Takes over the VMs that background builds have started.
    ///
    /// Called on refresh, where `&mut self` is available: the session and the
    /// compute-system handle a build produced belong here, not on its thread.
    fn adopt_started(&mut self, started: Vec<StartedVm>) {
        for StartedVm { mapping, session } in started {
            log::debug!(
                "taking over the console and the compute system of VM \"{}\", built in the \
                 background",
                mapping.vm_name
            );
            self.com1_sessions.insert(session);
            self.hold_started_system(&mapping);
        }
    }
```

заменив `self.builds.reap();` на `let started = self.builds.take_started(); self.adopt_started(started);`.

В `crates/vmlord/src/main.rs`, в `load_backend`, дополнить построение репозитория:

```rust
        return Box::new(
            vmlord_platform::HcsVmRepository::new(
                settings.vm_storage_path.clone(),
                cloud_disk_importer(settings.image_cache_path.clone()),
            )
            .with_readiness_timeouts(settings.guest_readiness),
        );
```

- [ ] **Step 4: Убедиться, что всё проходит**

Run:

```bash
cargo test -p vmlord-platform
cargo test -p vmlord-core
cargo test -p vmlord-ui
cargo build --target=x86_64-pc-windows-gnu
cargo clippy --target=x86_64-pc-windows-gnu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: всё PASS, `#[ignore]`-тест числится как ignored.

- [ ] **Step 5: Обновить ARCHITECTURE.md и закоммитить**

В разделе про создание VM описать полный цикл: создание → старт → ожидание готовности, три фазы ожидания, транспорт `ssh.exe`, правила отката и группу `[guest_readiness]` в настройках.

```bash
git add crates/platform/src/repository.rs crates/vmlord/src/main.rs crates/platform/tests/hyperv.rs ARCHITECTURE.md
git commit -m "TASK-63: End a build only when its guest is ready"
```

---

### Task 11: Ручная проверка на живом Hyper-V

**Files:** нет — это проверка, а не изменение.

- [ ] **Step 1: Прогнать живой тест**

Run (на Windows, из elevated-консоли):

```bash
cargo test -p vmlord-platform --test hyperv a_created_vm_becomes_ready -- --ignored --nocapture
```

Expected: тест проходит; в каталоге VM появились `com1.log` и `cloud-init-status.log`; в `cloud-init-status.log` видно `status: done`.

- [ ] **Step 2: Проверить руками сценарии, которые тест не покрывает**

- Создать VM через UI: список показывает `Building: starting the VM`, затем `Building: waiting for the guest`, и строка исчезает только когда гость готов.
- Отменить сборку на шаге ожидания: VM исчезает целиком — ни каталога, ни компьют-системы, ни endpoint'а, — а образ остаётся в кэше.
- Временно выставить `cloud_init_secs = 60` в `settings.toml` и создать VM: сборка заканчивается диагностикой Error с хвостом `com1.log`, VM остаётся запущенной.

- [ ] **Step 3: Зафиксировать результат**

Если что-то из проверок не сошлось — вернуться к соответствующей задаче. Если всё сошлось, ветка готова к ревью и merge request'у (по правилам `AGENTS.md` — только после явного одобрения владельцем проекта, с назначением на `mrundead`).

---

## Self-Review

**Покрытие спеки.** Три фазы и таймауты — задачи 5 и 1; `ssh.exe` как транспорт и его отсутствие — задачи 4 и 5; коды возврата 0/1/2/255 — задача 3; транскрипт в файл, а не в pipe — задача 6; шаги сборки — задача 2; полный цикл и передача `Com1Session` — задачи 8-10; правила отката — задача 9; настройки — задачи 1 и 10; хвост `com1.log` — задачи 6 и 9; `#[ignore]`-тест и ARCHITECTURE.md — задача 10; ручная проверка — задача 11.

**Согласованность имён.** `GuestReadinessTimeouts` — только в `vmlord-core` (настройки); `ReadinessTimeouts` — только в `vmlord-platform` (длительности); `GuestReadiness` — сам ожидатель. `outcome`, `tail`, `com1_tail`, `ssh_invocation`, `ssh_client_path`, `probe_port_at`, `run_ssh`, `StartedVm`, `take_started`, `adopt_started`, `CycleOutcome`, `CycleReport`, `VmBuildCycle` — каждое определено ровно в одной задаче и используется в последующих под тем же именем.

**Решённые на бумаге узкие места.** `with_readiness_timeouts` не пересобирает цикл целиком — `CloudDiskImporter` неклонируем, — а меняет через `VmBuildCycle::set_timeouts` только `GuestReadiness` (задачи 9 и 10). `Com1Session` едет на главный поток благодаря `unsafe impl Send for WindowsEvent` (задача 7) — единственный новый `unsafe` в работе, и он в крейте, где `unsafe_code` разрешён. `take_started` отделён от `reap`, потому что `reap` вызывается из методов на `&self`, а брошенная там сессия молча погасила бы читатель COM1 работающей VM (задача 8).
