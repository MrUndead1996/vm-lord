# TASK-50: загрузка cloud-образа с кешем и SHA256 — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** VMLord скачивает cloud-образ по HTTPS в кеш на диске, проверяет SHA256 при каждом попадании в кеш, докачивает прерванную загрузку через HTTP Range и публикует прогресс снимком, который UI-поток сможет читать.

**Architecture:** Новый кроссплатформенный крейт `vmlord-image` содержит блокирующий загрузчик на `ureq`; контракт прогресса (`DownloadPhase`, `ProgressPublisher`, `ProgressThrottle`) живёт в `vmlord-core`, чтобы UI мог его читать, не завися от HTTP-клиента. Кеш адресуется содержимым: имя файла — это ожидаемая контрольная сумма. Гонку двух загрузчиков закрывает эксклюзивный лок ОС на `.part` через `std::fs::File::try_lock`.

**Tech Stack:** Rust 2024, `ureq` 3.4 (`default-features = false`), `sha2` 0.11, `std::net::TcpListener` для тестов, крейты `vmlord-core` и новый `vmlord-image`.

**Спека:** `docs/superpowers/specs/2026-08-09-cloud-image-download-design.md`

## Global Constraints

- Ветка: `task-50-image-download` (уже создана, спека в ней закоммичена).
- Коммиты: `TASK-50: <comment>`, автор задаётся переменными окружения `GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local`.
- Комментарии, doc-комментарии, имена тестов и сообщения об ошибках — на английском, как весь код репозитория.
- Логирование через крейт `log`, уровни DEBUG..ERROR. `TRACE` не используется.
- `unsafe` запрещён: в `vmlord-image` и `vmlord-core` действует `unsafe_code = "deny"` из `[workspace.lints.rust]`. Ни одна задача плана его не требует.
- Никаких `anyhow`, `thiserror`, `Box<dyn Error>`: ошибки — руками написанные перечисления с `Display` и `std::error::Error`, по образцу `SettingsError` (`crates/core/src/settings.rs:150-211`).
- Никакого async: ни `tokio`, ни `.await` в проекте нет и не появляется.
- Тесты гоняются нативно под Linux — новый крейт кроссплатформенный: `cargo test -p vmlord-core -p vmlord-image`.
- Сборка всего workspace проверяется под целевой платформой: `cargo build --target=x86_64-pc-windows-gnu`. Проверено заранее, что `ureq` + `rustls` + `ring` собираются этим таргетом на этой машине.
- Тулчейн 1.97.1. `std::fs::File::try_lock` стабилен с 1.89 — проверено на месте, зависимость для локов не нужна.
- Перед возвратом ошибки она пишется в `log::error!`.

### Факты, проверенные экспериментом до написания плана

Эти четыре пункта уже подтверждены реальной сборкой и запуском; не переоткрывать их заново, но и не «чинить» вопреки им.

1. `ureq` шлёт имена заголовков **в нижнем регистре**: `range: bytes=1000-`. Тестовый сервер обязан сравнивать имена без учёта регистра, иначе он ответит 200 вместо 206, и тест на докачку будет зелёным, проверяя не то.
2. Набор фич по умолчанию включает `gzip`, и тогда уходит `accept-encoding: gzip`, а тело распаковывается прозрачно — счётчик байт разойдётся с файлом. Поэтому `default-features = false`. С ним заголовок не уходит.
3. Обрыв соединения на середине тела даёт `io::Error` с `kind() == UnexpectedEof` и текстом `Peer disconnected`, а не тихое короткое чтение.
4. `File::try_lock` конфликтует и внутри одного процесса: второй `File::open` того же пути возвращает `Err(TryLockError::WouldBlock)`. Ошибка разбирается как `std::fs::TryLockError::{WouldBlock, Error(io::Error)}`.
5. В `sha2` 0.11 результат `finalize()` **не** реализует `LowerHex`, так что `format!("{digest:x}")` не компилируется. Hex собирается руками: `digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()`.

---

## File Structure

**Создаются:**

- `crates/image/Cargo.toml` — манифест нового крейта.
- `crates/image/src/lib.rs` — документация крейта и реэкспорты публичного API.
- `crates/image/src/error.rs` — `DownloadError`.
- `crates/image/src/cache.rs` — имена файлов кеша, нормализация суммы, хеширование файла с прогрессом.
- `crates/image/src/part.rs` — `PartFile`: открытие `.part`, эксклюзивный лок, усечение, дозапись.
- `crates/image/src/http.rs` — построение `Agent`, `resume_decision`, разбор `Content-Range`.
- `crates/image/src/download.rs` — `fetch_image`, оркестрация всего перечисленного.
- `crates/core/src/progress.rs` — `DownloadPhase`, `ProgressPublisher`, `ProgressThrottle`.
- `crates/image/tests/support/mod.rs` — петлевой HTTP-сервер для интеграционных тестов.
- `crates/image/tests/download.rs` — свежая загрузка, кеш, сумма, переименование, гонки, отмена.
- `crates/image/tests/resume.rs` — ветки `Range`: 206, игнорирование, 416, обрыв.

**Изменяются:**

- `Cargo.toml:2-9` — новый член workspace.
- `crates/core/src/lib.rs:3-7` — модуль `progress` и его реэкспорты.
- `crates/core/src/settings.rs` — поле `image_cache_path`, его достройка и создание каталога.
- `crates/ui/src/lib.rs:66-104` — `SettingsForm` проносит `image_cache_path` через диалог, не теряя его.
- `crates/app/src/lib.rs:773-784` — литералы `AppSettings` в тестах.
- `ARCHITECTURE.md` — раздел о загрузчике образов.

---

### Task 1: Контракт прогресса в `vmlord-core`

**Files:**
- Create: `crates/core/src/progress.rs`
- Modify: `crates/core/src/lib.rs:3-7`

**Interfaces:**
- Consumes: ничего.
- Produces:
  - `pub enum DownloadPhase { Connecting, Downloading { downloaded: u64, total: Option<u64> }, Verifying { hashed: u64, total: u64 }, Completed }`, выводит `Clone, Copy, Debug, PartialEq, Eq`
  - `pub struct ProgressPublisher` — `Clone + Default`, методы `publish(&self, phase: DownloadPhase)` и `snapshot(&self) -> Option<DownloadPhase>`
  - `pub struct ProgressThrottle` с `new(publisher: ProgressPublisher) -> Self`, `with_interval(publisher: ProgressPublisher, min_interval: Duration) -> Self`, `publish(&mut self, phase: DownloadPhase)`, `publish_now(&mut self, phase: DownloadPhase)`
  - реэкспорт из `vmlord_core`: `pub use progress::{DownloadPhase, ProgressPublisher, ProgressThrottle};`

- [ ] **Step 1: Написать падающие тесты**

Создать `crates/core/src/progress.rs` и положить в него только тесты:

```rust
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DownloadPhase, ProgressPublisher, ProgressThrottle};

    #[test]
    fn a_publisher_starts_empty_and_then_reports_the_last_phase() {
        let publisher = ProgressPublisher::default();
        assert_eq!(publisher.snapshot(), None);

        publisher.publish(DownloadPhase::Connecting);
        publisher.publish(DownloadPhase::Downloading {
            downloaded: 10,
            total: Some(100),
        });

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Downloading {
                downloaded: 10,
                total: Some(100),
            }),
            "a later snapshot replaces the earlier one rather than queueing behind it"
        );
    }

    #[test]
    fn a_clone_of_a_publisher_shares_the_snapshot() {
        let publisher = ProgressPublisher::default();
        let worker_side = publisher.clone();

        worker_side.publish(DownloadPhase::Completed);

        assert_eq!(publisher.snapshot(), Some(DownloadPhase::Completed));
    }

    #[test]
    fn a_publisher_survives_a_panic_in_another_holder() {
        let publisher = ProgressPublisher::default();
        let poisoner = publisher.clone();
        let _ = std::thread::spawn(move || {
            poisoner.publish(DownloadPhase::Connecting);
            panic!("a worker panicked while VMLord was downloading");
        })
        .join();

        publisher.publish(DownloadPhase::Completed);

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Completed),
            "losing all progress reporting because an unrelated thread panicked \
             would be worse than reading through the poisoned lock"
        );
    }

    #[test]
    fn a_throttle_without_an_interval_publishes_everything() {
        let publisher = ProgressPublisher::default();
        let mut throttle = ProgressThrottle::with_interval(publisher.clone(), Duration::ZERO);

        throttle.publish(DownloadPhase::Downloading {
            downloaded: 1,
            total: None,
        });
        throttle.publish(DownloadPhase::Downloading {
            downloaded: 2,
            total: None,
        });

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Downloading {
                downloaded: 2,
                total: None,
            })
        );
    }

    #[test]
    fn a_throttle_drops_a_repeat_of_the_same_phase_inside_the_interval() {
        let publisher = ProgressPublisher::default();
        let mut throttle =
            ProgressThrottle::with_interval(publisher.clone(), Duration::from_secs(3600));

        throttle.publish(DownloadPhase::Downloading {
            downloaded: 1,
            total: None,
        });
        throttle.publish(DownloadPhase::Downloading {
            downloaded: 2,
            total: None,
        });

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Downloading {
                downloaded: 1,
                total: None,
            }),
            "publishing every read would take the lock tens of thousands of times per image"
        );
    }

    #[test]
    fn a_throttle_never_delays_a_change_of_phase() {
        let publisher = ProgressPublisher::default();
        let mut throttle =
            ProgressThrottle::with_interval(publisher.clone(), Duration::from_secs(3600));

        throttle.publish(DownloadPhase::Downloading {
            downloaded: 1,
            total: None,
        });
        throttle.publish(DownloadPhase::Verifying {
            hashed: 0,
            total: 1,
        });

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Verifying {
                hashed: 0,
                total: 1,
            }),
            "a throttled phase change would leave the UI stuck at 97% forever"
        );
    }

    #[test]
    fn publish_now_ignores_the_interval() {
        let publisher = ProgressPublisher::default();
        let mut throttle =
            ProgressThrottle::with_interval(publisher.clone(), Duration::from_secs(3600));

        throttle.publish(DownloadPhase::Downloading {
            downloaded: 1,
            total: Some(2),
        });
        throttle.publish_now(DownloadPhase::Downloading {
            downloaded: 2,
            total: Some(2),
        });

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Downloading {
                downloaded: 2,
                total: Some(2),
            }),
            "the last value of a phase must land, or the bar stops short of the end"
        );
    }
}
```

Подключить модуль в `crates/core/src/lib.rs`, добавив к существующим строкам 3-7:

```rust
pub mod logging;
pub mod progress;
pub mod settings;

pub use logging::{LoggingError, initialize as initialize_logging};
pub use progress::{DownloadPhase, ProgressPublisher, ProgressThrottle};
pub use settings::{AppSettings, Language, LogLevel, SettingsError, SettingsStore};
```

- [ ] **Step 2: Убедиться, что тесты не компилируются**

Run: `cargo test -p vmlord-core`
Expected: FAIL, `cannot find type DownloadPhase in this scope` и аналогичные.

- [ ] **Step 3: Реализовать модуль**

Вставить перед блоком `#[cfg(test)]` в `crates/core/src/progress.rs`:

```rust
//! Progress a long-running operation publishes for the thread that draws it.
//!
//! Progress is a level rather than a stream of events: only the latest value
//! matters, and a value the UI never got round to reading costs nothing. That
//! is why this is a single overwritten slot and not the queue `VmEventSink`
//! uses for HCS events, where every event is a distinct fact whose loss is the
//! loss of information.

use std::{
    mem::Discriminant,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// What a download is doing right now.
///
/// There is no `Failed` variant on purpose. A failure is the `Err` of the
/// operation, and mirroring it here would create a second source of truth that
/// can disagree with the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadPhase {
    /// The request has been made and no byte of the body has arrived yet.
    Connecting,
    /// Bytes are arriving. `total` is `None` when the server sent no length.
    Downloading { downloaded: u64, total: Option<u64> },
    /// The bytes on disk are being hashed to check them against the expected sum.
    Verifying { hashed: u64, total: u64 },
    /// The image is in the cache and verified.
    Completed,
}

/// The slot a worker writes progress into and the UI thread reads.
///
/// Cloning shares the slot: the worker holds one clone while the UI reads
/// through another.
#[derive(Clone, Default)]
pub struct ProgressPublisher(Arc<Mutex<Option<DownloadPhase>>>);

impl ProgressPublisher {
    /// Replaces whatever the last reported phase was.
    pub fn publish(&self, phase: DownloadPhase) {
        *self.lock() = Some(phase);
    }

    /// Reports the last published phase, or `None` before the first one.
    #[must_use]
    pub fn snapshot(&self) -> Option<DownloadPhase> {
        *self.lock()
    }

    /// Recovers a poisoned lock rather than propagating the panic.
    ///
    /// The slot holds a plain `Copy` value that a panic elsewhere cannot leave
    /// half-written, and losing all progress reporting because an unrelated
    /// thread panicked would be worse than reading it.
    fn lock(&self) -> MutexGuard<'_, Option<DownloadPhase>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Rate-limits publishing so a read loop does not take the lock per chunk.
///
/// A download reads in 64 KiB chunks, so an image of a few hundred megabytes
/// would otherwise publish thousands of times. What the UI can actually show is
/// bounded by its frame rate, so anything faster is waste.
pub struct ProgressThrottle {
    publisher: ProgressPublisher,
    min_interval: Duration,
    last: Option<(Instant, Discriminant<DownloadPhase>)>,
}

impl ProgressThrottle {
    /// How long two reports of the same phase must be apart.
    pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);

    #[must_use]
    pub fn new(publisher: ProgressPublisher) -> Self {
        Self::with_interval(publisher, Self::DEFAULT_INTERVAL)
    }

    #[must_use]
    pub fn with_interval(publisher: ProgressPublisher, min_interval: Duration) -> Self {
        Self {
            publisher,
            min_interval,
            last: None,
        }
    }

    /// Publishes `phase`, unless the same kind of phase was published less than
    /// `min_interval` ago.
    ///
    /// A change of phase is never delayed: it is the transition the UI needs to
    /// see, and holding it back is what leaves a progress bar stuck just short
    /// of the end.
    pub fn publish(&mut self, phase: DownloadPhase) {
        let kind = std::mem::discriminant(&phase);
        if let Some((published_at, last_kind)) = self.last
            && last_kind == kind
            && published_at.elapsed() < self.min_interval
        {
            return;
        }
        self.publish_now(phase);
    }

    /// Publishes `phase` whatever the interval says.
    ///
    /// Used for the last value of a phase, which must land even if the
    /// preceding one was moments ago.
    pub fn publish_now(&mut self, phase: DownloadPhase) {
        self.publisher.publish(phase);
        self.last = Some((Instant::now(), std::mem::discriminant(&phase)));
    }
}
```

- [ ] **Step 4: Прогнать тесты**

Run: `cargo test -p vmlord-core`
Expected: PASS, семь новых тестов в `progress::tests` плюс семь существующих.

- [ ] **Step 5: Проверить линты и сборку под целевой таргет**

Run: `cargo clippy -p vmlord-core --all-targets -- -D warnings`
Expected: без предупреждений.

Run: `cargo build -p vmlord-core --target=x86_64-pc-windows-gnu`
Expected: успех.

- [ ] **Step 6: Коммит**

```bash
git add crates/core/src/progress.rs crates/core/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-50: Publish download progress as a snapshot the UI can read"
```

---

### Task 2: Каталог кеша образов в настройках

**Files:**
- Modify: `crates/core/src/settings.rs:8-24` (константа и поле), `:76-93` (`load_or_create`), `:96-127` (`save`), `:129-139` (`default_settings`), `:213-269` (тесты)
- Modify: `crates/ui/src/lib.rs:66-104` (`SettingsForm`)
- Modify: `crates/app/src/lib.rs:773-784` (литералы в тесте)

**Interfaces:**
- Consumes: ничего.
- Produces: поле `AppSettings.image_cache_path: PathBuf`, по умолчанию `<каталог настроек>/images`.

- [ ] **Step 1: Написать падающие тесты**

В `crates/core/src/settings.rs`, в модуле `tests`, добавить:

```rust
    #[test]
    fn defaults_put_the_image_cache_next_to_the_other_application_directories() {
        let directory = temporary_directory();
        let store = SettingsStore::new(directory.join("settings.toml"));

        let settings = store.load_or_create().unwrap();

        assert_eq!(settings.image_cache_path, directory.join("images"));
        assert!(settings.image_cache_path.is_dir());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn settings_written_before_the_image_cache_existed_still_load() {
        let directory = temporary_directory();
        fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("settings.toml");
        fs::write(
            &config_path,
            format!(
                "vm_storage_path = {vms:?}\n\
                 language = \"en-US\"\n\
                 log_file_path = {log:?}\n\
                 log_level = \"info\"\n",
                vms = directory.join("vms").display().to_string(),
                log = directory.join("vmlord.log").display().to_string(),
            ),
        )
        .unwrap();

        let settings = SettingsStore::new(&config_path).load_or_create().unwrap();

        assert_eq!(
            settings.image_cache_path,
            directory.join("images"),
            "an existing settings.toml must keep loading without a migration"
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn an_explicit_image_cache_path_is_preserved() {
        let directory = temporary_directory();
        let store = SettingsStore::new(directory.join("settings.toml"));
        let settings = AppSettings {
            vm_storage_path: directory.join("vms"),
            language: Language::EnUs,
            log_file_path: directory.join("logs").join("vmlord.log"),
            log_level: LogLevel::Info,
            image_cache_path: directory.join("elsewhere").join("images"),
        };

        store.save(&settings).unwrap();

        assert_eq!(store.load_or_create().unwrap(), settings);
        assert!(settings.image_cache_path.is_dir());

        fs::remove_dir_all(directory).unwrap();
    }
```

- [ ] **Step 2: Прогнать и убедиться, что не компилируется**

Run: `cargo test -p vmlord-core`
Expected: FAIL, `struct AppSettings has no field named image_cache_path`.

- [ ] **Step 3: Добавить поле и его достройку**

В `crates/core/src/settings.rs` рядом с существующими константами:

```rust
const DEFAULT_IMAGE_DIRECTORY: &str = "images";
```

В `AppSettings`, после `log_level`:

```rust
    /// Directory holding distribution images downloaded from the internet.
    ///
    /// `serde(default)` leaves this empty for a `settings.toml` written before
    /// the field existed; `load_or_create` fills it in. That keeps existing
    /// configurations loading without a migration, the same way `endpoint_id`
    /// and `network_mode` are handled in `VmComputeSystemMapping`.
    #[serde(default)]
    pub image_cache_path: PathBuf,
```

В `load_or_create` заменить ветку `Ok(contents)`:

```rust
            Ok(contents) => {
                let mut settings: AppSettings =
                    toml::from_str(&contents).map_err(|source| SettingsError::Parse {
                        path: self.config_path.clone(),
                        source,
                    })?;
                if settings.image_cache_path.as_os_str().is_empty() {
                    settings.image_cache_path =
                        self.config_directory()?.join(DEFAULT_IMAGE_DIRECTORY);
                    log::debug!(
                        "settings carried no image cache path; defaulting to {}",
                        settings.image_cache_path.display()
                    );
                }
                Ok(settings)
            }
```

В `default_settings`, в литерал `AppSettings`, добавить:

```rust
            image_cache_path: config_directory.join(DEFAULT_IMAGE_DIRECTORY),
```

В `save`, рядом с созданием каталога VM:

```rust
        fs::create_dir_all(&settings.image_cache_path).map_err(|source| SettingsError::Io {
            operation: "create image cache directory",
            path: settings.image_cache_path.clone(),
            source,
        })?;
```

- [ ] **Step 4: Починить существующие литералы `AppSettings`**

`crates/core/src/settings.rs:257` — в тесте `save_and_load_preserves_custom_settings` добавить в литерал:

```rust
            image_cache_path: directory.join("images"),
```

`crates/app/src/lib.rs:773` и `:779` — в оба литерала теста `updates_and_persists_application_settings` добавить соответственно:

```rust
            image_cache_path: directory.join("images"),
```

```rust
            image_cache_path: directory.join("cached-images"),
```

- [ ] **Step 5: Провести путь через диалог настроек, не потеряв его**

`crates/ui/src/lib.rs:66-104`. Диалог настроек собирает `AppSettings` целиком из полей формы, поэтому новое поле, которого в форме нет, будет затираться пустым при каждом сохранении, а `load_or_create` затем молча подменит пользовательский путь дефолтным. Форма обязана пронести значение через себя, хотя виджета для него пока нет — он появится вместе с UI загрузки.

В `struct SettingsForm` добавить поле:

```rust
    /// Carried through the dialog unchanged: the settings form rebuilds the
    /// whole `AppSettings`, so a field it does not know about would be lost on
    /// every save. The widget for it arrives with the image download UI.
    image_cache_path: PathBuf,
```

В `SettingsForm::from_settings`:

```rust
            image_cache_path: settings.image_cache_path.clone(),
```

В `SettingsForm::settings`, в литерал `AppSettings`:

```rust
            image_cache_path: self.image_cache_path.clone(),
```

- [ ] **Step 6: Прогнать тесты**

Run: `cargo test -p vmlord-core -p vmlord-app`
Expected: PASS, включая три новых теста.

Run: `cargo build --target=x86_64-pc-windows-gnu`
Expected: успех — это единственная проверка того, что правка `crates/ui` компилируется, потому что `eframe` под Linux в этом workspace не собирается.

- [ ] **Step 7: Коммит**

```bash
git add crates/core/src/settings.rs crates/ui/src/lib.rs crates/app/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-50: Give downloaded images a configurable cache directory"
```

---

### Task 3: Крейт `vmlord-image`, ошибки и имена в кеше

**Files:**
- Create: `crates/image/Cargo.toml`, `crates/image/src/lib.rs`, `crates/image/src/error.rs`, `crates/image/src/cache.rs`
- Modify: `Cargo.toml:2-9`

**Interfaces:**
- Consumes: `vmlord_core::{DownloadPhase, ProgressThrottle}` из Task 1.
- Produces:
  - `pub enum DownloadError { Io { operation: &'static str, path: PathBuf, source: io::Error }, Http(String), UnexpectedStatus { status: u16 }, ChecksumMismatch { expected: String, actual: String }, AlreadyInProgress { path: PathBuf }, Cancelled, InvalidChecksum(String) }`
  - `pub(crate) fn io_error(operation: &'static str, path: &Path) -> impl FnOnce(io::Error) -> DownloadError`
  - `pub(crate) fn normalized_checksum(value: &str) -> Result<String, DownloadError>`
  - `pub(crate) fn cache_file_name(url: &str, checksum: &str) -> String`
  - `pub(crate) fn file_checksum(path: &Path, progress: &mut ProgressThrottle, cancel: &AtomicBool) -> Result<String, DownloadError>`

- [ ] **Step 1: Завести крейт**

`Cargo.toml` в корне, список `members`, добавить `"crates/image",` после `"crates/core",`.

Создать `crates/image/Cargo.toml`:

```toml
[package]
name = "vmlord-image"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
log.workspace = true
sha2 = "0.11"
ureq = { version = "3.4", default-features = false, features = [
    "rustls",
    "platform-verifier",
    "win-system-proxy",
] }
vmlord-core = { path = "../core" }

[lints]
workspace = true
```

Фича `gzip` отключена намеренно: с ней `ureq` шлёт `accept-encoding: gzip` и прозрачно распаковывает тело, после чего число прочитанных байт перестаёт совпадать с числом байт в файле и смещение докачки уезжает.

Создать `crates/image/src/lib.rs`:

```rust
//! Getting a distribution's cloud image onto disk, intact.
//!
//! The cache is addressed by content: a file is named after the SHA256 it is
//! expected to have. Two releases can therefore never collide on a name, and a
//! file whose name and content disagree cannot exist.
//!
//! Trust comes from HTTPS, not from the checksum. A list of sums downloaded
//! from the same server as the image proves nothing about authenticity:
//! whoever could swap the image could swap the list. The checksum is an
//! integrity check, and above all the one defence against a file left truncated
//! by an interrupted download.

mod cache;
mod error;

pub use error::DownloadError;
```

- [ ] **Step 2: Написать падающие тесты**

Создать `crates/image/src/cache.rs`, пока только с тестами:

```rust
#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };

    use vmlord_core::{DownloadPhase, ProgressPublisher, ProgressThrottle};

    use super::{cache_file_name, file_checksum, normalized_checksum};
    use crate::error::DownloadError;

    const SUM: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn temporary_directory(tag: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vmlord-image-{tag}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_checksum_is_accepted_in_either_case_and_kept_lowercase() {
        assert_eq!(normalized_checksum(&SUM.to_uppercase()).unwrap(), SUM);
        assert_eq!(normalized_checksum(&format!("  {SUM}  ")).unwrap(), SUM);
    }

    #[test]
    fn a_checksum_of_the_wrong_shape_is_refused_before_anything_is_downloaded() {
        for candidate in ["", "abc", &SUM[1..], &format!("{}z", &SUM[1..])] {
            assert!(
                matches!(
                    normalized_checksum(candidate),
                    Err(DownloadError::InvalidChecksum(_))
                ),
                "{candidate:?} is not a SHA256"
            );
        }
    }

    #[test]
    fn a_cached_file_is_named_after_its_checksum_and_keeps_the_extension() {
        assert_eq!(
            cache_file_name("https://example.test/path/noble-cloudimg-amd64.img", SUM),
            format!("{SUM}.img")
        );
        assert_eq!(
            cache_file_name("https://example.test/image.qcow2?token=abc#frag", SUM),
            format!("{SUM}.qcow2")
        );
    }

    #[test]
    fn a_url_without_a_usable_extension_gives_a_bare_checksum() {
        assert_eq!(cache_file_name("https://example.test/download", SUM), SUM);
        assert_eq!(cache_file_name("https://example.test/", SUM), SUM);
        assert_eq!(
            cache_file_name("https://example.test/archive.tar.gz.part1of2", SUM),
            SUM,
            "an extension that is not a short alphanumeric word is not one"
        );
    }

    #[test]
    fn hashing_a_file_reports_the_sum_and_the_progress_of_getting_there() {
        let directory = temporary_directory("hash");
        let path = directory.join("payload");
        fs::write(&path, b"vmlord").unwrap();
        let publisher = ProgressPublisher::default();
        let mut throttle = ProgressThrottle::with_interval(publisher.clone(), Duration::ZERO);

        let sum = file_checksum(&path, &mut throttle, &AtomicBool::new(false)).unwrap();

        assert_eq!(
            sum, "c423e3a9d7b4a6f1f03492cfded44b0b9c00c4c63f1ef3c410368e8a9ad3bcd2",
            "the sum of the bytes \"vmlord\""
        );
        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Verifying {
                hashed: 6,
                total: 6
            })
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn hashing_stops_when_the_download_is_cancelled() {
        let directory = temporary_directory("hash-cancel");
        let path = directory.join("payload");
        fs::write(&path, vec![0u8; 4 * 1024 * 1024]).unwrap();
        let mut throttle = ProgressThrottle::with_interval(ProgressPublisher::default(), Duration::ZERO);
        let cancel = AtomicBool::new(false);
        cancel.store(true, Ordering::Relaxed);

        let error = file_checksum(&path, &mut throttle, &cancel)
            .expect_err("a cancelled verification must not report a sum");

        assert!(matches!(error, DownloadError::Cancelled));

        fs::remove_dir_all(directory).unwrap();
    }
}
```

- [ ] **Step 3: Прогнать и убедиться, что падает**

Run: `cargo test -p vmlord-image`
Expected: FAIL, `cannot find function cache_file_name` и соседние.

- [ ] **Step 4: Реализовать `error.rs`**

```rust
//! Everything that can go wrong fetching an image, and how it reads.

use std::{fmt, io, path::Path, path::PathBuf};

/// A failure fetching a distribution image.
#[derive(Debug)]
pub enum DownloadError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// The transport failed: connection refused, TLS rejected, body cut short.
    Http(String),
    UnexpectedStatus {
        status: u16,
    },
    ChecksumMismatch {
        expected: String,
        actual: String,
    },
    /// Another downloader holds the lock on this image's partial file.
    AlreadyInProgress {
        path: PathBuf,
    },
    Cancelled,
    /// The caller supplied something that is not a SHA256.
    InvalidChecksum(String),
}

impl fmt::Display for DownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} at {}: {source}",
                path.display()
            ),
            Self::Http(message) => write!(formatter, "the image request failed: {message}"),
            Self::UnexpectedStatus { status } => {
                write!(formatter, "the image server answered with status {status}")
            }
            Self::ChecksumMismatch { expected, actual } => write!(
                formatter,
                "the downloaded image hashes to {actual} instead of the expected {expected}"
            ),
            Self::AlreadyInProgress { path } => write!(
                formatter,
                "another download of this image is already running; it holds {}",
                path.display()
            ),
            Self::Cancelled => formatter.write_str("the image download was cancelled"),
            Self::InvalidChecksum(value) => {
                write!(formatter, "{value:?} is not a SHA256 checksum")
            }
        }
    }
}

impl std::error::Error for DownloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Http(_)
            | Self::UnexpectedStatus { .. }
            | Self::ChecksumMismatch { .. }
            | Self::AlreadyInProgress { .. }
            | Self::Cancelled
            | Self::InvalidChecksum(_) => None,
        }
    }
}

/// Builds the `Io` variant for a fallible filesystem call.
///
/// Written as a closure factory so call sites read
/// `.map_err(io_error("open the partial download", &path))?`.
pub(crate) fn io_error(
    operation: &'static str,
    path: &Path,
) -> impl FnOnce(io::Error) -> DownloadError + use<> {
    let path = path.to_path_buf();
    move |source| DownloadError::Io {
        operation,
        path,
        source,
    }
}
```

- [ ] **Step 5: Реализовать `cache.rs`**

Вставить перед блоком `#[cfg(test)]`:

```rust
//! Where a downloaded image lives on disk and how its integrity is checked.

use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use sha2::{Digest, Sha256};
use vmlord_core::{DownloadPhase, ProgressThrottle};

use crate::error::{DownloadError, io_error};

/// How much is hashed between two cancellation checks.
const HASH_CHUNK: usize = 1024 * 1024;

/// The number of hex characters in a SHA256.
const CHECKSUM_LENGTH: usize = 64;

/// The longest extension taken from a URL, in characters.
const MAX_EXTENSION: usize = 8;

/// Accepts a SHA256 in either case and returns it lowercase.
///
/// Checked before any request goes out: a caller that got the checksum wrong
/// should learn it in milliseconds, not after several hundred megabytes.
pub(crate) fn normalized_checksum(value: &str) -> Result<String, DownloadError> {
    let trimmed = value.trim();
    if trimmed.len() != CHECKSUM_LENGTH || !trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DownloadError::InvalidChecksum(trimmed.to_owned()));
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// Names the cache entry for an image: its checksum, plus the extension the URL
/// carried so the file stays recognisable to a human and to tooling.
///
/// The name is the content, so two releases cannot collide and a file whose
/// name disagrees with its content cannot exist.
pub(crate) fn cache_file_name(url: &str, checksum: &str) -> String {
    match url_extension(url) {
        Some(extension) => format!("{checksum}.{extension}"),
        None => checksum.to_owned(),
    }
}

/// The extension of the last path segment of `url`, when it looks like one.
///
/// Anything that is not a short alphanumeric word is rejected rather than
/// pasted into a filename: the URL is attacker-influenced input.
fn url_extension(url: &str) -> Option<&str> {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let segment = path.rsplit('/').next()?;
    let extension = segment.rsplit_once('.')?.1;
    let sane = !extension.is_empty()
        && extension.len() <= MAX_EXTENSION
        && extension.chars().all(|character| character.is_ascii_alphanumeric());
    sane.then_some(extension)
}

/// Hashes the file at `path`, reporting progress and honouring cancellation.
///
/// Run on every cache hit, not only after a download: it is the only thing that
/// tells a complete image from one an interrupted download left truncated, and
/// it costs seconds against re-fetching hundreds of megabytes.
pub(crate) fn file_checksum(
    path: &Path,
    progress: &mut ProgressThrottle,
    cancel: &AtomicBool,
) -> Result<String, DownloadError> {
    let mut file = File::open(path).map_err(io_error("open the image for hashing", path))?;
    let total = file
        .metadata()
        .map_err(io_error("measure the image", path))?
        .len();
    log::debug!("hashing {} ({total} bytes)", path.display());

    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_CHUNK];
    let mut hashed = 0u64;
    progress.publish_now(DownloadPhase::Verifying { hashed, total });
    loop {
        if cancel.load(Ordering::Relaxed) {
            log::debug!("hashing {} was cancelled", path.display());
            return Err(DownloadError::Cancelled);
        }
        let read = file
            .read(&mut buffer)
            .map_err(io_error("read the image for hashing", path))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        hashed += read as u64;
        progress.publish(DownloadPhase::Verifying { hashed, total });
    }
    progress.publish_now(DownloadPhase::Verifying { hashed, total });

    let digest = hasher.finalize();
    let checksum: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    log::debug!("{} hashes to {checksum}", path.display());
    Ok(checksum)
}
```

Заметь: в `sha2` 0.11 результат `finalize()` не реализует `LowerHex`, поэтому hex собирается вручную; `format!("{digest:x}")` не скомпилируется.

Добавить `mod cache;` уже есть в `lib.rs` из шага 1.

- [ ] **Step 6: Прогнать тесты**

Run: `cargo test -p vmlord-image`
Expected: PASS, шесть тестов.

Run: `cargo clippy -p vmlord-image --all-targets -- -D warnings`
Expected: без предупреждений.

- [ ] **Step 7: Коммит**

```bash
git add Cargo.toml Cargo.lock crates/image
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-50: Address the image cache by the checksum of its contents"
```

---

### Task 4: `PartFile` и эксклюзивный лок

**Files:**
- Create: `crates/image/src/part.rs`
- Modify: `crates/image/src/lib.rs`

**Interfaces:**
- Consumes: `crate::error::{DownloadError, io_error}` из Task 3.
- Produces:
  - `pub(crate) struct PartFile`
  - `PartFile::open_locked(path: PathBuf) -> Result<Self, DownloadError>`
  - `PartFile::path(&self) -> &Path`
  - `PartFile::len(&self) -> Result<u64, DownloadError>`
  - `PartFile::truncate(&mut self) -> Result<(), DownloadError>`
  - `PartFile::seek_to_end(&mut self) -> Result<u64, DownloadError>`
  - `PartFile::write_all(&mut self, bytes: &[u8]) -> Result<(), DownloadError>`
  - `PartFile::sync(&self) -> Result<(), DownloadError>`

- [ ] **Step 1: Написать падающие тесты**

Создать `crates/image/src/part.rs` с тестами:

```rust
#[cfg(test)]
mod tests {
    use std::{fs, thread};

    use super::PartFile;
    use crate::error::DownloadError;

    fn temporary_directory(tag: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vmlord-part-{tag}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_fresh_partial_file_starts_empty_and_takes_what_is_written_to_it() {
        let directory = temporary_directory("write");
        let mut part = PartFile::open_locked(directory.join("image.part")).unwrap();

        assert_eq!(part.len().unwrap(), 0);
        part.seek_to_end().unwrap();
        part.write_all(b"first").unwrap();
        part.write_all(b"-second").unwrap();
        part.sync().unwrap();

        assert_eq!(part.len().unwrap(), 12);
        assert_eq!(fs::read(part.path()).unwrap(), b"first-second");

        drop(part);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reopening_a_partial_file_sees_what_the_last_attempt_left() {
        let directory = temporary_directory("resume");
        let path = directory.join("image.part");
        let mut first = PartFile::open_locked(path.clone()).unwrap();
        first.seek_to_end().unwrap();
        first.write_all(b"half").unwrap();
        first.sync().unwrap();
        drop(first);

        let mut second = PartFile::open_locked(path).unwrap();

        assert_eq!(
            second.len().unwrap(),
            4,
            "the whole point of a stable .part name is that the next run can resume it"
        );
        second.seek_to_end().unwrap();
        second.write_all(b"-rest").unwrap();
        second.sync().unwrap();
        assert_eq!(fs::read(second.path()).unwrap(), b"half-rest");

        drop(second);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn truncating_empties_the_file_without_dropping_the_lock() {
        let directory = temporary_directory("truncate");
        let mut part = PartFile::open_locked(directory.join("image.part")).unwrap();
        part.seek_to_end().unwrap();
        part.write_all(b"stale bytes").unwrap();

        part.truncate().unwrap();

        assert_eq!(part.len().unwrap(), 0);
        part.seek_to_end().unwrap();
        part.write_all(b"fresh").unwrap();
        part.sync().unwrap();
        assert_eq!(fs::read(part.path()).unwrap(), b"fresh");

        drop(part);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_second_downloader_of_the_same_image_is_turned_away() {
        let directory = temporary_directory("lock");
        let path = directory.join("image.part");
        let held = PartFile::open_locked(path.clone()).unwrap();

        let error = PartFile::open_locked(path.clone())
            .expect_err("two downloaders must not write into one partial file");

        assert!(
            matches!(error, DownloadError::AlreadyInProgress { .. }),
            "got {error:?}"
        );

        drop(held);
        PartFile::open_locked(path)
            .expect("the lock is released with the file, so the next attempt may resume");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_lock_is_held_against_another_thread_too() {
        let directory = temporary_directory("lock-thread");
        let path = directory.join("image.part");
        let held = PartFile::open_locked(path.clone()).unwrap();

        let contender = path.clone();
        let outcome = thread::spawn(move || PartFile::open_locked(contender).is_err())
            .join()
            .unwrap();

        assert!(
            outcome,
            "the lock has to cover two threads of one process, not just two processes"
        );

        drop(held);
        fs::remove_dir_all(directory).unwrap();
    }
}
```

- [ ] **Step 2: Прогнать и убедиться, что падает**

Run: `cargo test -p vmlord-image`
Expected: FAIL, `cannot find struct PartFile`.

- [ ] **Step 3: Реализовать**

Вставить перед `#[cfg(test)]` в `crates/image/src/part.rs`:

```rust
//! The partial download, and the lock that makes it one downloader's business.

use std::{
    fs::{File, TryLockError},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use crate::error::{DownloadError, io_error};

/// A `.part` file held under an exclusive OS lock for as long as it exists.
///
/// The name of a `.part` is derived from the image's checksum, so two
/// downloaders of one image aim at one file. Interleaving their writes would
/// not corrupt anything a caller can see -- the checksum catches it -- but it
/// wastes the whole download and reports the wrong reason. The lock turns that
/// into an immediate, accurate refusal.
///
/// The lock is the operating system's, taken on the open file: `LockFileEx`
/// with `LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY` on Windows,
/// `flock` elsewhere. It therefore covers two threads of one process as well as
/// two processes, and -- unlike a `.lock` marker file -- it is released when the
/// handle closes, including when the process dies. Nothing has to guess whether
/// a leftover lock is stale.
pub(crate) struct PartFile {
    file: File,
    path: PathBuf,
}

impl PartFile {
    /// Opens the partial download and claims it.
    ///
    /// Reports `AlreadyInProgress` rather than waiting: whether it is worth
    /// queueing behind another download is the caller's policy, and a wait
    /// buried in here would need a timeout invented out of nothing.
    pub(crate) fn open_locked(path: PathBuf) -> Result<Self, DownloadError> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_error("open the partial download", &path))?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                log::debug!("{} is locked by another downloader", path.display());
                return Err(DownloadError::AlreadyInProgress { path });
            }
            Err(TryLockError::Error(source)) => {
                return Err(DownloadError::Io {
                    operation: "lock the partial download",
                    path,
                    source,
                });
            }
        }

        Ok(Self { file, path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn len(&self) -> Result<u64, DownloadError> {
        Ok(self
            .file
            .metadata()
            .map_err(io_error("measure the partial download", &self.path))?
            .len())
    }

    /// Empties the file, keeping the handle and therefore the lock.
    ///
    /// Deleting and recreating would drop the lock for an instant and, on
    /// Windows, fight the still-open handle. Truncation does neither.
    pub(crate) fn truncate(&mut self) -> Result<(), DownloadError> {
        self.file
            .set_len(0)
            .map_err(io_error("truncate the partial download", &self.path))?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(io_error("rewind the partial download", &self.path))?;
        Ok(())
    }

    pub(crate) fn seek_to_end(&mut self) -> Result<u64, DownloadError> {
        self.file
            .seek(SeekFrom::End(0))
            .map_err(io_error("seek the partial download", &self.path))
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), DownloadError> {
        self.file
            .write_all(bytes)
            .map_err(io_error("write the partial download", &self.path))
    }

    pub(crate) fn sync(&self) -> Result<(), DownloadError> {
        self.file
            .sync_all()
            .map_err(io_error("flush the partial download", &self.path))
    }
}
```

Добавить `mod part;` в `crates/image/src/lib.rs` рядом с `mod cache;`.

- [ ] **Step 4: Прогнать тесты**

Run: `cargo test -p vmlord-image`
Expected: PASS, одиннадцать тестов.

- [ ] **Step 5: Коммит**

```bash
git add crates/image/src/part.rs crates/image/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-50: Let one downloader at a time own an image's partial file"
```

---

### Task 5: Разбор ответа сервера на `Range`

**Files:**
- Create: `crates/image/src/http.rs`
- Modify: `crates/image/src/lib.rs`

**Interfaces:**
- Consumes: `crate::error::DownloadError` из Task 3.
- Produces:
  - `pub(crate) enum ResumeOutcome { Append { total: Option<u64> }, StartOver, RangeUnsatisfiable }`
  - `pub(crate) fn resume_decision(status: u16, content_range: Option<&str>, requested_from: u64) -> Result<ResumeOutcome, DownloadError>`
  - `pub(crate) fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)>`
  - `pub(crate) fn build_agent() -> ureq::Agent`

- [ ] **Step 1: Написать падающие тесты**

Создать `crates/image/src/http.rs` с тестами:

```rust
#[cfg(test)]
mod tests {
    use super::{ResumeOutcome, parse_content_range, resume_decision};
    use crate::error::DownloadError;

    #[test]
    fn a_content_range_reports_its_bounds_and_total() {
        assert_eq!(
            parse_content_range("bytes 100-999/1000"),
            Some((100, 999, Some(1000)))
        );
        assert_eq!(
            parse_content_range("bytes 0-0/*"),
            Some((0, 0, None)),
            "a server may know the range without knowing the whole length"
        );
    }

    #[test]
    fn a_content_range_that_is_not_one_is_refused_rather_than_guessed() {
        for candidate in [
            "",
            "bytes",
            "items 1-2/3",
            "bytes 100/1000",
            "bytes abc-999/1000",
            "bytes 100-999",
        ] {
            assert_eq!(parse_content_range(candidate), None, "{candidate:?}");
        }
    }

    #[test]
    fn a_partial_content_answer_at_the_requested_offset_is_appended() {
        let outcome = resume_decision(206, Some("bytes 500-999/1000"), 500).unwrap();

        assert_eq!(outcome, ResumeOutcome::Append { total: Some(1000) });
    }

    #[test]
    fn a_partial_content_answer_at_another_offset_is_an_error() {
        let error = resume_decision(206, Some("bytes 0-999/1000"), 500)
            .expect_err("appending this to our 500 bytes would duplicate them");

        assert!(matches!(error, DownloadError::Http(_)), "got {error:?}");
    }

    #[test]
    fn a_partial_content_answer_without_a_usable_range_is_an_error() {
        assert!(matches!(
            resume_decision(206, None, 500),
            Err(DownloadError::Http(_))
        ));
        assert!(matches!(
            resume_decision(206, Some("bytes ???"), 500),
            Err(DownloadError::Http(_))
        ));
    }

    #[test]
    fn a_plain_ok_means_the_server_ignored_the_range() {
        assert_eq!(
            resume_decision(200, None, 500).unwrap(),
            ResumeOutcome::StartOver,
            "the body starts at byte zero, so what we already have is worthless"
        );
    }

    #[test]
    fn a_range_not_satisfiable_answer_asks_for_a_restart() {
        assert_eq!(
            resume_decision(416, None, 5_000).unwrap(),
            ResumeOutcome::RangeUnsatisfiable
        );
    }

    #[test]
    fn any_other_status_is_reported_as_it_came() {
        let error = resume_decision(404, None, 500).expect_err("404 is not a download");

        assert!(
            matches!(error, DownloadError::UnexpectedStatus { status: 404 }),
            "got {error:?}"
        );
    }
}
```

- [ ] **Step 2: Прогнать и убедиться, что падает**

Run: `cargo test -p vmlord-image`
Expected: FAIL, `cannot find function resume_decision`.

- [ ] **Step 3: Реализовать**

Вставить перед `#[cfg(test)]`:

```rust
//! Talking to the image server, and reading what it agreed to.

use std::time::Duration;

use ureq::{
    Agent,
    tls::{RootCerts, TlsConfig},
};

use crate::error::DownloadError;

/// How long a connection may take to establish.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the server may take to start answering.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// What the server agreed to send, given the range we asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResumeOutcome {
    /// The body continues where our partial file ends.
    Append { total: Option<u64> },
    /// The body starts at byte zero: whatever we had is of no use.
    StartOver,
    /// The server says the range lies past the end of the file.
    RangeUnsatisfiable,
}

/// Builds the client every image request goes through.
///
/// Certificate trust comes from the platform verifier, which on Windows means
/// the system certificate store: a machine behind a TLS-inspecting corporate
/// proxy trusts that proxy, and a client with its own compiled-in root list
/// would simply fail there with nothing the user can do.
///
/// Both timeouts are explicit. A server that accepts a connection and then says
/// nothing would otherwise park the worker thread forever.
pub(crate) fn build_agent() -> Agent {
    Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

/// Reads `Content-Range: bytes <first>-<last>/<complete>` into its parts.
///
/// A `*` for the complete length is legal and means the server does not know
/// it, which is reported as `None` rather than faked as a number.
pub(crate) fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let range = value.trim().strip_prefix("bytes ")?;
    let (bounds, complete) = range.split_once('/')?;
    let (first, last) = bounds.split_once('-')?;
    let first = first.trim().parse::<u64>().ok()?;
    let last = last.trim().parse::<u64>().ok()?;
    let complete = match complete.trim() {
        "*" => None,
        digits => Some(digits.parse::<u64>().ok()?),
    };
    Some((first, last, complete))
}

/// Decides what to do with the answer to a `Range` request.
///
/// Kept a plain function over a status and a header so the whole table can be
/// tested without a socket.
pub(crate) fn resume_decision(
    status: u16,
    content_range: Option<&str>,
    requested_from: u64,
) -> Result<ResumeOutcome, DownloadError> {
    match status {
        206 => {
            let Some((first, _, complete)) = content_range.and_then(parse_content_range) else {
                return Err(DownloadError::Http(format!(
                    "the server answered 206 without a usable Content-Range: {}",
                    content_range.unwrap_or("<missing>")
                )));
            };
            if first != requested_from {
                return Err(DownloadError::Http(format!(
                    "the server resumed at byte {first} instead of the requested {requested_from}"
                )));
            }
            Ok(ResumeOutcome::Append { total: complete })
        }
        200 => Ok(ResumeOutcome::StartOver),
        416 => Ok(ResumeOutcome::RangeUnsatisfiable),
        status => Err(DownloadError::UnexpectedStatus { status }),
    }
}
```

Добавить `mod http;` в `crates/image/src/lib.rs`.

- [ ] **Step 4: Прогнать тесты**

Run: `cargo test -p vmlord-image`
Expected: PASS, девятнадцать тестов.

- [ ] **Step 5: Коммит**

```bash
git add crates/image/src/http.rs crates/image/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-50: Read what the image server agreed to send"
```

---

### Task 6: Петлевой сервер и свежая загрузка

**Files:**
- Create: `crates/image/src/download.rs`, `crates/image/tests/support/mod.rs`, `crates/image/tests/download.rs`
- Modify: `crates/image/src/lib.rs`

**Interfaces:**
- Consumes: всё из Task 3-5.
- Produces:
  - `pub struct ImageDownloadRequest<'a> { pub url: &'a str, pub expected_sha256: &'a str, pub cache_directory: &'a Path }`
  - `pub fn fetch_image(request: ImageDownloadRequest<'_>, progress: &ProgressPublisher, cancel: &AtomicBool) -> Result<PathBuf, DownloadError>`
  - тестовый хелпер `support::TestServer` с `start(body: Vec<u8>, behaviour: Behaviour) -> Self`, `url(&self) -> &str`, `ranges_seen(&self) -> Vec<Option<String>>`, и `support::Behaviour { Ranged, IgnoresRange, RejectsRange, Truncated { bytes: usize } }`

- [ ] **Step 1: Написать петлевой сервер**

Создать `crates/image/tests/support/mod.rs`:

```rust
//! A hand-written HTTP server on the loopback interface.
//!
//! The download is tested against a real socket rather than a stubbed-out
//! client trait: a trait with one implementation is what AGENTS.md tells us not
//! to write, and it would only ever prove that our own stub behaves the way we
//! imagined. A socket exercises our code and `ureq` together.

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

/// How the server answers a request.
#[derive(Clone, Copy, Debug)]
pub enum Behaviour {
    /// Honours `range`, answering 206 when one is asked for.
    Ranged,
    /// Answers 200 with the whole body whatever `range` says.
    IgnoresRange,
    /// Answers 416 to any `range`, and 200 to a request without one.
    RejectsRange,
    /// Promises the whole length, sends `bytes`, then hangs up.
    Truncated { bytes: usize },
}

pub struct TestServer {
    url: String,
    ranges: Arc<Mutex<Vec<Option<String>>>>,
}

impl TestServer {
    pub fn start(body: Vec<u8>, behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the loopback port should bind");
        let url = format!(
            "http://{}/noble-cloudimg-amd64.img",
            listener.local_addr().unwrap()
        );
        let ranges = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&ranges);
        // The thread is left to park in `accept` when the test ends: the test
        // binary exits and takes it with it, which is cheaper than wiring a
        // shutdown protocol into a fixture.
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let range = answer(stream, &body, behaviour);
                recorded.lock().unwrap().push(range);
            }
        });

        Self { url, ranges }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The `range` header of every request served so far, in order.
    pub fn ranges_seen(&self) -> Vec<Option<String>> {
        self.ranges.lock().unwrap().clone()
    }
}

fn answer(mut stream: TcpStream, body: &[u8], behaviour: Behaviour) -> Option<String> {
    let range = read_range_header(&stream);
    let requested_from = range
        .as_deref()
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split('-').next())
        .and_then(|value| value.parse::<usize>().ok());

    match (behaviour, requested_from) {
        (Behaviour::Truncated { bytes }, _) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body[..bytes.min(body.len())]);
            let _ = stream.flush();
            // Dropping the stream here is the point: the client sees EOF with
            // bytes still outstanding.
        }
        (Behaviour::RejectsRange, Some(_)) => {
            let head = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
        }
        (Behaviour::Ranged, Some(from)) if from < body.len() => {
            let slice = &body[from..];
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\n\r\n",
                slice.len(),
                from,
                body.len() - 1,
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(slice);
            let _ = stream.flush();
        }
        _ => {
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    }

    range
}

/// Reads the request and returns its `range` header, if any.
///
/// Header names are matched case-insensitively because `ureq` sends them
/// lowercase (`range: bytes=1000-`). A server looking for `Range: ` would
/// silently answer 200 to every resume, and the resume test would pass while
/// testing nothing.
fn read_range_header(stream: &TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().expect("the stream should clone"));
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("range")
        {
            range = Some(value.trim().to_owned());
        }
    }
    range
}
```

- [ ] **Step 2: Написать падающие тесты свежей загрузки и кеша**

Создать `crates/image/tests/download.rs`:

```rust
mod support;

use std::{
    fs,
    path::PathBuf,
    sync::atomic::AtomicBool,
};

use support::{Behaviour, TestServer};
use vmlord_core::{DownloadPhase, ProgressPublisher};
use vmlord_image::{DownloadError, ImageDownloadRequest, fetch_image};

/// The bytes every test downloads, and the sum they hash to.
fn image_body() -> Vec<u8> {
    (0u8..=255).cycle().take(64 * 1024 + 17).collect()
}

fn image_sum() -> String {
    // Computed once by the test itself so the fixture and the expectation
    // cannot drift apart.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(image_body());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cache_directory(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("vmlord-download-{tag}-{unique}"));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn an_image_is_downloaded_verified_and_named_after_its_checksum() {
    let directory = cache_directory("fresh");
    let server = TestServer::start(image_body(), Behaviour::Ranged);
    let sum = image_sum();
    let publisher = ProgressPublisher::default();

    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &publisher,
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(path, directory.join(format!("{sum}.img")));
    assert_eq!(fs::read(&path).unwrap(), image_body());
    assert!(
        !directory.join(format!("{sum}.img.part")).exists()
            || fs::metadata(directory.join(format!("{sum}.img.part")))
                .unwrap()
                .len()
                == 0,
        "the partial file must not be left holding a copy of the image"
    );
    assert_eq!(publisher.snapshot(), Some(DownloadPhase::Completed));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_cached_image_is_used_without_touching_the_network() {
    let directory = cache_directory("hit");
    let server = TestServer::start(image_body(), Behaviour::Ranged);
    let sum = image_sum();
    fs::write(directory.join(format!("{sum}.img")), image_body()).unwrap();
    let publisher = ProgressPublisher::default();

    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &publisher,
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(path, directory.join(format!("{sum}.img")));
    assert!(
        server.ranges_seen().is_empty(),
        "a cache hit must not make a request at all"
    );
    assert_eq!(publisher.snapshot(), Some(DownloadPhase::Completed));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_cached_image_left_truncated_by_an_earlier_run_is_replaced() {
    let directory = cache_directory("truncated-cache");
    let server = TestServer::start(image_body(), Behaviour::Ranged);
    let sum = image_sum();
    let cached = directory.join(format!("{sum}.img"));
    fs::write(&cached, &image_body()[..1000]).unwrap();

    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(fs::read(&path).unwrap(), image_body());
    assert_eq!(
        server.ranges_seen().len(),
        1,
        "checking the sum on every cache hit is the only thing that catches this"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_image_that_does_not_hash_to_what_was_promised_is_refused() {
    let directory = cache_directory("mismatch");
    let server = TestServer::start(image_body(), Behaviour::Ranged);
    let wrong_sum = "0".repeat(64);

    let error = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &wrong_sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("an image that hashes differently must never enter the cache");

    assert!(
        matches!(error, DownloadError::ChecksumMismatch { .. }),
        "got {error:?}"
    );
    assert!(!directory.join(format!("{wrong_sum}.img")).exists());
    assert_eq!(
        fs::metadata(directory.join(format!("{wrong_sum}.img.part")))
            .unwrap()
            .len(),
        0,
        "the bad bytes are dropped, but the .part keeps its lock and its name"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_checksum_that_is_not_one_is_refused_before_any_request() {
    let directory = cache_directory("bad-sum");
    let server = TestServer::start(image_body(), Behaviour::Ranged);

    let error = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: "not-a-checksum",
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("the caller should learn this in milliseconds, not after 600 MB");

    assert!(matches!(error, DownloadError::InvalidChecksum(_)));
    assert!(server.ranges_seen().is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_image_another_downloader_finished_first_is_adopted() {
    let directory = cache_directory("race-rename");
    let sum = image_sum();
    // The winner's file is already in place; our download has to notice rather
    // than replace a file the importer may have open.
    fs::write(directory.join(format!("{sum}.img")), image_body()).unwrap();
    let server = TestServer::start(image_body(), Behaviour::Ranged);

    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(path, directory.join(format!("{sum}.img")));
    assert_eq!(fs::read(&path).unwrap(), image_body());

    fs::remove_dir_all(directory).unwrap();
}
```

Крейт `sha2` нужен тесту, поэтому в `crates/image/Cargo.toml` добавить:

```toml
[dev-dependencies]
sha2 = "0.11"
```

- [ ] **Step 3: Прогнать и убедиться, что падает**

Run: `cargo test -p vmlord-image --test download`
Expected: FAIL, `cannot find function fetch_image in crate vmlord_image`.

- [ ] **Step 4: Реализовать `download.rs`**

```rust
//! Fetching a distribution image into the cache, intact.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use ureq::{Agent, Body, http::Response};
use vmlord_core::{DownloadPhase, ProgressPublisher, ProgressThrottle};

use crate::{
    cache::{cache_file_name, file_checksum, normalized_checksum},
    error::{DownloadError, io_error},
    http::{ResumeOutcome, build_agent, parse_content_range, resume_decision},
    part::PartFile,
};

/// How much of the body is read between two cancellation checks.
const READ_CHUNK: usize = 64 * 1024;

/// An image to fetch, and where to keep it.
pub struct ImageDownloadRequest<'a> {
    pub url: &'a str,
    /// The SHA256 the image must hash to, in hex.
    pub expected_sha256: &'a str,
    pub cache_directory: &'a Path,
}

/// Returns the path of the verified image in the cache, downloading it first if
/// the cache has not got it.
///
/// The checksum is verified on every call, cache hit included. That is the only
/// thing that distinguishes a complete image from one an interrupted download
/// left truncated, and it costs seconds against re-fetching hundreds of
/// megabytes.
pub fn fetch_image(
    request: ImageDownloadRequest<'_>,
    progress: &ProgressPublisher,
    cancel: &AtomicBool,
) -> Result<PathBuf, DownloadError> {
    let expected = normalized_checksum(request.expected_sha256).inspect_err(|error| {
        log::error!("{error}");
    })?;
    let mut throttle = ProgressThrottle::new(progress.clone());

    fs::create_dir_all(request.cache_directory)
        .map_err(io_error("create the image cache", request.cache_directory))?;

    let file_name = cache_file_name(request.url, &expected);
    let final_path = request.cache_directory.join(&file_name);
    let part_path = request.cache_directory.join(format!("{file_name}.part"));

    if final_path.is_file() && cache_hit(&final_path, &expected, &mut throttle, cancel)? {
        log::info!("using the cached image at {}", final_path.display());
        throttle.publish_now(DownloadPhase::Completed);
        return Ok(final_path);
    }

    let mut part = PartFile::open_locked(part_path).inspect_err(|error| log::error!("{error}"))?;

    log::info!("downloading {} into {}", request.url, final_path.display());
    download_into(&mut part, request.url, &mut throttle, cancel)
        .inspect_err(|error| log::error!("failed to download {}: {error}", request.url))?;

    let actual = file_checksum(part.path(), &mut throttle, cancel)?;
    if actual != expected {
        // Truncate rather than delete: the file is open and holds the lock, and
        // its name is worth keeping for the next attempt.
        part.truncate()?;
        let error = DownloadError::ChecksumMismatch { expected, actual };
        log::error!("{error}");
        return Err(error);
    }

    publish_into_cache(&mut part, &final_path)?;
    log::info!("image ready at {}", final_path.display());
    throttle.publish_now(DownloadPhase::Completed);
    Ok(final_path)
}

/// Whether the cached file is the image it claims to be.
///
/// A file that is not is deleted here, so the caller can go straight on to
/// downloading it again.
fn cache_hit(
    final_path: &Path,
    expected: &str,
    throttle: &mut ProgressThrottle,
    cancel: &AtomicBool,
) -> Result<bool, DownloadError> {
    let actual = file_checksum(final_path, throttle, cancel)?;
    if actual == expected {
        return Ok(true);
    }

    log::warn!(
        "the cached image at {} hashes to {actual} instead of {expected}; \
         discarding it and downloading again",
        final_path.display()
    );
    match fs::remove_file(final_path) {
        Ok(()) => Ok(false),
        // Another downloader reached the same conclusion first.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(DownloadError::Io {
            operation: "discard the corrupt cached image",
            path: final_path.to_path_buf(),
            source,
        }),
    }
}

/// Moves the verified partial file into its final name.
///
/// If the final file already exists, another downloader won the race. Both
/// files have the same content by construction -- the name is the checksum --
/// so the winner's copy is adopted rather than overwritten. `fs::rename` on
/// Windows replaces the target silently, and replacing a file the importer may
/// have open is exactly what this avoids.
///
/// Renaming a file this process still holds open is fine: Rust opens files with
/// `FILE_SHARE_DELETE` among its share flags, so `MoveFileEx` succeeds. The
/// handle then refers to the renamed file and keeps its lock until `fetch_image`
/// returns and drops it, which is before any caller sees the path.
fn publish_into_cache(part: &mut PartFile, final_path: &Path) -> Result<(), DownloadError> {
    if final_path.exists() {
        log::info!(
            "another download finished {} first; keeping its copy",
            final_path.display()
        );
        return part.truncate();
    }

    fs::rename(part.path(), final_path).map_err(io_error("publish the image", final_path))
}

/// Streams the body into the partial file, resuming where it can.
fn download_into(
    part: &mut PartFile,
    url: &str,
    throttle: &mut ProgressThrottle,
    cancel: &AtomicBool,
) -> Result<(), DownloadError> {
    let agent = build_agent();
    throttle.publish_now(DownloadPhase::Connecting);

    let mut from = part.len()?;
    let mut response = send(&agent, url, from)?;

    if from > 0 {
        let content_range = header(&response, "content-range");
        match resume_decision(
            response.status().as_u16(),
            content_range.as_deref(),
            from,
        )? {
            ResumeOutcome::Append { .. } => {
                log::debug!("resuming {url} at byte {from}");
            }
            ResumeOutcome::StartOver => {
                log::warn!("the server ignored the range request for {url}; downloading from the start");
                part.truncate()?;
                from = 0;
            }
            ResumeOutcome::RangeUnsatisfiable => {
                log::warn!(
                    "the server rejected the range request for {url}; the partial file is stale"
                );
                part.truncate()?;
                from = 0;
                response = send(&agent, url, 0)?;
                let status = response.status().as_u16();
                if status != 200 {
                    return Err(DownloadError::UnexpectedStatus { status });
                }
            }
        }
    } else {
        let status = response.status().as_u16();
        if status != 200 {
            return Err(DownloadError::UnexpectedStatus { status });
        }
    }

    let total = total_length(&response, from);
    part.seek_to_end()?;

    let mut downloaded = from;
    throttle.publish_now(DownloadPhase::Downloading { downloaded, total });
    let mut reader = response.body_mut().as_reader();
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        if cancel.load(Ordering::Relaxed) {
            log::debug!("the download of {url} was cancelled at byte {downloaded}");
            return Err(DownloadError::Cancelled);
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|source| DownloadError::Http(format!("reading {url} failed: {source}")))?;
        if read == 0 {
            break;
        }
        part.write_all(&buffer[..read])?;
        downloaded += read as u64;
        throttle.publish(DownloadPhase::Downloading { downloaded, total });
    }
    throttle.publish_now(DownloadPhase::Downloading { downloaded, total });

    part.sync()?;
    log::debug!("{url} delivered {downloaded} bytes");
    Ok(())
}

/// Issues the request, asking to resume when `from` is past the start.
fn send(agent: &Agent, url: &str, from: u64) -> Result<Response<Body>, DownloadError> {
    let request = agent.get(url);
    let request = if from > 0 {
        request.header("Range", format!("bytes={from}-"))
    } else {
        request
    };
    request
        .call()
        .map_err(|source| DownloadError::Http(format!("requesting {url} failed: {source}")))
}

fn header(response: &Response<Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The full size of the image, when the server said enough to know it.
fn total_length(response: &Response<Body>, from: u64) -> Option<u64> {
    if let Some(range) = header(response, "content-range")
        && let Some((_, _, complete)) = parse_content_range(&range)
    {
        return complete;
    }
    header(response, "content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .map(|length| length + from)
}
```

Дописать `crates/image/src/lib.rs`:

```rust
mod cache;
mod download;
mod error;
mod http;
mod part;

pub use download::{ImageDownloadRequest, fetch_image};
pub use error::DownloadError;
```

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test -p vmlord-image`
Expected: PASS, включая шесть тестов в `tests/download.rs`.

Run: `cargo clippy -p vmlord-image --all-targets -- -D warnings`
Expected: без предупреждений.

- [ ] **Step 6: Коммит**

```bash
git add crates/image
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-50: Download an image into the cache and verify it there"
```

---

### Task 7: Докачка и упрямые серверы

**Files:**
- Create: `crates/image/tests/resume.rs`

**Interfaces:**
- Consumes: `fetch_image`, `support::{Behaviour, TestServer}` из Task 6. Новый код в `src/` эта задача добавлять не должна: ветки уже написаны в Task 6, здесь они доказываются на живом сокете.

- [ ] **Step 1: Написать тесты**

Создать `crates/image/tests/resume.rs`:

```rust
mod support;

use std::{fs, path::PathBuf, sync::atomic::AtomicBool};

use support::{Behaviour, TestServer};
use vmlord_core::ProgressPublisher;
use vmlord_image::{DownloadError, ImageDownloadRequest, fetch_image};

fn image_body() -> Vec<u8> {
    (0u8..=255).cycle().take(64 * 1024 + 17).collect()
}

fn image_sum() -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(image_body());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cache_directory(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("vmlord-resume-{tag}-{unique}"));
    fs::create_dir_all(&path).unwrap();
    path
}

/// Leaves `prefix` bytes of the image in the partial file, as an interrupted
/// download would have.
fn seed_partial(directory: &PathBuf, sum: &str, prefix: usize) {
    fs::write(
        directory.join(format!("{sum}.img.part")),
        &image_body()[..prefix],
    )
    .unwrap();
}

#[test]
fn an_interrupted_download_asks_only_for_the_rest() {
    let directory = cache_directory("resume");
    let sum = image_sum();
    seed_partial(&directory, &sum, 1000);
    let server = TestServer::start(image_body(), Behaviour::Ranged);

    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(fs::read(&path).unwrap(), image_body());
    assert_eq!(
        server.ranges_seen(),
        vec![Some("bytes=1000-".to_owned())],
        "the whole point is not to fetch the first 1000 bytes twice"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_server_that_ignores_the_range_still_yields_a_correct_image() {
    let directory = cache_directory("ignored");
    let sum = image_sum();
    seed_partial(&directory, &sum, 1000);
    let server = TestServer::start(image_body(), Behaviour::IgnoresRange);

    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(
        fs::read(&path).unwrap(),
        image_body(),
        "appending a whole body to a partial one would double the first 1000 bytes"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_range_the_server_rejects_leads_to_one_clean_restart() {
    let directory = cache_directory("rejected");
    let sum = image_sum();
    seed_partial(&directory, &sum, 1000);
    let server = TestServer::start(image_body(), Behaviour::RejectsRange);

    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(fs::read(&path).unwrap(), image_body());
    assert_eq!(
        server.ranges_seen(),
        vec![Some("bytes=1000-".to_owned()), None],
        "exactly one retry, and it asks for the whole file"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_body_cut_short_fails_and_keeps_what_arrived() {
    let directory = cache_directory("cut");
    let sum = image_sum();
    let server = TestServer::start(image_body(), Behaviour::Truncated { bytes: 4096 });

    let error = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("a body that stops early is not an image");

    assert!(matches!(error, DownloadError::Http(_)), "got {error:?}");
    assert_eq!(
        fs::metadata(directory.join(format!("{sum}.img.part")))
            .unwrap()
            .len(),
        4096,
        "the bytes that did arrive are kept so the next attempt can resume them"
    );
    assert!(!directory.join(format!("{sum}.img")).exists());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn the_next_attempt_resumes_what_the_cut_body_left() {
    let directory = cache_directory("cut-then-resume");
    let sum = image_sum();
    let cut = TestServer::start(image_body(), Behaviour::Truncated { bytes: 4096 });
    let _ = fetch_image(
        ImageDownloadRequest {
            url: cut.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    );

    let server = TestServer::start(image_body(), Behaviour::Ranged);
    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(fs::read(&path).unwrap(), image_body());
    assert_eq!(server.ranges_seen(), vec![Some("bytes=4096-".to_owned())]);

    fs::remove_dir_all(directory).unwrap();
}
```

- [ ] **Step 2: Прогнать**

Run: `cargo test -p vmlord-image --test resume`
Expected: PASS, пять тестов. Если тест `an_interrupted_download_asks_only_for_the_rest` видит `ranges_seen() == [None]`, это значит, что сервер не распознал заголовок — проверь, что имя сравнивается через `eq_ignore_ascii_case`, потому что `ureq` шлёт `range:` в нижнем регистре.

- [ ] **Step 3: Коммит**

```bash
git add crates/image/tests/resume.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-50: Resume an interrupted download, whatever the server allows"
```

---

### Task 8: Отмена и отказ второму загрузчику

**Files:**
- Modify: `crates/image/tests/download.rs`

**Interfaces:**
- Consumes: всё из Task 6. Нового кода в `src/` не требуется — проверяются пути отмены и лока, уже написанные в Task 4 и Task 6.

- [ ] **Step 1: Написать тесты**

Дописать в `crates/image/tests/download.rs`:

```rust
#[test]
fn a_cancelled_download_stops_and_keeps_what_it_had() {
    let directory = cache_directory("cancel");
    let sum = image_sum();
    let server = TestServer::start(image_body(), Behaviour::Ranged);
    let cancel = AtomicBool::new(true);

    let error = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &cancel,
    )
    .expect_err("a cancelled download must not report success");

    assert!(matches!(error, DownloadError::Cancelled), "got {error:?}");
    assert!(
        directory.join(format!("{sum}.img.part")).exists(),
        "the partial file survives cancellation so the next run can resume it"
    );
    assert!(!directory.join(format!("{sum}.img")).exists());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_second_download_of_the_same_image_is_refused_while_the_first_runs() {
    use std::sync::{Arc, Barrier};

    let directory = cache_directory("race");
    let sum = image_sum();
    let server = TestServer::start(image_body(), Behaviour::Ranged);

    // Hold the lock the way a running download holds it, by starting one and
    // parking it: the partial file is opened and locked before any byte moves.
    let held = directory.join(format!("{sum}.img.part"));
    let lock_taken = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let holder = {
        let (lock_taken, release, held) = (
            Arc::clone(&lock_taken),
            Arc::clone(&release),
            held.clone(),
        );
        std::thread::spawn(move || {
            let file = fs::File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&held)
                .unwrap();
            file.try_lock().unwrap();
            lock_taken.wait();
            release.wait();
            drop(file);
        })
    };
    lock_taken.wait();

    let error = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("two downloaders must not write into one partial file");

    assert!(
        matches!(error, DownloadError::AlreadyInProgress { .. }),
        "got {error:?}"
    );
    assert!(
        server.ranges_seen().is_empty(),
        "the refusal must come before any bandwidth is spent"
    );

    release.wait();
    holder.join().unwrap();

    // With the lock gone, the same call goes through.
    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(fs::read(&path).unwrap(), image_body());

    fs::remove_dir_all(directory).unwrap();
}
```

- [ ] **Step 2: Прогнать**

Run: `cargo test -p vmlord-image --test download`
Expected: PASS, восемь тестов.

Если `a_cancelled_download_stops_and_keeps_what_it_had` падает с `Http(...)` вместо `Cancelled`, значит флаг проверяется только внутри цикла чтения, а тело успело прийти целиком одним чтением. Проверка отмены должна стоять первой в теле цикла — как и написано в Task 6 — и, кроме того, `file_checksum` проверяет её отдельно.

- [ ] **Step 3: Коммит**

```bash
git add crates/image/tests/download.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-50: Cover cancelling a download and refusing a second one"
```

---

### Task 9: Документация и итоговая проверка

**Files:**
- Modify: `ARCHITECTURE.md`

- [ ] **Step 1: Описать загрузчик в ARCHITECTURE.md**

Найти раздел, перечисляющий крейты workspace, и добавить `vmlord-image` рядом с остальными, затем добавить раздел:

```markdown
### Image download

`vmlord-image` fetches a distribution's cloud image over HTTPS into a cache
directory configured as `image_cache_path`.

The cache is addressed by content: an entry is named after the SHA256 it is
expected to have, so two releases cannot collide on a name and a file whose name
disagrees with its content cannot exist. The checksum is verified on every call,
cache hit included — it is the only thing that tells a complete image from one
an interrupted download left truncated.

Trust comes from HTTPS. A checksum list downloaded from the same server as the
image proves nothing about authenticity, because whoever could swap the image
could swap the list. Certificates are checked against the platform verifier, so
a host behind a TLS-inspecting proxy works.

The client is blocking (`ureq`, no async runtime, matching the DHCP worker in
`crates/platform/src/dhcp.rs`). Downloads resume through HTTP `Range` when the
server allows it, and fall back to a fresh transfer when it does not.

Two downloaders of one image are separated by an exclusive OS lock on the
partial file (`std::fs::File::try_lock`), which covers two threads of one
process as well as two processes and is released even if a process dies. The
second one is refused with `AlreadyInProgress` rather than made to wait:
queueing is the caller's policy.

Progress is published as a `vmlord_core::DownloadPhase` snapshot that a UI
thread can poll. It is a level rather than a queue of events, so only the latest
value is kept. The widget that draws it is not part of this crate.
```

- [ ] **Step 2: Прогнать всё**

Run: `cargo test -p vmlord-core -p vmlord-image`
Expected: PASS, все тесты.

Run: `cargo clippy -p vmlord-core -p vmlord-image --all-targets -- -D warnings`
Expected: без предупреждений.

Run: `cargo build --target=x86_64-pc-windows-gnu`
Expected: успех для всего workspace, включая `crates/ui`.

Run: `cargo fmt --check`
Expected: без расхождений. Если есть — `cargo fmt` и перепроверить.

- [ ] **Step 3: Коммит**

```bash
git add ARCHITECTURE.md
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local \
GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
git commit -m "TASK-50: Document the image downloader"
```

- [ ] **Step 4: Отметить пункты в Vikunja**

Отметить выполненными все чекбоксы в описании задачи #50 и оставить комментарий с тем, что проверено, а что осталось за владельцем проекта (загрузка реального образа Ubuntu с реального зеркала на Windows-хосте — единственное, что нельзя проверить в WSL).

---

## Что остаётся владельцу проекта

Всё в этом плане проверяется автоматически в WSL, кроме одного: реальный HTTPS к
`cloud-images.ubuntu.com` с Windows-хоста, где работает `platform-verifier` и
системный прокси. Петлевой сервер ходит по plain HTTP, поэтому путь проверки
сертификатов тестами не покрыт по построению. Это ручная проверка на хосте.
