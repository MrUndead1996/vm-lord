# TASK-64: фоновое создание VM, состояние Building и прогресс — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** создание VM уходит в рабочий поток, строящаяся VM видна в списке со
стадией и байтами скачивания, а отмена, закрытие приложения и паника потока не
оставляют ни каталога VM, ни осиротевшей HCS-системы.

**Architecture:** поток принадлежит `platform` и живёт внутри
`HcsVmRepository`. `create_vm` заводит запись в реестре сборок и возвращает
`Ok` немедленно; `list_vms` склеивает записи `MetadataStore` со строящимися.
`VmRepository` остаётся синхронным, `WorkspaceApp` не меняется, UI узнаёт о
`Building` через существующий секундный `refresh`. Прогресс — уровень в двух
слотах (`BuildStep` снаружи, `DownloadPhase` изнутри `vmlord-image`),
склеиваемых в момент чтения.

**Tech Stack:** Rust 2024, `std::thread` без async-рантайма, `std::sync`
(`Arc`, `Mutex`, `AtomicBool`), `log`.

## Global Constraints

* Спека: `docs/superpowers/specs/2026-08-11-background-vm-creation-design.md`.
* Ветка `task-64-background-vm-creation`, уже создана от `main`. Коммиты —
  `TASK-64: <comment>`, subject по-английски, в повелительном наклонении.
* Сборка и тесты — под Windows-таргет:
  `cargo test --target=x86_64-pc-windows-gnu`. Крейты `vmlord-core` и
  `vmlord-image` собираются и тестируются и нативно
  (`cargo test -p vmlord-core`). Без префикса `timeout`.
* Никакого async-рантайма: ни `tokio`, ни `async`, ни `.await`. Единственный
  образец фонового потока — `crates/platform/src/dhcp.rs`.
* Никаких новых зависимостей: всё делается на `std`.
* Отравленный `Mutex` восстанавливается, а не пробрасывает панику:
  `.unwrap_or_else(|poisoned| poisoned.into_inner())` — идиома уже принята в
  `progress.rs:56` и `repository.rs:137`.
* Уровни логов — DEBUG..ERROR, TRACE не используется. В лог не попадают ни
  открытый пароль, ни его `$6$`-хеш, ни приватный ключ.
* Комментарии и docstring'и — по-английски, как во всём репозитории.
* `catch_unwind` не применять: замыкания швов не `UnwindSafe`. Защита от паники
  — только страж на `Drop`.
* MR открывается только после явного одобрения владельца проекта.

---

### Task 1: `ProgressPublisher` и `ProgressThrottle` становятся дженериками

**Files:**
- Modify: `crates/core/src/progress.rs:32-117` (типы и их impl'ы)
- Modify: `crates/image/src/open.rs:11,38`
- Modify: `crates/image/src/download.rs:11,40,89,142`
- Modify: `crates/image/src/cache.rs:11,75,99`
- Modify: `crates/image/src/part.rs:10,116`
- Test: `crates/core/src/progress.rs` (существующий `mod tests`)

**Interfaces:**
- Consumes: ничего нового.
- Produces: `ProgressPublisher<P>` и `ProgressThrottle<P>` с прежними методами
  (`publish`, `snapshot`, `new`, `with_interval`, `publish_now`,
  `DEFAULT_INTERVAL`). Прежний неявный `DownloadPhase` становится явным
  параметром: `ProgressPublisher<DownloadPhase>`.

- [ ] **Step 1: Написать падающий тест на публикацию не-`DownloadPhase`**

В конец `mod tests` в `crates/core/src/progress.rs`:

```rust
/// The slot carries whatever a long operation reports, not downloads alone:
/// #64 publishes the step a VM's creation is at through the same type.
#[test]
fn a_publisher_carries_any_copyable_value() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Step {
        First,
        Second,
    }

    let publisher = ProgressPublisher::<Step>::default();
    assert_eq!(publisher.snapshot(), None);

    publisher.publish(Step::First);
    publisher.publish(Step::Second);

    assert_eq!(publisher.snapshot(), Some(Step::Second));
}
```

- [ ] **Step 2: Убедиться, что тест не компилируется**

Run: `cargo test -p vmlord-core --lib progress`
Expected: FAIL — `struct takes 0 generic arguments but 1 generic argument was supplied`.

- [ ] **Step 3: Сделать оба типа дженериками**

В `crates/core/src/progress.rs` заменить блок с `ProgressPublisher` (строки
32–61) на:

```rust
/// The slot a worker writes progress into and the UI thread reads.
///
/// Cloning shares the slot: the worker holds one clone while the UI reads
/// through another.
///
/// Generic over what is being reported because a slot does not care: a
/// download reports its bytes, and creating a VM reports the step it is at,
/// through the same overwritten cell.
pub struct ProgressPublisher<P>(Arc<Mutex<Option<P>>>);

// Written out rather than derived: `#[derive(Clone)]` would demand `P: Clone`,
// and cloning a publisher clones the handle to the slot, never its contents.
impl<P> Clone for ProgressPublisher<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

// Likewise: an empty slot needs no `P: Default`, because it holds nothing yet.
impl<P> Default for ProgressPublisher<P> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }
}

impl<P: Copy> ProgressPublisher<P> {
    /// Replaces whatever the last reported value was.
    pub fn publish(&self, value: P) {
        *self.lock() = Some(value);
    }

    /// Reports the last published value, or `None` before the first one.
    #[must_use]
    pub fn snapshot(&self) -> Option<P> {
        *self.lock()
    }

    /// Recovers a poisoned lock rather than propagating the panic.
    ///
    /// The slot holds a plain `Copy` value that a panic elsewhere cannot leave
    /// half-written, and losing all progress reporting because an unrelated
    /// thread panicked would be worse than reading it.
    fn lock(&self) -> MutexGuard<'_, Option<P>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
```

Дальше — `ProgressThrottle`: заменить объявление и `impl` (строки 68–117) на
те же тела с параметром типа:

```rust
pub struct ProgressThrottle<P> {
    publisher: ProgressPublisher<P>,
    min_interval: Duration,
    last: Option<(Instant, Discriminant<P>)>,
}

impl<P: Copy> ProgressThrottle<P> {
    /// How long two reports of the same phase must be apart.
    pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);

    #[must_use]
    pub fn new(publisher: ProgressPublisher<P>) -> Self {
        Self::with_interval(publisher, Self::DEFAULT_INTERVAL)
    }

    #[must_use]
    pub fn with_interval(publisher: ProgressPublisher<P>, min_interval: Duration) -> Self {
        Self {
            publisher,
            min_interval,
            last: None,
        }
    }

    /// Publishes `value`, unless the same kind of value was published less
    /// than `min_interval` ago.
    ///
    /// A change of phase is never delayed: it is the transition the UI needs to
    /// see, and holding it back is what leaves a progress bar stuck just short
    /// of the end.
    pub fn publish(&mut self, value: P) {
        let kind = std::mem::discriminant(&value);
        if let Some((published_at, last_kind)) = self.last
            && last_kind == kind
            && published_at.elapsed() < self.min_interval
        {
            return;
        }
        self.publish_now(value);
    }

    /// Publishes `value` whatever the interval says.
    ///
    /// Used for the last value of a phase, which must land even if the
    /// preceding one was moments ago.
    pub fn publish_now(&mut self, value: P) {
        self.publisher.publish(value);
        self.last = Some((Instant::now(), std::mem::discriminant(&value)));
    }
}
```

- [ ] **Step 4: Проставить `<DownloadPhase>` в `crates/image`**

Пять правок, все механические:

* `crates/image/src/open.rs:11` — импорт уже содержит нужное; в сигнатуре
  (строка 38) `progress: &ProgressPublisher` → `progress: &ProgressPublisher<DownloadPhase>`,
  а в `use vmlord_core::{DistroProfile, ProgressPublisher, RepositoryError};`
  добавить `DownloadPhase`.
* `crates/image/src/download.rs:40` — `progress: &ProgressPublisher<DownloadPhase>`;
  строки 89 и 142 — `throttle: &mut ProgressThrottle<DownloadPhase>`.
* `crates/image/src/cache.rs:75,99` — `progress: &mut ProgressThrottle<DownloadPhase>`
  (`DownloadPhase` там уже импортирован).
* `crates/image/src/part.rs:116` — `progress: &mut ProgressThrottle<DownloadPhase>`,
  и в `use vmlord_core::ProgressThrottle;` (строка 10) добавить `DownloadPhase`:
  `use vmlord_core::{DownloadPhase, ProgressThrottle};`.

Вызовы `ProgressPublisher::default()` в тестах не трогать: параметр выводится
из сигнатуры вызываемой функции.

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test -p vmlord-core --lib` и `cargo test -p vmlord-image`
Expected: PASS, включая новый `a_publisher_carries_any_copyable_value`.

- [ ] **Step 6: Коммит**

```bash
git add crates/core/src/progress.rs crates/image/src
git commit -m "TASK-64: Make the progress slot generic over what it reports"
```

---

### Task 2: `BuildStep`, `BuildProgress` и `BuildMonitor`

**Files:**
- Modify: `crates/core/src/progress.rs` (новые типы в конец, перед `mod tests`)
- Modify: `crates/core/src/lib.rs:11` (реэкспорт)
- Test: `crates/core/src/progress.rs` (`mod tests`)

**Interfaces:**
- Consumes: `ProgressPublisher<P>` из Task 1, `DownloadPhase`, `RepositoryError`.
- Produces:
  - `BuildStep { Downloading, WritingDisk, Provisioning, Registering }`, `Copy`;
  - `BuildProgress { step: BuildStep, download: Option<DownloadPhase> }`, `Copy`;
  - `BuildMonitor` с `new(initial: BuildStep)`, `report(&self, step: BuildStep)`,
    `downloads(&self) -> &ProgressPublisher<DownloadPhase>`,
    `cancel_flag(&self) -> &AtomicBool`, `cancel(&self)`,
    `is_cancelled(&self) -> bool`,
    `check_cancelled(&self) -> Result<(), RepositoryError>`,
    `snapshot(&self) -> BuildProgress`. `Clone`, `Send`, `Sync`.
  - Реэкспорты из `vmlord_core`: `BuildMonitor`, `BuildProgress`, `BuildStep`.

- [ ] **Step 1: Написать падающие тесты**

В `mod tests` в `crates/core/src/progress.rs` добавить импорт
`use super::{BuildMonitor, BuildStep};` к существующему `use super::{...}` и
дописать:

```rust
#[test]
fn a_monitor_reports_the_step_it_was_started_at() {
    let monitor = BuildMonitor::new(BuildStep::WritingDisk);

    let progress = monitor.snapshot();

    assert_eq!(progress.step, BuildStep::WritingDisk);
    assert_eq!(
        progress.download, None,
        "a build that never downloads anything has no bytes to show"
    );
}

#[test]
fn a_monitor_shows_downloaded_bytes_only_while_downloading() {
    let monitor = BuildMonitor::new(BuildStep::Downloading);
    monitor.downloads().publish(DownloadPhase::Downloading {
        downloaded: 10,
        total: Some(100),
    });

    assert_eq!(
        monitor.snapshot().download,
        Some(DownloadPhase::Downloading {
            downloaded: 10,
            total: Some(100),
        })
    );

    monitor.report(BuildStep::WritingDisk);

    assert_eq!(
        monitor.snapshot(),
        super::BuildProgress {
            step: BuildStep::WritingDisk,
            download: None,
        },
        "the download's last phase must not be shown beside a later step"
    );
}

#[test]
fn a_clone_of_a_monitor_shares_the_step_and_the_cancellation() {
    let monitor = BuildMonitor::new(BuildStep::Downloading);
    let worker_side = monitor.clone();

    worker_side.report(BuildStep::Registering);
    monitor.cancel();

    assert_eq!(monitor.snapshot().step, BuildStep::Registering);
    assert!(worker_side.is_cancelled());
}

#[test]
fn check_cancelled_names_the_cancellation_as_the_cause() {
    let monitor = BuildMonitor::new(BuildStep::Downloading);
    assert!(monitor.check_cancelled().is_ok());

    monitor.cancel();

    let error = monitor
        .check_cancelled()
        .expect_err("a cancelled build must not be allowed to continue");
    assert!(error.to_string().contains("cancelled"), "got {error}");
}
```

- [ ] **Step 2: Прогнать и убедиться, что не компилируется**

Run: `cargo test -p vmlord-core --lib progress`
Expected: FAIL — `cannot find type BuildMonitor in this scope`.

- [ ] **Step 3: Реализовать типы**

В `crates/core/src/progress.rs`: расширить импорты
`use std::sync::atomic::{AtomicBool, Ordering};` и добавить перед `mod tests`:

```rust
/// Which step of creating a VM is running.
///
/// Four steps and no overall percentage: fetching an image, writing a disk and
/// handing the result to HCS are not commensurable, so a bar over all of them
/// would need a denominator that does not exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildStep {
    /// Fetching the cloud image into the cache. Cloud images only.
    Downloading,
    /// Writing the system disk: an empty VHDX, or the image onto one.
    WritingDisk,
    /// Writing what the VM needs into its directory -- the key pair, the seed
    /// volume, the HCS configuration -- and granting the VM access to it.
    Provisioning,
    /// Creating the compute system and recording the VM in the metadata.
    Registering,
}

/// What creating a VM looks like from outside the thread doing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildProgress {
    pub step: BuildStep,
    /// The download's own progress. Only meaningful while `step` is
    /// `Downloading`, and `None` at every other step -- a stale byte count
    /// shown beside a later step would read as a download still running.
    pub download: Option<DownloadPhase>,
}

/// The channel between a VM being built and the thread watching it: what the
/// build is doing, and whether it has been told to stop.
///
/// Two slots rather than one because the byte counts are published deep inside
/// `vmlord-image`, which knows nothing of VMs, while the steps are published
/// around it. Joining them at the moment of reading costs one comparison;
/// joining them at the moment of writing would cost either a dependency the
/// wrong way round or a thread whose only job is forwarding.
#[derive(Clone)]
pub struct BuildMonitor {
    step: ProgressPublisher<BuildStep>,
    download: ProgressPublisher<DownloadPhase>,
    cancel: Arc<AtomicBool>,
}

impl BuildMonitor {
    /// Starts a monitor already reporting `initial`.
    ///
    /// There is no empty state: a build that has been accepted is at some step
    /// from the moment it is listed, and `initial` is the one its source
    /// begins with. The worker replaces it with its own first report as soon
    /// as it runs.
    #[must_use]
    pub fn new(initial: BuildStep) -> Self {
        let step = ProgressPublisher::default();
        step.publish(initial);
        Self {
            step,
            download: ProgressPublisher::default(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Records the step the build has reached.
    pub fn report(&self, step: BuildStep) {
        log::debug!("a VM build reached {step:?}");
        self.step.publish(step);
    }

    /// The slot the image download publishes its bytes into.
    #[must_use]
    pub fn downloads(&self) -> &ProgressPublisher<DownloadPhase> {
        &self.download
    }

    /// The flag the long steps poll, for handing down to them.
    #[must_use]
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }

    /// Asks the build to stop at its next checkpoint.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Turns a cancellation into the error the build fails with.
    ///
    /// Cancelling is an ordinary failure on purpose: it then takes the same
    /// rollback every other failure takes, instead of a second cleanup path
    /// that can drift away from the first.
    pub fn check_cancelled(&self) -> Result<(), RepositoryError> {
        if self.is_cancelled() {
            return Err(RepositoryError::new("creating the VM was cancelled"));
        }
        Ok(())
    }

    /// What the watching thread shows for this build right now.
    #[must_use]
    pub fn snapshot(&self) -> BuildProgress {
        let step = self.step.snapshot().unwrap_or(BuildStep::Downloading);
        BuildProgress {
            step,
            download: match step {
                BuildStep::Downloading => self.download.snapshot(),
                _ => None,
            },
        }
    }
}
```

В шапке файла добавить `use crate::RepositoryError;`.

- [ ] **Step 4: Реэкспортировать из крейта**

`crates/core/src/lib.rs:11`:

```rust
pub use progress::{
    BuildMonitor, BuildProgress, BuildStep, DownloadPhase, ProgressPublisher, ProgressThrottle,
};
```

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test -p vmlord-core --lib progress`
Expected: PASS, четыре новых теста в том числе.

- [ ] **Step 6: Коммит**

```bash
git add crates/core/src
git commit -m "TASK-64: Add the build monitor a VM creation reports through"
```

---

### Task 3: состояние `VmState::Building` и его отображение

**Files:**
- Modify: `crates/core/src/lib.rs:87-92` (`VmState`)
- Modify: `crates/ui/src/lib.rs:993-998,1319-1339`
- Test: `crates/ui/src/lib.rs` (`mod tests`)

**Interfaces:**
- Consumes: `BuildProgress`, `BuildStep` из Task 2.
- Produces: `VmState::Building { progress: BuildProgress }`; в UI —
  `vm_state(state) -> &'static str`, возвращающая `"Building: downloading"`,
  `"Building: writing the disk"`, `"Building: provisioning"`,
  `"Building: registering"` для соответствующих шагов.

- [ ] **Step 1: Написать падающий тест на подпись состояния**

В `mod tests` в `crates/ui/src/lib.rs`:

```rust
/// `Starting` and `Building` are different things, and the label said
/// "Building" for `Starting` only because there was no building state yet.
#[test]
fn each_state_gets_its_own_label() {
    use vmlord_core::{BuildProgress, BuildStep};

    assert_eq!(vm_state(VmState::Stopped), "Stopped");
    assert_eq!(vm_state(VmState::Starting), "Starting");
    assert_eq!(
        vm_state(VmState::Building {
            progress: BuildProgress {
                step: BuildStep::Downloading,
                download: None,
            },
        }),
        "Building: downloading"
    );
    assert_eq!(
        vm_state(VmState::Building {
            progress: BuildProgress {
                step: BuildStep::Registering,
                download: None,
            },
        }),
        "Building: registering"
    );
}
```

- [ ] **Step 2: Прогнать и убедиться, что не компилируется**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-ui`
Expected: FAIL — `no variant named Building found for enum VmState`.

- [ ] **Step 3: Добавить вариант в домен**

`crates/core/src/lib.rs`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmState {
    Stopped,
    /// The VM is being created: nothing of it exists yet that could be
    /// started, stopped or deleted.
    Building {
        progress: BuildProgress,
    },
    Starting,
    Running {
        agent_status: AgentStatus,
    },
}
```

`BuildProgress` уже в области видимости через `pub use progress::{...}` из
Task 2.

- [ ] **Step 4: Обработать вариант в UI**

`crates/ui/src/lib.rs`, `vm_state` (строка 1335):

```rust
fn vm_state(state: VmState) -> &'static str {
    match state {
        VmState::Stopped => "Stopped",
        VmState::Building { progress } => match progress.step {
            BuildStep::Downloading => "Building: downloading",
            BuildStep::WritingDisk => "Building: writing the disk",
            BuildStep::Provisioning => "Building: provisioning",
            BuildStep::Registering => "Building: registering",
        },
        VmState::Starting => "Starting",
        VmState::Running { .. } => "Running",
    }
}
```

`agent_status` (строка 1319):

```rust
fn agent_status(state: VmState) -> AgentStatus {
    match state {
        VmState::Running { agent_status } => agent_status,
        VmState::Stopped | VmState::Building { .. } | VmState::Starting => AgentStatus::Unknown,
    }
}
```

Панель выбранной VM (строки 993–998 и группа `Edit` на строке ~1029):

```rust
    let primary_action = match vm.state {
        VmState::Stopped | VmState::Building { .. } => (VmAction::Start, "Start"),
        VmState::Starting | VmState::Running { .. } => (VmAction::Stop, "Stop"),
    };
    let is_running = matches!(vm.state, VmState::Running { .. });
    // A VM that is still being created has nothing to start, stop, edit or
    // delete yet: what exists of it is a directory the build still owns.
    let is_building = matches!(vm.state, VmState::Building { .. });
    let can_delete = matches!(vm.state, VmState::Stopped);
```

и в первом `render_action_group` заменить `true` на `!is_building` (с
подсказкой `Some("Available when the VM has finished building")`), в группе
`Edit` — `true` на `!is_building` (подсказку оставить прежней).

Добавить `BuildStep` в импорт `vmlord_core` в шапке `crates/ui/src/lib.rs`.

- [ ] **Step 5: Прогнать тесты и сборку**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-ui -p vmlord-core -p vmlord-app`
Expected: PASS. Компилятор укажет все оставшиеся неполные `match` по `VmState`
— обработать каждый; в `crates/platform/src/repository.rs` их пока нет, там
`VmState` только строится.

- [ ] **Step 6: Коммит**

```bash
git add crates/core/src/lib.rs crates/ui/src/lib.rs
git commit -m "TASK-64: Add the Building state and tell it apart from Starting"
```

---

### Task 4: `MetadataStore` переживает одновременные записи

**Files:**
- Modify: `crates/platform/src/metadata.rs:86-125` (`insert`), `:113-125` (`remove`)
- Test: `crates/platform/src/metadata.rs` (`mod tests`)

**Interfaces:**
- Consumes: ничего нового.
- Produces: прежние `MetadataStore::insert` и `::remove`, безопасные при
  одновременном вызове из нескольких потоков одного процесса.

- [ ] **Step 1: Написать падающий тест**

В `mod tests` в `crates/platform/src/metadata.rs`:

```rust
/// Parallel builds are the first thing to write metadata concurrently, and
/// `insert` is a read-modify-write: two writers finishing together would
/// otherwise drop one of the two VMs that had just been created.
#[test]
fn concurrent_inserts_keep_every_mapping() {
    let (root, store) = temp_store("concurrent-inserts");

    let mut workers = Vec::new();
    for index in 0..8 {
        let store = store.clone();
        workers.push(std::thread::spawn(move || {
            store
                .insert(VmComputeSystemMapping {
                    vm_id: Uuid::new_v4(),
                    vm_name: format!("vm-{index}"),
                    hcs_compute_system_id: format!("vmlord-{index}"),
                    disk_gb: 1,
                    endpoint_id: None,
                    network_mode: NetworkMode::None,
                })
                .expect("each mapping should be stored");
        }));
    }
    for worker in workers {
        worker.join().expect("no writer should panic");
    }

    assert_eq!(store.list().unwrap().len(), 8);
    let _ = fs::remove_dir_all(root);
}
```

Если в `mod tests` этого файла нет хелпера `temp_store`, написать его по
образцу `repository.rs:703-710` (каталог в `std::env::temp_dir()` с меткой и
`std::process::id()`), и импортировать `Uuid`, `NetworkMode`, `fs`.

- [ ] **Step 2: Прогнать и убедиться, что падает**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform metadata::tests::concurrent_inserts`
Expected: FAIL — записей меньше восьми (тест нестабильный по своей природе;
достаточно одного падения из нескольких прогонов, чтобы увидеть проблему).

- [ ] **Step 3: Взять чтение-изменение-запись под замок**

В `crates/platform/src/metadata.rs` рядом с константами:

```rust
/// Serializes the read-modify-write of the mapping document.
///
/// Creating a VM runs on its own thread, so two builds finishing at the same
/// moment would both read the document, both add their own VM and both write
/// it back -- and one of the two VMs would be gone from a file that reported
/// success twice. The lock is process-wide because a `MetadataStore` is a path
/// and nothing else: two stores over the same file are the same document.
///
/// Two VMLord processes over one storage root are not covered, and are not a
/// case this task creates.
static DOCUMENT_LOCK: Mutex<()> = Mutex::new(());
```

и в начале `insert` и `remove`:

```rust
        let _guard = DOCUMENT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
```

Он должен стоять до первого `self.load()` и жить до конца функции. Импорт:
`use std::sync::Mutex;`.

- [ ] **Step 4: Прогнать тест**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform metadata`
Expected: PASS, устойчиво при повторных прогонах.

- [ ] **Step 5: Коммит**

```bash
git add crates/platform/src/metadata.rs
git commit -m "TASK-64: Keep concurrent metadata writes from losing a VM"
```

---

### Task 5: конвейер сообщает стадии и проверяет отмену

**Files:**
- Modify: `crates/platform/src/create.rs:24-37` (типы швов), `:52-84`
  (`production`/`for_test`), `:88-208` (`create`)
- Modify: `crates/vmlord/src/main.rs:4-6,74-95`
- Modify: `crates/platform/tests/hyperv.rs:1525-1540` (замыкание импортёра)
- Test: `crates/platform/src/create.rs` (`mod tests`)

**Interfaces:**
- Consumes: `vmlord_core::{BuildMonitor, BuildStep}` из Task 2.
- Produces:
  - `pub type CloudDiskImporter = Box<dyn Fn(&CloudImage, u64, &Path, &BuildMonitor) -> Result<(), RepositoryError> + Send + Sync>;`
  - `VmCreationPipeline::create(&self, store: &MetadataStore, request: &VmCreateRequest, vm_directory: &Path, monitor: &BuildMonitor) -> Result<VmComputeSystemMapping, RepositoryError>`
  - `VmCreationPipeline: Send + Sync`.

- [ ] **Step 1: Написать падающие тесты**

В `mod tests` в `crates/platform/src/create.rs` добавить хелпер и тесты.
Хелпер (рядом с `fixture`):

```rust
    use vmlord_core::{BuildMonitor, BuildStep};

    fn monitor() -> BuildMonitor {
        BuildMonitor::new(BuildStep::WritingDisk)
    }
```

Тесты:

```rust
    #[test]
    fn a_local_media_build_reports_its_steps_in_order() {
        let fixture = fixture("steps-local");
        let calls = fixture.calls.clone();
        let monitor = monitor();
        // Each injected seam records the step the pipeline had reported by the
        // time it was called, which is what "in order" can be checked against.
        let seen: Arc<Mutex<Vec<BuildStep>>> = Arc::new(Mutex::new(Vec::new()));
        let pipeline = observing_pipeline(&calls, &monitor, &seen);

        pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor,
            )
            .expect("creation should succeed");

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[
                BuildStep::WritingDisk,
                BuildStep::Provisioning,
                BuildStep::Provisioning,
                BuildStep::Registering,
            ],
            "the disk, then the files written for the VM and their grants, \
             then the compute system"
        );
        assert_eq!(monitor.snapshot().step, BuildStep::Registering);
    }

    #[test]
    fn a_cancelled_build_stops_before_touching_the_disk() {
        let fixture = fixture("cancelled-early");
        let calls = fixture.calls.clone();
        let monitor = monitor();
        monitor.cancel();
        let pipeline = pipeline(&calls, false, false, false);

        let error = pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor,
            )
            .expect_err("a cancelled build must not create a VM");

        assert!(error.to_string().contains("cancelled"), "got {error}");
        assert!(calls.vhd.lock().unwrap().is_empty());
        assert!(calls.create.lock().unwrap().is_empty());
        assert!(!fixture.vm_directory.exists());
        assert!(fixture.store.list().unwrap().is_empty());
    }

    #[test]
    fn a_build_cancelled_while_writing_the_disk_is_rolled_back() {
        let fixture = fixture("cancelled-midway");
        let calls = fixture.calls.clone();
        let monitor = monitor();
        let pipeline = VmCreationPipeline::for_test(
            {
                let calls = calls.clone();
                let monitor = monitor.clone();
                move |path: &std::path::Path, size| {
                    calls.vhd.lock().unwrap().push((path.to_path_buf(), size));
                    fs::write(path, b"vhdx").unwrap();
                    // The user pressed Cancel while the disk was being written.
                    monitor.cancel();
                    Ok(())
                }
            },
            |_: &CloudImage, _, _: &std::path::Path, _: &BuildMonitor| Ok(()),
            |_, _| Ok(()),
            {
                let calls = calls.clone();
                move |id: &str, config: &str| {
                    calls
                        .create
                        .lock()
                        .unwrap()
                        .push((id.to_owned(), config.to_owned()));
                    Ok(())
                }
            },
            |_| Ok(()),
        );

        let error = pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor,
            )
            .expect_err("a cancelled build must not create a VM");

        assert!(error.to_string().contains("cancelled"), "got {error}");
        assert!(
            calls.create.lock().unwrap().is_empty(),
            "cancellation must be noticed before the compute system is created"
        );
        assert!(!fixture.vm_directory.exists());
        assert!(fixture.store.list().unwrap().is_empty());
    }
```

И наблюдающий конвейер рядом с `fn pipeline`:

```rust
    /// A pipeline whose seams record the step the monitor was reporting when
    /// each of them ran.
    fn observing_pipeline(
        calls: &Calls,
        monitor: &BuildMonitor,
        seen: &Arc<Mutex<Vec<BuildStep>>>,
    ) -> VmCreationPipeline {
        // Boxed so the same recorder can be handed to three seams; a plain
        // closure would be moved into the first of them.
        let record: Arc<dyn Fn() + Send + Sync> = Arc::new({
            let monitor = monitor.clone();
            let seen = Arc::clone(seen);
            move || seen.lock().unwrap().push(monitor.snapshot().step)
        });
        VmCreationPipeline::for_test(
            {
                let calls = calls.clone();
                let record = Arc::clone(&record);
                move |path: &std::path::Path, size| {
                    record();
                    calls.vhd.lock().unwrap().push((path.to_path_buf(), size));
                    fs::write(path, b"vhdx")
                        .map_err(|error| vmlord_core::RepositoryError::new(format!("vhd: {error}")))
                }
            },
            |_: &CloudImage, _, _: &std::path::Path, _: &BuildMonitor| Ok(()),
            {
                let record = Arc::clone(&record);
                move |_: &str, _: &std::path::Path| {
                    record();
                    Ok(())
                }
            },
            {
                let record = Arc::clone(&record);
                move |_: &str, _: &str| {
                    record();
                    Ok(())
                }
            },
            |_| Ok(()),
        )
    }
```

- [ ] **Step 2: Прогнать и убедиться, что не компилируется**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform create::`
Expected: FAIL — `create` принимает 3 аргумента, а не 4.

- [ ] **Step 3: Провести монитор через швы и конвейер**

`crates/platform/src/create.rs`:

```rust
type VhdCreator = Box<dyn Fn(&Path, u64) -> Result<(), RepositoryError> + Send + Sync>;
type AccessGranter = Box<dyn Fn(&str, &Path) -> Result<(), RepositoryError> + Send + Sync>;
type SystemCreator = Box<dyn Fn(&str, &str) -> Result<(), RepositoryError> + Send + Sync>;

/// Makes the VM's system disk out of a cloud image: fetch the image the release
/// means, then write it into a VHDX at the given path, sized for the VM rather
/// than for the image.
///
/// Injected rather than called directly because the fetching half is not
/// Windows's business: it lives in `vmlord-image`, which knows no Windows API,
/// and the composition root joins the two. The pipeline keeps the half that is
/// Windows -- writing into a VHDX through the disk it is attached as.
///
/// The monitor comes with it because both halves are long: the importer
/// reports `Downloading` and `WritingDisk` itself, and passes the cancellation
/// flag down to the download. Whoever runs a step is who reports it -- from
/// outside this closure the two are one call.
///
/// `Send + Sync` because creation runs on its own thread, and every seam of the
/// pipeline goes with it.
pub type CloudDiskImporter = Box<
    dyn Fn(&CloudImage, u64, &Path, &BuildMonitor) -> Result<(), RepositoryError> + Send + Sync,
>;
```

`SystemTeardown` в `cleanup.rs` тоже должен получить `+ Send + Sync` — проверить
его объявление и дополнить.

`for_test` — те же `+ Send + Sync` на каждом `impl Fn`, и у `cloud_disk`
четвёртый параметр `&BuildMonitor`.

`create`: сигнатура получает `monitor: &BuildMonitor`, и внутри замыкания:

```rust
        let result = (|| {
            monitor.check_cancelled()?;
            match &request.source {
                VmSource::LocalMedia { .. } => {
                    monitor.report(BuildStep::WritingDisk);
                    (self.vhd_creator)(&system_disk_path, disk_size_bytes)?;
                    if !media_path.is_file() {
                        return Err(RepositoryError::new(format!(
                            "VM image no longer exists: {}",
                            media_path.display()
                        )));
                    }
                }
                VmSource::CloudImage {
                    image,
                    provisioning,
                } => {
                    log::debug!(
                        "importing {} {} into {}",
                        image.profile.name,
                        image.release,
                        system_disk_path.display()
                    );
                    // The importer reports `Downloading` and `WritingDisk`
                    // itself: both happen inside this one call.
                    (self.cloud_disk)(image, disk_size_bytes, &system_disk_path, monitor)?;
                    monitor.check_cancelled()?;
                    monitor.report(BuildStep::Provisioning);
                    write_provisioning(
                        vm_directory,
                        &seed_path,
                        &request.name,
                        &hcs_compute_system_id,
                        image,
                        provisioning,
                    )?;
                }
            }
            monitor.check_cancelled()?;
            // Local media reaches provisioning here: it writes no seed and no
            // keys, but the configuration and the grants are still files
            // written for the VM.
            monitor.report(BuildStep::Provisioning);

            fs::write(layout::configuration_path(vm_directory), &configuration).map_err(
                |error| RepositoryError::new(format!("failed to write HCS configuration: {error}")),
            )?;

            // Hyper-V opens VM-owned files under the VM's own security
            // principal, not the creating user's token: without this, start
            // fails with access denied even though both files exist and are
            // readable by this (elevated) process.
            (self.access_granter)(&hcs_compute_system_id, &system_disk_path)?;
            (self.access_granter)(&hcs_compute_system_id, &media_path)?;

            monitor.check_cancelled()?;
            monitor.report(BuildStep::Registering);
            (self.system_creator)(&hcs_compute_system_id, &configuration)?;
            system_created = true;

            store.insert(mapping.clone())?;
            Ok(())
        })();
```

(повторный `report(Provisioning)` для cloud-пути безвреден — слот уровневый, и
именно поэтому тест ожидает `Provisioning` дважды подряд у грантов.)

- [ ] **Step 4: Обновить остальные вызовы**

* Все существующие вызовы `pipeline.create(...)` в `mod tests` этого файла
  получают четвёртым аргументом `&monitor()`.
* `crates/platform/src/repository.rs:492-494` — временно
  `self.creation.create(&self.store, &request, &vm_directory, &BuildMonitor::new(BuildStep::WritingDisk)).map(|_mapping| ())`
  (Task 9 заменит это на поток), и замыкание-заглушка в `fn repository()`
  тестов получает четвёртый параметр.
* `crates/platform/tests/hyperv.rs:43` (`no_cloud_images`) — замыкание
  получает четвёртый параметр: `Box::new(|_, _, _, _| Err(...))`.
* `crates/platform/tests/hyperv.rs:1524-1535` — замыкание импортёра получает
  `|image, size, target, monitor: &vmlord_core::BuildMonitor|`, сообщает
  `BuildStep::Downloading` и `BuildStep::WritingDisk` и передаёт
  `monitor.downloads()` и `monitor.cancel_flag()` в `open_cloud_image` вместо
  `ProgressPublisher::default()` и `AtomicBool::new(false)`; вызов
  `pipeline.create(&store, &request, &vm_directory)` на строке 1560 получает
  четвёртым аргументом `&vmlord_core::BuildMonitor::new(vmlord_core::BuildStep::Downloading)`.
* `crates/vmlord/src/main.rs`:

```rust
/// Joins the two halves of getting a cloud image onto a VM's disk: fetching it,
/// which knows nothing of Windows and lives in `vmlord-image`, and writing it
/// into a VHDX, which is `vmlord-platform`'s business.
///
/// The composition root is where they meet, which is what keeps the network out
/// of the Windows layer.
///
/// Both halves are long enough to report and to be cancelled, and both are
/// invisible from outside this closure, so the steps are reported here.
fn cloud_disk_importer(cache_directory: PathBuf) -> vmlord_platform::CloudDiskImporter {
    Box::new(move |image, disk_size_bytes, target, monitor: &BuildMonitor| {
        monitor.report(BuildStep::Downloading);
        let mut source = vmlord_image::open_cloud_image(
            &image.profile,
            &image.release,
            &cache_directory,
            disk_size_bytes,
            monitor.downloads(),
            monitor.cancel_flag(),
        )?;
        monitor.report(BuildStep::WritingDisk);
        vmlord_platform::import_image(&mut source, target, disk_size_bytes).map(|_summary| ())
    })
}
```

Импорты в `main.rs`: `use vmlord_core::{AppSettings, BuildMonitor, BuildStep, VmRepository};`,
и убрать ставшие ненужными `ProgressPublisher` и `std::sync::atomic::AtomicBool`.

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform create::` и
`cargo build --target=x86_64-pc-windows-gnu`
Expected: PASS; сборка всего workspace без ошибок.

- [ ] **Step 6: Коммит**

```bash
git add crates/platform/src crates/vmlord/src/main.rs crates/platform/tests/hyperv.rs
git commit -m "TASK-64: Report creation steps and honour cancellation in the pipeline"
```

---

### Task 6: страж, убирающий за паникой

**Files:**
- Modify: `crates/platform/src/create.rs:146-232` (`create` и `rollback`)
- Test: `crates/platform/src/create.rs` (`mod tests`)

**Interfaces:**
- Consumes: `SystemTeardown`, `cleanup::remove_vm_directory`.
- Produces: поведение — паника внутри `create` не оставляет ни каталога VM, ни
  созданной HCS-системы. Публичная сигнатура не меняется.

- [ ] **Step 1: Написать падающий тест**

```rust
    /// A build runs on its own thread, and a panic there would otherwise leave
    /// the VM's directory behind for good: nothing else knows it was ever
    /// being created.
    #[test]
    fn a_panicking_step_leaves_no_vm_directory_behind() {
        let fixture = fixture("panicking");
        let calls = fixture.calls.clone();
        let pipeline = VmCreationPipeline::for_test(
            {
                let calls = calls.clone();
                move |path: &std::path::Path, size| {
                    calls.vhd.lock().unwrap().push((path.to_path_buf(), size));
                    fs::write(path, b"vhdx").unwrap();
                    Ok(())
                }
            },
            |_: &CloudImage, _, _: &std::path::Path, _: &BuildMonitor| Ok(()),
            |_, _| Ok(()),
            |_: &str, _: &str| panic!("the HCS client panicked"),
            {
                let calls = calls.clone();
                move |id: &str| {
                    calls.teardown.lock().unwrap().push(id.to_owned());
                    Ok(())
                }
            },
        );

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = pipeline.create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            );
        }));

        assert!(panicked.is_err(), "the panic must reach the caller");
        assert!(
            !fixture.vm_directory.exists(),
            "the guard must remove what the interrupted build had created"
        );
        assert!(fixture.store.list().unwrap().is_empty());
    }
```

`catch_unwind` здесь — инструмент теста, а не продакшн-кода: он ловит панику,
которую страж уже отработал.

- [ ] **Step 2: Прогнать и убедиться, что падает**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform create::tests::a_panicking_step`
Expected: FAIL на `!fixture.vm_directory.exists()` — каталог остался.

- [ ] **Step 3: Заменить флаг `system_created` стражем**

В `crates/platform/src/create.rs` перед `impl VmCreationPipeline`:

```rust
/// Removes what a creation had built if it leaves without disarming this.
///
/// The `Err` path disarms the guard and rolls back explicitly, because the
/// error the caller sees has to be able to say what the rollback itself could
/// not do. What is left for the guard is the path with no `Err` to carry a
/// message: a panic, which would otherwise leave a VM directory -- and
/// possibly a compute system -- that nothing else knows about.
///
/// `catch_unwind` would be the other way to do this, and cannot be: the
/// pipeline's seams are boxed closures, which are not `UnwindSafe`, and
/// `AssertUnwindSafe` would assert exactly what needs proving.
struct CreationGuard<'a> {
    vm_directory: &'a Path,
    teardown: &'a SystemTeardown,
    hcs_compute_system_id: &'a str,
    system_created: bool,
    armed: bool,
}

impl Drop for CreationGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        log::error!(
            "creating the VM at {} was interrupted; removing what it had created",
            self.vm_directory.display()
        );
        if self.system_created
            && let Err(error) = (self.teardown)(self.hcs_compute_system_id)
        {
            log::error!(
                "the compute system \"{}\" of the interrupted creation could not be \
                 torn down: {error}",
                self.hcs_compute_system_id
            );
        }
        if let Err(error) = cleanup::remove_vm_directory(self.vm_directory) {
            log::error!(
                "the directory of the interrupted creation at {} could not be removed: {error}",
                self.vm_directory.display()
            );
        }
    }
}
```

В `create` заменить `let mut system_created = false;` на

```rust
        let mut guard = CreationGuard {
            vm_directory,
            teardown: &self.system_teardown,
            hcs_compute_system_id: &hcs_compute_system_id,
            system_created: false,
            armed: true,
        };
```

замыкание сделать принимающим страж — `let result = (|guard: &mut CreationGuard| { ... })(&mut guard);`
с `guard.system_created = true;` вместо `system_created = true;`, а разбор
результата — на:

```rust
        guard.armed = false;
        match result {
            Ok(()) => {
                log::info!("created VM \"{}\" ({vm_id})", request.name);
                Ok(mapping)
            }
            Err(error) => Err(self.rollback(vm_directory, &mapping, guard.system_created, error)),
        }
```

`hcs_compute_system_id` заимствуется стражем, поэтому строку, уходящую в
`mapping`, брать через `.clone()` до создания стража — она уже так и берётся
(`create.rs:138`).

- [ ] **Step 4: Прогнать тесты**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform create::`
Expected: PASS, включая все прежние тесты отката.

- [ ] **Step 5: Коммит**

```bash
git add crates/platform/src/create.rs
git commit -m "TASK-64: Undo an interrupted creation from a drop guard"
```

---

### Task 7: заливку в VHDX можно прервать

**Files:**
- Modify: `crates/platform/src/import.rs:52-103`
- Modify: `crates/platform/src/import/copy.rs:68-115`
- Modify: `crates/vmlord/src/main.rs` (вызов `import_image`)
- Modify: `crates/platform/tests/import.rs:86,117,144`
- Modify: `crates/platform/tests/hyperv.rs:1534`
- Test: `crates/platform/src/import/copy.rs` (`mod tests`)

**Interfaces:**
- Consumes: `std::sync::atomic::AtomicBool`.
- Produces:
  `pub fn import_image(source: &mut dyn Read, target: &Path, disk_size_bytes: u64, cancel: &AtomicBool) -> Result<ImportSummary, RepositoryError>`
  — прерывается на границе чанка, удаляя недописанный VHDX тем же путём, что и
  при любой другой ошибке.

- [ ] **Step 1: Написать падающий тест**

В `mod tests` в `crates/platform/src/import/copy.rs`:

```rust
    /// Writing a disk is the second-longest step of creating a VM, and a
    /// cancellation that only takes effect after it would not be a
    /// cancellation.
    #[test]
    fn a_cancelled_copy_stops_and_reports_why() {
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let mut source = Cursor::new(vec![7u8; CHUNK * 4]);
        let mut disk = MemoryDisk::new(CHUNK * 4);

        let error = copy_image(&mut source, &mut disk, (CHUNK * 4) as u64, CHUNK, &cancel)
            .expect_err("a cancelled copy must not report success");

        assert!(error.to_string().contains("cancelled"), "got {error}");
        assert!(disk.write_offsets().is_empty());
    }
```

- [ ] **Step 2: Прогнать и убедиться, что не компилируется**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform import::copy`
Expected: FAIL — `copy_image` принимает 4 аргумента.

- [ ] **Step 3: Провести флаг через копирование**

`crates/platform/src/import/copy.rs` — в сигнатуру `copy_image` добавить
`cancel: &AtomicBool` (импорт `use std::sync::atomic::{AtomicBool, Ordering};`),
в docstring — абзац:

```
/// `cancel` is polled once per chunk. A build is cancelled by the user or by
/// VMLord shutting down, and a copy that only noticed afterwards would hold
/// both for as long as a disk takes to write.
```

и первым делом внутри `loop`:

```rust
        if cancel.load(Ordering::Relaxed) {
            return Err(repository_error("writing the disk was cancelled".to_owned()));
        }
```

- [ ] **Step 4: Провести флаг через `import_image`**

`crates/platform/src/import.rs`: `import_image` и `write_into` получают
`cancel: &AtomicBool`, `write_into` передаёт его в `copy_image`. В docstring
`import_image` — строка:

```
/// A cancelled import leaves nothing behind either: it takes the same path a
/// failed one takes, because a cancellation here is an ordinary failure.
```

- [ ] **Step 5: Обновить вызывающих**

* `crates/vmlord/src/main.rs` — `import_image(&mut source, target, disk_size_bytes, monitor.cancel_flag())`.
* `crates/platform/tests/import.rs:86,117,144` — четвёртым аргументом
  `&AtomicBool::new(false)`.
* `crates/platform/tests/hyperv.rs:1534` — так же, через `monitor.cancel_flag()`.

- [ ] **Step 6: Прогнать тесты**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform import`
Expected: PASS.

- [ ] **Step 7: Коммит**

```bash
git add crates/platform/src/import.rs crates/platform/src/import/copy.rs \
        crates/platform/tests/import.rs crates/platform/tests/hyperv.rs \
        crates/vmlord/src/main.rs
git commit -m "TASK-64: Let a disk import be cancelled between chunks"
```

---

### Task 8: реестр строящихся VM

**Files:**
- Create: `crates/platform/src/build.rs`
- Modify: `crates/platform/src/lib.rs` (объявление модуля)
- Test: `crates/platform/src/build.rs` (`mod tests`)

**Interfaces:**
- Consumes: `vmlord_core::{BuildMonitor, BuildStep, GpuMode, RepositoryError, VmCreateRequest, VmSource, VmState, VmSummary}`.
- Produces (всё `pub(crate)`):
  - `struct BuildRegistry` с `Default`;
  - `fn contains(&self, name: &str) -> bool`;
  - `fn start<F>(&self, request: VmCreateRequest, build: F) -> Result<(), RepositoryError> where F: FnOnce(&BuildMonitor) + Send + 'static`;
  - `fn summaries(&self) -> Vec<VmSummary>`;
  - `fn cancel(&self, name: &str) -> Result<(), RepositoryError>`;
  - `fn reap(&self)`;
  - `fn cancel_all_and_join(&self)`;
  - `fn refuse_if_building(&self, name: &str) -> Result<(), RepositoryError>`.

- [ ] **Step 1: Написать падающие тесты**

`crates/platform/src/build.rs`, `mod tests`:

```rust
#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use vmlord_core::{
        BuildStep, CloudImage, GpuMode, NetworkMode, Provisioning, SshAccess, VmCreateRequest,
        VmSource, VmState,
    };

    use super::BuildRegistry;

    fn request(name: &str) -> VmCreateRequest {
        VmCreateRequest {
            name: name.into(),
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: vmlord_core::ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "dev".into(),
                    password: None,
                    ssh: SshAccess::Enabled { deploy_key: true },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                },
            },
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        }
    }

    #[test]
    fn a_started_build_is_listed_as_building_until_it_finishes() {
        let registry = BuildRegistry::default();
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&release);
        registry
            .start(request("dev"), move |monitor| {
                monitor.report(BuildStep::WritingDisk);
                while !held.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
            })
            .expect("the build should start");

        let summaries = registry.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "dev");
        assert_eq!(summaries[0].ram_mb, 2048);
        assert_eq!(summaries[0].disk_gb, 20);
        assert_eq!(summaries[0].cpu_cores, 2);
        assert_eq!(summaries[0].ip_address, None);
        assert!(matches!(summaries[0].state, VmState::Building { .. }));

        release.store(true, Ordering::Relaxed);
        registry.cancel_all_and_join();
        registry.reap();

        assert!(registry.summaries().is_empty());
    }

    #[test]
    fn a_second_build_of_the_same_name_is_refused() {
        let registry = BuildRegistry::default();
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&release);
        registry
            .start(request("dev"), move |_| {
                while !held.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
            })
            .expect("the first build should start");

        let error = registry
            .start(request("dev"), |_| panic!("this build must never run"))
            .expect_err("two builds must not share a name and a directory");

        assert!(error.to_string().contains("dev"), "got {error}");
        release.store(true, Ordering::Relaxed);
        registry.cancel_all_and_join();
    }

    #[test]
    fn cancelling_sets_the_flag_the_build_polls() {
        let registry = BuildRegistry::default();
        let seen = Arc::new(AtomicBool::new(false));
        let reporter = Arc::clone(&seen);
        registry
            .start(request("dev"), move |monitor| {
                while !monitor.is_cancelled() {
                    std::thread::yield_now();
                }
                reporter.store(true, Ordering::Relaxed);
            })
            .expect("the build should start");

        registry.cancel("dev").expect("a running build is cancellable");
        registry.cancel_all_and_join();

        assert!(seen.load(Ordering::Relaxed));
    }

    #[test]
    fn cancelling_an_unknown_build_says_so() {
        let registry = BuildRegistry::default();

        let error = registry
            .cancel("ghost")
            .expect_err("there is nothing to cancel");

        assert!(error.to_string().contains("ghost"), "got {error}");
    }

    #[test]
    fn a_panicking_build_is_still_reaped() {
        let registry = BuildRegistry::default();
        registry
            .start(request("dev"), |_| panic!("the build thread panicked"))
            .expect("the build should start");

        registry.cancel_all_and_join();
        registry.reap();

        assert!(
            registry.summaries().is_empty(),
            "a build that panicked is over, and a row for it would never go away"
        );
    }

    #[test]
    fn operations_on_a_building_vm_are_refused_by_name() {
        let registry = BuildRegistry::default();
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&release);
        registry
            .start(request("dev"), move |_| {
                while !held.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
            })
            .expect("the build should start");

        let error = registry
            .refuse_if_building("dev")
            .expect_err("a VM that does not exist yet cannot be acted on");

        assert!(error.to_string().contains("dev"), "got {error}");
        assert!(error.to_string().contains("still being created"), "got {error}");
        assert!(registry.refuse_if_building("other").is_ok());

        release.store(true, Ordering::Relaxed);
        registry.cancel_all_and_join();
    }
}
```

- [ ] **Step 2: Прогнать и убедиться, что модуля нет**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform build::`
Expected: FAIL — `file not found for module build`.

- [ ] **Step 3: Написать реестр**

`crates/platform/src/build.rs`:

```rust
//! The VMs being created right now, and the threads creating them.
//!
//! Creating a VM takes minutes -- an image is fetched, a disk is written, a
//! compute system is made -- and it used to take them on the caller's thread,
//! which is the UI's. Here each creation gets a thread of its own, and the
//! registry is what the UI sees instead: a VM that exists as a build and not
//! yet as a VM.
//!
//! Modelled on `dhcp::DhcpService`, the only other background thread in
//! VMLord: a shared flag to stop by, a handle to join, and a `Drop` that does
//! both. There is no async runtime here and none anywhere in the project.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, atomic::{AtomicBool, Ordering}},
    thread::JoinHandle,
};

use vmlord_core::{
    BuildMonitor, BuildStep, GpuMode, RepositoryError, VmCreateRequest, VmSource, VmState,
    VmSummary,
};

/// Every VM VMLord creates today is a Linux guest.
const OS_TYPE: &str = "Linux";

/// One VM being created.
struct Build {
    monitor: BuildMonitor,
    /// What the VM will be, for listing it before it is.
    request: VmCreateRequest,
    /// Set by the worker as it leaves, by whichever exit -- returning or
    /// panicking -- so that a build that died still stops being listed.
    finished: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

/// The VMs being created, by name.
#[derive(Default)]
pub(crate) struct BuildRegistry {
    builds: Mutex<HashMap<String, Build>>,
}

impl BuildRegistry {
    /// Whether a VM of this name is being created right now.
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.lock().contains_key(name)
    }

    /// Starts `build` on a thread of its own, listing the VM as building until
    /// it returns.
    ///
    /// `build` must not touch the registry: it runs while nothing holds the
    /// lock, but the entry it belongs to is inserted by the caller of this
    /// function while the lock is held.
    pub(crate) fn start<F>(
        &self,
        request: VmCreateRequest,
        build: F,
    ) -> Result<(), RepositoryError>
    where
        F: FnOnce(&BuildMonitor) + Send + 'static,
    {
        let mut builds = self.lock();
        if builds.contains_key(&request.name) {
            let error = RepositoryError::new(format!(
                "VM \"{}\" is already being created",
                request.name
            ));
            log::error!("{error}");
            return Err(error);
        }

        let monitor = BuildMonitor::new(first_step(&request.source));
        let finished = Arc::new(AtomicBool::new(false));
        let worker = std::thread::Builder::new()
            .name(format!("vmlord-build-{}", request.name))
            .spawn({
                let monitor = monitor.clone();
                let finished = Arc::clone(&finished);
                move || {
                    // Set on the way out however the build leaves, panic
                    // included: an entry nobody clears is a row that never
                    // goes away.
                    let _finish = Finish(finished);
                    build(&monitor);
                }
            })
            .map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the thread creating VM \"{}\" could not be started: {error}",
                    request.name
                ));
                log::error!("{error}");
                error
            })?;

        log::info!("started creating VM \"{}\" in the background", request.name);
        builds.insert(
            request.name.clone(),
            Build {
                monitor,
                request,
                finished,
                worker: Some(worker),
            },
        );
        Ok(())
    }

    /// The VMs being created, as the list shows them.
    ///
    /// Sizes come from the request rather than from disk, because nothing of
    /// the VM is on disk yet to read them from.
    pub(crate) fn summaries(&self) -> Vec<VmSummary> {
        self.lock()
            .values()
            .map(|build| VmSummary {
                name: build.request.name.clone(),
                os_type: OS_TYPE.to_owned(),
                state: VmState::Building {
                    progress: build.monitor.snapshot(),
                },
                ram_mb: build.request.ram_mb,
                disk_gb: build.request.disk_gb,
                cpu_cores: build.request.cpu_cores,
                gpu_mode: GpuMode::None,
                network_mode: build.request.network_mode,
                // A VM that does not exist answers nowhere.
                ip_address: None,
                ssh_port: None,
            })
            .collect()
    }

    /// Asks the build of `name` to stop at its next checkpoint.
    ///
    /// Returning here does not mean the build is over: it means it has been
    /// told. The build rolls itself back and disappears from the list on its
    /// own.
    pub(crate) fn cancel(&self, name: &str) -> Result<(), RepositoryError> {
        let builds = self.lock();
        let Some(build) = builds.get(name) else {
            let error = RepositoryError::new(format!("VM \"{name}\" is not being created"));
            log::error!("{error}");
            return Err(error);
        };
        log::warn!("cancelling the creation of VM \"{name}\"");
        build.monitor.cancel();
        Ok(())
    }

    /// Refuses an operation on a VM that is still being created.
    ///
    /// "Not found" would be the wrong answer and the confusing one: the VM is
    /// in the list the user is looking at.
    pub(crate) fn refuse_if_building(&self, name: &str) -> Result<(), RepositoryError> {
        if !self.contains(name) {
            return Ok(());
        }
        let error = RepositoryError::new(format!("VM \"{name}\" is still being created"));
        log::error!("{error}");
        Err(error)
    }

    /// Removes and joins the builds that have finished.
    ///
    /// Joining a thread that has already left is immediate, and it is the only
    /// place its result is collected: a build reports what it did through the
    /// diagnostics, so there is nothing here to read but the end of the thread.
    pub(crate) fn reap(&self) {
        let mut builds = self.lock();
        let done: Vec<String> = builds
            .iter()
            .filter(|(_, build)| build.finished.load(Ordering::Relaxed))
            .map(|(name, _)| name.clone())
            .collect();
        for name in done {
            if let Some(mut build) = builds.remove(&name)
                && let Some(worker) = build.worker.take()
                && worker.join().is_err()
            {
                log::error!("the thread creating VM \"{name}\" panicked");
            }
        }
    }

    /// Cancels every build and waits for all of them.
    ///
    /// Called as VMLord shuts down. Leaving without it would either kill a
    /// thread in the middle of writing a VHDX or hang the process waiting for
    /// one that was never told to stop.
    pub(crate) fn cancel_all_and_join(&self) {
        let mut builds = self.lock();
        for build in builds.values() {
            build.monitor.cancel();
        }
        for (name, mut build) in builds.drain() {
            if let Some(worker) = build.worker.take()
                && worker.join().is_err()
            {
                log::error!("the thread creating VM \"{name}\" panicked");
            }
        }
    }

    /// Recovers a poisoned lock rather than propagating the panic: a build
    /// that panicked must not take the list of VMs down with it.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Build>> {
        self.builds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The step a build of this source begins at, for the moments before its
/// thread has reported one of its own.
fn first_step(source: &VmSource) -> BuildStep {
    match source {
        VmSource::CloudImage { .. } => BuildStep::Downloading,
        VmSource::LocalMedia { .. } => BuildStep::WritingDisk,
    }
}

/// Marks a build as over as it is dropped, however its thread left.
struct Finish(Arc<AtomicBool>);

impl Drop for Finish {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
```

- [ ] **Step 4: Объявить модуль**

В `crates/platform/src/lib.rs` рядом с остальными: `mod build;`.

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform build::`
Expected: PASS, все шесть тестов. Предупреждения `never used` на методах
реестра здесь ожидаемы — вызывающий появляется в Task 9; workspace их не
запрещает (`Cargo.toml:25-29`).

- [ ] **Step 6: Коммит**

```bash
git add crates/platform/src/build.rs crates/platform/src/lib.rs
git commit -m "TASK-64: Add the registry of VMs being created"
```

---

### Task 9: репозиторий создаёт VM в фоне

**Files:**
- Modify: `crates/platform/src/repository.rs:8-13,39-85,488-495,603-610,619-653`
- Modify: `crates/platform/src/repository.rs` (`mod tests`, `fn repository`)
- Test: `crates/platform/src/repository.rs` (`mod tests`)

**Interfaces:**
- Consumes: `BuildRegistry` из Task 8, `VmCreationPipeline::create(.., &BuildMonitor)`
  из Task 5.
- Produces: `HcsVmRepository` с полями `creation: Arc<VmCreationPipeline>`,
  `builds: Arc<BuildRegistry>`, `diagnostics: Arc<Mutex<Vec<Diagnostic>>>` и
  `impl Drop`. `create_vm` возвращается немедленно; `list_vms` включает
  строящиеся VM.

- [ ] **Step 1: Написать падающий тест на порядок проверок**

Проверки, которые можно поставить без живого HCS, — отказ по строящемуся имени.
В `mod tests` в `crates/platform/src/repository.rs`:

```rust
    /// A build in flight is not in the metadata store yet, so without this the
    /// duplicate-name check would let a second creation through and the two
    /// would fight over one directory.
    #[test]
    fn a_name_being_built_counts_as_taken() {
        let repository = repository();
        let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let held = std::sync::Arc::clone(&release);
        repository
            .builds
            .start(create_request("dev"), move |_| {
                while !held.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::yield_now();
                }
            })
            .expect("the build should start");

        let error = repository
            .builds
            .refuse_if_building("dev")
            .expect_err("a VM that is still being created cannot be started or deleted");
        assert!(error.to_string().contains("still being created"));
        assert!(repository.builds.contains("dev"));

        release.store(true, std::sync::atomic::Ordering::Relaxed);
        repository.builds.cancel_all_and_join();
    }
```

и хелпер рядом с `fn update_request`:

```rust
    fn create_request(name: &str) -> vmlord_core::VmCreateRequest {
        vmlord_core::VmCreateRequest {
            name: name.into(),
            source: vmlord_core::VmSource::LocalMedia {
                path: "C:\\images\\ubuntu.iso".into(),
            },
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        }
    }
```

`fn repository()` в этих же тестах передаёт замыкание-заглушку, которое после
Task 5 принимает четыре аргумента: `Box::new(|_, _, _, _| Err(...))`.

- [ ] **Step 2: Прогнать и убедиться, что не компилируется**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform repository::`
Expected: FAIL — `no field builds on type HcsVmRepository`.

- [ ] **Step 3: Перестроить поля репозитория**

```rust
pub struct HcsVmRepository {
    client: HcsClient,
    store: MetadataStore,
    storage_root: PathBuf,
    connections: VmConnections,
    events: VmEventSink,
    /// Shared with every build thread, which is why it is behind an `Arc`.
    creation: Arc<VmCreationPipeline>,
    /// The VMs being created right now.
    builds: Arc<BuildRegistry>,
    start: VmStartPipeline,
    shutdown: VmShutdownPipeline,
    force_stop: VmForceStopPipeline,
    delete: VmDeletionPipeline,
    // `list_vms` takes `&self` but still has findings worth surfacing, and a
    // build thread reports its failure the same way, so the diagnostics buffer
    // is both shared and interior-mutable.
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
    initialized: bool,
    service_disconnect_reported: bool,
}
```

`new` — `creation: Arc::new(VmCreationPipeline::production(cloud_disk))`,
`builds: Arc::new(BuildRegistry::default())`,
`diagnostics: Arc::new(Mutex::new(Vec::new()))`.

`push_diagnostic` не меняется. Добавить свободную функцию, которой пользуется
поток:

```rust
/// Records a diagnostic in a buffer shared with the build threads.
///
/// Free rather than a method because a build thread has the buffer and not the
/// repository: the repository is not `Send`, and does not need to be.
fn push_shared_diagnostic(
    diagnostics: &Mutex<Vec<Diagnostic>>,
    level: DiagnosticLevel,
    message: String,
) {
    diagnostics
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(Diagnostic { level, message });
}
```

- [ ] **Step 4: Перевести `create_vm` на поток**

```rust
    /// Accepts the creation of a VM and returns; the VM is built on a thread of
    /// its own and appears in the list as `Building` until it is done.
    ///
    /// Everything that can be refused cheaply and certainly is refused here,
    /// before the thread: an obvious mistake belongs in the return value of the
    /// call that made it, not in a diagnostic a second later.
    fn create_vm(&mut self, request: VmCreateRequest) -> Result<(), RepositoryError> {
        self.require_initialized()?;
        request.validate()?;

        if self.store.find_by_vm_name(&request.name)?.is_some() || self.builds.contains(&request.name)
        {
            let error = RepositoryError::new(format!("VM \"{}\" already exists", request.name));
            log::error!("{error}");
            return Err(error);
        }
        let vm_directory = layout::vm_directory(&self.storage_root, &request.name)?;
        if vm_directory.exists() {
            let error = RepositoryError::new(format!(
                "VM directory already exists: {}",
                vm_directory.display()
            ));
            log::error!("{error}");
            return Err(error);
        }

        let pipeline = Arc::clone(&self.creation);
        let store = self.store.clone();
        let diagnostics = Arc::clone(&self.diagnostics);
        let name = request.name.clone();
        self.builds.start(request.clone(), move |monitor| {
            match pipeline.create(&store, &request, &vm_directory, monitor) {
                Ok(mapping) => log::info!(
                    "VM \"{}\" ({}) finished building",
                    mapping.vm_name,
                    mapping.vm_id
                ),
                Err(error) => {
                    log::error!("creating VM \"{name}\" failed: {error}");
                    push_shared_diagnostic(
                        &diagnostics,
                        DiagnosticLevel::Error,
                        format!("Failed to create VM \"{name}\": {error}"),
                    );
                }
            }
        })
    }
```

`name` захватывается замыканием, а `request.name` уходит в реестр — поэтому
`request.clone()` в `start` и `name` отдельно. (`VmCreateRequest: Clone`.)

- [ ] **Step 5: Склеить список и отказать операциям над строящейся VM**

```rust
    fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError> {
        self.require_initialized()?;

        let mut summaries: Vec<VmSummary> = list_known_vms(&self.client, &self.store)?
            .into_iter()
            .map(|known| self.summary(known))
            .collect();
        // A build that failed rolled itself back and never reached the store,
        // so its row simply stops being here.
        summaries.extend(self.builds.summaries());
        Ok(summaries)
    }
```

В `update_vm`, `start_vm`, `stop_vm`, `force_stop_vm` и `delete_vm` — сразу
после `require_initialized()?`:

```rust
        self.builds.refuse_if_building(&request.name)?;
```

(в `start_vm`/`stop_vm`/`force_stop_vm` — `refuse_if_building(name)?`).

`open_ssh` в `HcsVmRepository` не переопределён — оставить как есть; строящаяся
VM отказывает через дефолт трейта, и отдельного отказа он не требует.

В `take_diagnostics`, первой строкой:

```rust
        // The `&mut self` call the application already makes on every refresh,
        // right after listing: the place a finished build can be joined.
        self.builds.reap();
```

- [ ] **Step 6: Добавить `Drop`**

```rust
/// Stops every build before the process leaves.
///
/// Without this, shutting VMLord down either kills a thread in the middle of
/// writing a VHDX -- leaving the directory it was told to remove -- or waits
/// forever on one that was never told to stop.
impl Drop for HcsVmRepository {
    fn drop(&mut self) {
        self.builds.cancel_all_and_join();
    }
}
```

- [ ] **Step 7: Научить существующие `#[ignore]`-тесты дожидаться сборки**

`crates/platform/tests/hyperv.rs:125` и `:1266` вызывают `create_vm` и тут же
пользуются готовой VM. Теперь `create_vm` возвращается раньше, чем VM
существует, поэтому оба вызова получают ожидание. Хелпер рядом с
`listed_summary`:

```rust
/// Waits for a VM's background creation to finish.
///
/// `create_vm` returns as soon as the build is accepted, so a test that acts
/// on the VM has to wait for it the way the UI does: by looking at the list.
fn wait_for_build(repository: &HcsVmRepository, vm_name: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    loop {
        let summaries = repository
            .list_vms()
            .map_err(|error| format!("listing should work: {error}"))?;
        match summaries.iter().find(|vm| vm.name == vm_name) {
            None => return Err(format!("the build of VM \"{vm_name}\" failed")),
            Some(vm) if !matches!(vm.state, VmState::Building { .. }) => return Ok(()),
            Some(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("the build of VM \"{vm_name}\" did not finish"));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
```

Вставить `wait_for_build(&repository, &vm_name).expect("the build should finish");`
сразу после каждого `create_vm`, а в тесте на строке 125 — по его имени VM.

- [ ] **Step 8: Прогнать тесты и собрать всё**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform` и
`cargo build --target=x86_64-pc-windows-gnu`
Expected: PASS; `#[ignore]`-тесты компилируются и пропускаются.

- [ ] **Step 9: Коммит**

```bash
git add crates/platform/src/repository.rs crates/platform/tests/hyperv.rs
git commit -m "TASK-64: Create VMs on a worker thread and list them while they build"
```

---

### Task 10: `cancel_create` в контракте и в приложении

**Files:**
- Modify: `crates/core/src/lib.rs:163-187` (`trait VmRepository`)
- Modify: `crates/platform/src/repository.rs` (`impl VmRepository`)
- Modify: `crates/app/src/lib.rs` (рядом с `connect_display`)
- Test: `crates/app/src/lib.rs` (`mod tests`)

**Interfaces:**
- Consumes: `BuildRegistry::cancel` из Task 8.
- Produces:
  - `VmRepository::cancel_create(&mut self, name: &str) -> Result<(), RepositoryError>`
    с отказом по умолчанию;
  - `WorkspaceApp::cancel_create(&mut self, name: &str) -> Result<(), RepositoryError>`.

- [ ] **Step 1: Написать падающий тест**

В `mod tests` в `crates/app/src/lib.rs`:

```rust
    /// The button that calls this arrives with #65; the contract arrives here,
    /// so that adding the button is adding a button.
    #[test]
    fn cancelling_a_creation_a_backend_cannot_cancel_is_reported() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            vm_is_running: false,
            actions: Vec::new(),
        }));
        app.start();

        let error = app
            .cancel_create("dev")
            .expect_err("the fake backend inherits the trait's refusal");

        assert!(!error.to_string().is_empty());
        assert!(
            app.diagnostics().iter().any(|diagnostic| {
                diagnostic.level == DiagnosticLevel::Error && diagnostic.message.contains("dev")
            }),
            "the user has to be told the cancellation did not happen"
        );
    }
```

`FakeRepository` не переопределяет `cancel_create` — она наследует отказ по
умолчанию, и именно это здесь и проверяется.

- [ ] **Step 2: Прогнать и убедиться, что не компилируется**

Run: `cargo test -p vmlord-app`
Expected: FAIL — `no method named cancel_create`.

- [ ] **Step 3: Добавить метод в трейт**

В `crates/core/src/lib.rs`, между `delete_vm` и `open_display`:

```rust
    /// Stops a VM that is still being created, undoing what has been built.
    ///
    /// Defaulted rather than required: a backend that creates VMs
    /// synchronously has nothing in flight to cancel, and saying so is the
    /// honest answer. Deletion is deliberately not made to double as this --
    /// removing a VM that does not exist yet is a different operation with a
    /// different outcome.
    fn cancel_create(&mut self, _name: &str) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(
            "this backend creates VMs in the foreground, so there is nothing to cancel",
        ))
    }
```

- [ ] **Step 4: Реализовать в платформе и в приложении**

`crates/platform/src/repository.rs`, в `impl VmRepository`:

```rust
    fn cancel_create(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;
        self.builds.cancel(name)
    }
```

`crates/app/src/lib.rs`, рядом с `connect_display`:

```rust
    /// Asks the backend to stop creating a VM.
    ///
    /// The build rolls itself back and leaves the list on its own, so there is
    /// nothing to refresh here: the next refresh is a second away and will
    /// find whatever the build made of the request.
    pub fn cancel_create(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_ready_backend("cancelling VM creation")?;

        match self.repository.cancel_create(name) {
            Ok(()) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Info,
                    message: format!("Cancelling the creation of VM \"{name}\""),
                });
                Ok(())
            }
            Err(error) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: format!("Failed to cancel the creation of VM \"{name}\": {error}"),
                });
                self.collect_diagnostics();
                Err(error)
            }
        }
    }
```

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test --target=x86_64-pc-windows-gnu`
Expected: PASS во всём workspace.

- [ ] **Step 6: Коммит**

```bash
git add crates/core/src/lib.rs crates/platform/src/repository.rs crates/app/src/lib.rs
git commit -m "TASK-64: Add cancelling a VM creation to the repository contract"
```

---

### Task 11: проверка на живом хосте и документация

**Files:**
- Modify: `crates/platform/tests/hyperv.rs`
- Modify: `ARCHITECTURE.md`
- Test: `crates/platform/tests/hyperv.rs`

**Interfaces:**
- Consumes: всё предыдущее.
- Produces: два `#[ignore]`-теста и раздел в `ARCHITECTURE.md` про фоновое
  создание.

- [ ] **Step 1: Написать `#[ignore]`-тесты**

Сначала два хелпера — рядом с `no_cloud_images` (строка 43). Импортёр здесь
собран из тех же двух вызовов, что делает композиционный корень, чтобы
проверялось то, что поставляется:

```rust
/// A repository whose importer is the one the composition root builds.
fn cloud_repository(root: &std::path::Path) -> HcsVmRepository {
    let cache = root.join("cache");
    HcsVmRepository::new(
        root,
        Box::new(move |image: &vmlord_core::CloudImage, size, target: &std::path::Path,
                       monitor: &vmlord_core::BuildMonitor| {
            monitor.report(vmlord_core::BuildStep::Downloading);
            let mut source = vmlord_image::open_cloud_image(
                &image.profile,
                &image.release,
                &cache,
                size,
                monitor.downloads(),
                monitor.cancel_flag(),
            )?;
            monitor.report(vmlord_core::BuildStep::WritingDisk);
            vmlord_platform::import_image(&mut source, target, size, monitor.cancel_flag())
                .map(|_| ())
        }),
    )
}

fn background_cloud_request(name: &str) -> VmCreateRequest {
    VmCreateRequest {
        name: name.to_owned(),
        source: VmSource::CloudImage {
            image: vmlord_core::CloudImage {
                profile: vmlord_core::ubuntu(),
                release: "24.04".into(),
            },
            provisioning: vmlord_core::Provisioning {
                username: "dev".into(),
                password: None,
                ssh: vmlord_core::SshAccess::Enabled { deploy_key: true },
                locale: "en_US.UTF-8".into(),
                keyboard: "us".into(),
                timezone: "Europe/Moscow".into(),
            },
        },
        ram_mb: 2048,
        disk_gb: 16,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
    }
}
```

И сами тесты:

```rust
/// Creating a VM from a real cloud image must return at once and finish on its
/// own thread: this is the whole point of the task, and it cannot be observed
/// anywhere but on a host with HCS.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and downloads a cloud image"]
fn a_cloud_vm_is_built_in_the_background() {
    let root = std::env::temp_dir().join(format!("vmlord-bg-build-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let mut repository = cloud_repository(&root);
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");

    let accepted_at = std::time::Instant::now();
    repository
        .create_vm(background_cloud_request("bg-build"))
        .expect("the creation should be accepted");
    let accepted_in = accepted_at.elapsed();

    let mut seen_building = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(20 * 60);
    let outcome = loop {
        let summaries = repository.list_vms().expect("listing should work");
        match summaries.iter().find(|vm| vm.name == "bg-build") {
            Some(vm) if matches!(vm.state, VmState::Building { .. }) => seen_building = true,
            Some(_) => break Ok(()),
            None => break Err(format!("the build failed: {:?}", repository.take_diagnostics())),
        }
        if std::time::Instant::now() >= deadline {
            break Err("the build did not finish".to_owned());
        }
        std::thread::sleep(Duration::from_secs(2));
    };

    // Best-effort cleanup regardless of the assertions below.
    let _ = repository.delete_vm(VmDeleteRequest {
        name: "bg-build".into(),
        delete_disks: true,
    });
    drop(repository);
    let _ = fs::remove_dir_all(&root);

    outcome.expect("the VM should finish building");
    assert!(
        accepted_in < Duration::from_secs(2),
        "create_vm must not wait for the build, took {accepted_in:?}"
    );
    assert!(
        seen_building,
        "the VM must be listed as Building while it builds"
    );
}

/// A cancelled build leaves nothing: not the directory, not a metadata entry,
/// not a row in the list.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and downloads a cloud image"]
fn a_cancelled_build_leaves_nothing_behind() {
    let root = std::env::temp_dir().join(format!("vmlord-bg-cancel-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let mut repository = cloud_repository(&root);
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");

    repository
        .create_vm(background_cloud_request("bg-cancel"))
        .expect("the creation should be accepted");
    std::thread::sleep(Duration::from_secs(3));
    repository
        .cancel_create("bg-cancel")
        .expect("a build in flight is cancellable");

    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    let outcome = loop {
        let listed = repository
            .list_vms()
            .expect("listing should work")
            .iter()
            .any(|vm| vm.name == "bg-cancel");
        if !listed {
            break Ok(repository.take_diagnostics());
        }
        if std::time::Instant::now() >= deadline {
            break Err("the cancelled build did not go away".to_owned());
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let left_behind = root.join("bg-cancel").exists();
    drop(repository);
    let _ = fs::remove_dir_all(&root);

    let diagnostics = outcome.expect("the cancelled build should disappear from the list");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("bg-cancel")),
        "the user has to be told why the VM went away: {diagnostics:?}"
    );
    assert!(!left_behind, "the cancelled build must remove its directory");
}
```

`vmlord-image` уже в dev-зависимостях `vmlord-platform`, так что новых
зависимостей это не требует.

- [ ] **Step 2: Убедиться, что тесты компилируются и пропускаются**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform --test hyperv`
Expected: PASS с `ignored` на новых тестах.

- [ ] **Step 3: Обновить `ARCHITECTURE.md`**

Добавить в раздел о слое `platform` абзац о том, что создание VM асинхронно по
отношению к вызывающему: `create_vm` заводит запись в реестре сборок и
возвращается, поток исполняет `VmCreationPipeline`, `list_vms` склеивает
`MetadataStore` с реестром, отмена — флаг в `BuildMonitor`, а `Drop`
репозитория отменяет и джойнит всё. Явно записать, что `VmRepository` при этом
остаётся синхронным и async-рантайма в проекте по-прежнему нет.

- [ ] **Step 4: Прогнать всё**

Run: `cargo test --target=x86_64-pc-windows-gnu` и
`cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
Expected: PASS без предупреждений clippy.

- [ ] **Step 5: Коммит**

```bash
git add crates/platform/tests/hyperv.rs ARCHITECTURE.md
git commit -m "TASK-64: Cover background creation on a live host and document it"
```

- [ ] **Step 6: Ручная проверка на Hyper-V — за владельцем проекта**

Сценарий: создать VM из Ubuntu cloud image → окно не замирает, строка VM
показывает стадии и байты скачивания → VM появляется как обычная → создать
вторую и отменить её на скачивании → каталог не остался, в диагностике есть
сообщение с именем VM → закрыть VMLord во время сборки → процесс завершается, и
после перезапуска ни каталога, ни записи в метаданных нет.
