# TASK-44: снятие запрета NetworkMode::Nat — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Открыть границу домена для `NetworkMode::Nat`, чтобы VM можно было создать и отредактировать с NAT, а `VmStartPipeline` (#38) наконец получил достижимую ветку с endpoint'ом.

**Architecture:** Одна общая проверка режима в `hcs_config` обслуживает обе точки входа (`HcsVmConfigBuilder::build` для создания и `HcsVmRepository::update_vm` для правки). Режим живёт в `VmComputeSystemMapping`, `config.json` — производная: старт вписывает секцию `NetworkAdapters` при `Nat` и сносит её при любом другом режиме. `VmSummary` начинает показывать режим из мэппинга, иначе правка RAM в UI молча сбросила бы сеть.

**Tech Stack:** Rust 2024, `serde_json`, `windows` crate, `log`. Крейты: `vmlord-platform`, `vmlord-ui`, `vmlord-app`.

**Spec:** `docs/superpowers/specs/2026-08-08-hns-allow-nat-design.md`

## Global Constraints

* Ветка `task-44-hns-allow-nat` (уже создана, ответвлена от `task-38-hns-start-endpoint`). Merge request — только по явному разрешению владельца.
* Каждый коммит: `TASK-44: <comment>`, автор — агент:
  `GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local git commit -m "..."`
* Сборка только под Windows-таргет: `cargo build --target=x86_64-pc-windows-gnu`.
* Unit-тесты запускаются прямо здесь: `cargo test --target x86_64-pc-windows-gnu` — WSL исполняет Windows-бинарники через interop. `#[ignore]`-тесты в `crates/platform/tests/hyperv.rs` требуют настоящего Hyper-V и остаются за владельцем.
* Clippy без предупреждений: `cargo clippy --target=x86_64-pc-windows-gnu --all-targets`.
* `External`, `Internal` и `Unknown(_)` остаются отклонёнными, сообщение называет #10.
* Endpoint'ы в HNS этой задачей не удаляются никогда — очистка это #42.
* Комментарии и сообщения в коде — на английском, как во всём репозитории.

## File Structure

| Файл | Роль в задаче |
| --- | --- |
| `crates/platform/src/hcs_config.rs` | Общая проверка режима `ensure_supported_network_mode`; новая `remove_network_adapter`; `build` принимает `Nat` |
| `crates/platform/src/start.rs` | Ветка «не Nat» сносит устаревшую секцию `NetworkAdapters` |
| `crates/platform/src/repository.rs` | `update_vm` принимает `Nat` и записывает режим в мэппинг; `summary()` показывает режим из мэппинга |
| `crates/ui/src/lib.rs` | Комбобокс сети в форме редактирования: только NAT и None |
| `crates/platform/tests/hyperv.rs` | E2E-тест создаёт NAT VM напрямую, без обхода запрета |
| `crates/app/tests/update_vm.rs` | Фикстура перестаёт использовать отклоняемый `Internal` |
| `ARCHITECTURE.md` | Контракт: `Nat` разрешён, `External`/`Internal` ждут #10 |

---

### Task 1: Общая проверка режима на границе домена

**Files:**
- Modify: `crates/platform/src/hcs_config.rs:24-41` (проверка в `build`), тесты в том же файле
- Modify: `crates/platform/src/repository.rs:354-370` (`update_vm`)

**Interfaces:**
- Produces: `pub(crate) fn ensure_supported_network_mode(mode: NetworkMode) -> Result<(), RepositoryError>` в `crates/platform/src/hcs_config.rs`

- [ ] **Step 1: Заменить тест на отклонение всех режимов, кроме None**

В `crates/platform/src/hcs_config.rs`, в `mod tests`, заменить тест `rejects_each_unsupported_network_mode` целиком на два теста и добавить `ensure_supported_network_mode` в `use super::{...}`:

```rust
    #[test]
    fn accepts_nat_without_writing_a_network_adapter() {
        // Creation writes no adapter: the endpoint and its MAC only exist once
        // `VmStartPipeline` has run, so the section is the start's to write.
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let request = VmCreateRequest {
            network_mode: NetworkMode::Nat,
            ..request()
        };

        let document = HcsVmConfigBuilder::build(&request, &system_disk_path).unwrap();

        let json: Value = serde_json::from_str(&document).unwrap();
        assert!(
            json.pointer("/VirtualMachine/Devices/NetworkAdapters")
                .is_none()
        );
    }

    #[test]
    fn rejects_each_network_mode_that_waits_for_its_own_task() {
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        for mode in [
            NetworkMode::External,
            NetworkMode::Internal,
            NetworkMode::Unknown(7),
        ] {
            let request = VmCreateRequest {
                network_mode: mode,
                ..request()
            };

            let message = HcsVmConfigBuilder::build(&request, &system_disk_path)
                .unwrap_err()
                .to_string();

            assert!(message.contains("network mode"), "got: {message}");
            assert!(message.contains("#10"), "got: {message}");
        }
    }

    #[test]
    fn ensure_supported_network_mode_accepts_none_and_nat() {
        assert!(ensure_supported_network_mode(NetworkMode::None).is_ok());
        assert!(ensure_supported_network_mode(NetworkMode::Nat).is_ok());
    }
```

- [ ] **Step 2: Убедиться, что тесты не собираются**

Run: `cargo test --no-run --target x86_64-pc-windows-gnu -p vmlord-platform`
Expected: FAIL — `cannot find function ensure_supported_network_mode in module super`.

- [ ] **Step 3: Добавить функцию и подключить её в `build`**

В `crates/platform/src/hcs_config.rs`, сразу после блока `impl HcsVmConfigBuilder` (перед `pub(crate) struct VmTopology`):

```rust
/// Checks the network mode against what the native backend implements today.
///
/// Both entry points into the domain -- creation through
/// [`HcsVmConfigBuilder::build`] and editing through `HcsVmRepository::update_vm`
/// -- ask this, so a mode is refused in one place and with one message. The
/// message names the task that will lift the refusal: an HRESULT from HNS,
/// raised much deeper, tells the user nothing about why the mode is missing.
pub(crate) fn ensure_supported_network_mode(mode: NetworkMode) -> Result<(), RepositoryError> {
    match mode {
        NetworkMode::None | NetworkMode::Nat => Ok(()),
        other => {
            let error = RepositoryError::new(format!(
                "the HCS backend does not support network mode {other:?} yet; \
                 External and Internal networking arrive with #10"
            ));
            log::error!("{error}");
            Err(error)
        }
    }
}
```

В `build` заменить блок `if request.network_mode != NetworkMode::None { ... }` (строки 36-41) на:

```rust
        ensure_supported_network_mode(request.network_mode)?;
```

и поправить доккомментарий `build` (строки 22-23), заменив

```rust
    /// GPU and network configuration are not yet implemented (deferred to
    /// their own tasks); any mode other than `None` is rejected.
```

на

```rust
    /// GPU configuration is not yet implemented; any mode other than `None` is
    /// rejected. Networking accepts `None` and `NetworkMode::Nat`; a NAT VM
    /// gets no adapter here, because `VmStartPipeline` writes the
    /// `NetworkAdapters` section once its endpoint exists.
```

- [ ] **Step 4: Проверить, что тесты собираются**

Run: `cargo test --no-run --target x86_64-pc-windows-gnu -p vmlord-platform`
Expected: собирается без ошибок. Если `NetworkMode` остался неиспользованным импортом в `hcs_config.rs` — он всё ещё нужен для `ensure_supported_network_mode`, предупреждения быть не должно.

- [ ] **Step 5: Подключить проверку в `update_vm`**

В `crates/platform/src/repository.rs`, в `update_vm`, заменить блок (строки 364-369)

```rust
        if request.network_mode != NetworkMode::None {
            return Err(RepositoryError::new(format!(
                "the HCS backend does not support network mode {:?} yet",
                request.network_mode
            )));
        }
```

на

```rust
        hcs_config::ensure_supported_network_mode(request.network_mode)?;
```

Импорт `NetworkMode` в начале `repository.rs` остаётся нужным: его использует `summary` (строка 201).

- [ ] **Step 6: Собрать и проверить clippy**

Run: `cargo build --target=x86_64-pc-windows-gnu && cargo test --no-run --target x86_64-pc-windows-gnu`
Expected: успешно.

Run: `cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
Expected: без предупреждений.

- [ ] **Step 7: Коммит**

```bash
git add crates/platform/src/hcs_config.rs crates/platform/src/repository.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-44: Accept NetworkMode::Nat at the domain boundary"
```

---

### Task 2: Снятие устаревшей секции NetworkAdapters при старте

**Files:**
- Modify: `crates/platform/src/hcs_config.rs` (добавить `remove_network_adapter` рядом с `apply_network_adapter`, тесты в том же файле)
- Modify: `crates/platform/src/start.rs:130-170` (ветка «не Nat» в `attach_network`), тесты в том же файле

**Interfaces:**
- Consumes: `DEVICES_POINTER`, `NETWORK_ADAPTERS_KEY`, `parse` из `hcs_config` (уже есть)
- Produces: `pub(crate) fn remove_network_adapter(document: &str) -> Result<String, RepositoryError>`

- [ ] **Step 1: Написать падающие тесты на `remove_network_adapter`**

В `crates/platform/src/hcs_config.rs`, в `mod tests`, добавить `remove_network_adapter` в `use super::{...}` и добавить тесты:

```rust
    #[test]
    fn removes_the_network_adapter_section_and_nothing_else() {
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let created = HcsVmConfigBuilder::build(&request(), &system_disk_path).unwrap();
        let attached = apply_network_adapter(
            &created,
            Uuid::from_u128(0x3f2b_0c11_5c78_4c1b_9e2f_3a8b_7d4c_6e50),
            "00-15-5D-01-02-03",
        )
        .unwrap();

        let removed = remove_network_adapter(&attached).unwrap();

        let before: Value = serde_json::from_str(&created).unwrap();
        let after: Value = serde_json::from_str(&removed).unwrap();
        assert!(
            after
                .pointer("/VirtualMachine/Devices/NetworkAdapters")
                .is_none()
        );
        assert_eq!(after, before);
    }

    #[test]
    fn removing_an_absent_network_adapter_returns_the_document_unchanged() {
        // Byte-identical, not merely equivalent: `VmStartPipeline` decides
        // whether to rewrite `config.json` by comparing the two strings.
        let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
        let created = HcsVmConfigBuilder::build(&request(), &system_disk_path).unwrap();

        let removed = remove_network_adapter(&created).unwrap();

        assert_eq!(removed, created);
    }

    #[test]
    fn removing_a_network_adapter_from_a_document_without_devices_changes_nothing() {
        let document = json!({ "VirtualMachine": {} }).to_string();

        let removed = remove_network_adapter(&document).unwrap();

        assert_eq!(removed, document);
    }

    #[test]
    fn removing_a_network_adapter_rejects_invalid_json() {
        let error = remove_network_adapter("not json").unwrap_err().to_string();

        assert!(error.contains("not valid JSON"), "got: {error}");
    }
```

- [ ] **Step 2: Убедиться, что тесты не собираются**

Run: `cargo test --no-run --target x86_64-pc-windows-gnu -p vmlord-platform`
Expected: FAIL — `cannot find function remove_network_adapter in module super`.

- [ ] **Step 3: Реализовать `remove_network_adapter`**

В `crates/platform/src/hcs_config.rs`, сразу после `apply_network_adapter`:

```rust
/// Returns `document` without its `NetworkAdapters` section.
///
/// This is what a VM that no longer asks for a network needs: the stored
/// document describes the adapter a previous start gave it, and leaving the
/// section in place would bring the VM up on the network it just gave up.
///
/// A document that has no such section -- or no `Devices` object to hold one --
/// is returned byte for byte, so a start that changes nothing writes nothing.
pub(crate) fn remove_network_adapter(document: &str) -> Result<String, RepositoryError> {
    let mut configuration = parse(document)?;
    let removed = configuration
        .pointer_mut(DEVICES_POINTER)
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|devices| devices.remove(NETWORK_ADAPTERS_KEY));
    if removed.is_none() {
        return Ok(document.to_owned());
    }

    serde_json::to_string(&configuration).map_err(|error| {
        RepositoryError::new(format!(
            "failed to serialize the HCS VM configuration without its network adapter: {error}"
        ))
    })
}
```

- [ ] **Step 4: Проверить сборку тестов**

Run: `cargo test --no-run --target x86_64-pc-windows-gnu -p vmlord-platform`
Expected: собирается без ошибок.

- [ ] **Step 5: Написать падающий тест на старт VM с устаревшей секцией**

В `crates/platform/src/start.rs`, в `mod tests`, добавить после `a_vm_without_networking_never_asks_for_an_endpoint`:

```rust
    #[test]
    fn a_vm_switched_off_the_network_loses_the_adapter_it_used_to_have() {
        // The VM ran with NAT, then its mode was edited back to `None`: the
        // section a previous start wrote must not survive into this one.
        let fixture = fixture("network-removed");
        let calls = fixture.calls.clone();
        let stale = crate::hcs_config::apply_network_adapter(
            &fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap(),
            NEW_ENDPOINT_ID,
            MAC_ADDRESS,
        )
        .unwrap();
        fs::write(fixture.vm_directory.join("config.json"), &stale).unwrap();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        assert!(calls.endpoint.lock().unwrap().is_empty());
        assert!(
            fixture
                .configuration()
                .pointer("/VirtualMachine/Devices/NetworkAdapters")
                .is_none(),
            "the stale adapter must be gone from the stored configuration"
        );
        let started = calls.start.lock().unwrap().clone();
        assert!(
            !started[0].1.contains("NetworkAdapters"),
            "the starter must be handed the document without the adapter"
        );
    }
```

- [ ] **Step 6: Убедиться, что тест падает по существу**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform a_vm_switched_off_the_network`
Expected: FAIL — ветка «не Nat» возвращает документ как есть, устаревшая секция остаётся на месте.

- [ ] **Step 7: Снести секцию в ветке «не Nat»**

В `crates/platform/src/start.rs`, в `attach_network`, заменить блок (строки 137-144)

```rust
        if mapping.network_mode != NetworkMode::Nat {
            log::debug!(
                "VM \"{}\" asks for {:?} networking; starting it without an endpoint",
                mapping.vm_name,
                mapping.network_mode
            );
            return Ok(configuration);
        }
```

на

```rust
        if mapping.network_mode != NetworkMode::Nat {
            log::debug!(
                "VM \"{}\" asks for {:?} networking; starting it without an endpoint",
                mapping.vm_name,
                mapping.network_mode
            );
            // A VM edited off the network still has the adapter an earlier
            // start wrote; without this it would come up on the network it was
            // just taken off. The endpoint itself stays in HNS until the VM is
            // deleted, so switching back to NAT keeps the guest's address.
            let updated = hcs_config::remove_network_adapter(&configuration)?;
            if updated != configuration {
                self.write_configuration(mapping, vm_directory, &updated)?;
                log::info!(
                    "VM \"{}\" ({}) no longer asks for a network; its adapter was removed \
                     from the stored configuration",
                    mapping.vm_name,
                    mapping.vm_id
                );
            }
            return Ok(updated);
        }
```

Обновить доккомментарий `attach_network` (строка 124), заменив

```rust
    /// A VM that asked for no network is left exactly as it was, HNS untouched.
```

на

```rust
    /// A VM that asked for no network is left off HNS entirely; the adapter an
    /// earlier start may have written is removed from its configuration.
```

- [ ] **Step 8: Собрать, проверить clippy**

Run: `cargo build --target=x86_64-pc-windows-gnu && cargo test --no-run --target x86_64-pc-windows-gnu && cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
Expected: успешно, без предупреждений.

- [ ] **Step 9: Коммит**

```bash
git add crates/platform/src/hcs_config.rs crates/platform/src/start.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-44: Drop the network adapter when a VM gives up its network"
```

---

### Task 3: Запись режима в мэппинг и отчёт о нём

**Files:**
- Modify: `crates/platform/src/repository.rs` (`update_vm`, `summary`, новая свободная функция `record_network_mode`, тесты в том же файле)

**Interfaces:**
- Consumes: `hcs_config::ensure_supported_network_mode` (Task 1); `MetadataStore::insert`, `MetadataStore::find_by_vm_name`, `VmComputeSystemMapping { vm_id, vm_name, hcs_compute_system_id, disk_gb, endpoint_id, network_mode }`
- Produces: `fn record_network_mode(store: &MetadataStore, mapping: &VmComputeSystemMapping, network_mode: NetworkMode) -> Result<(), RepositoryError>` (свободная функция уровня модуля в `repository.rs`)

- [ ] **Step 1: Написать падающие тесты**

В `crates/platform/src/repository.rs`, в `mod tests`, дополнить импорты и добавить тесты. Импорты в начале `mod tests` должны стать такими:

```rust
    use std::fs;

    use uuid::Uuid;
    use vmlord_core::{
        DiagnosticLevel, GpuMode, NetworkMode, RepositoryError, VmDeleteRequest, VmRepository,
        VmState, VmUpdateRequest,
    };

    use super::{HcsVmRepository, record_network_mode};
    use crate::{
        KnownVm, MetadataStore, VmComputeSystemMapping,
        watch::{HcsEventKind, HcsVmEvent},
    };
```

и добавить тесты:

```rust
    /// A store under a directory of this test's own, removed by the test that
    /// created it. The repository tests never share one.
    fn temp_store(label: &str) -> (std::path::PathBuf, MetadataStore) {
        let root = std::env::temp_dir().join(format!(
            "vmlord-repository-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("test root should be created");
        let store = MetadataStore::new(root.join("vm-mapping.json"));
        (root, store)
    }

    fn mapping(network_mode: NetworkMode) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode,
        }
    }

    #[test]
    fn a_changed_network_mode_is_recorded_in_the_mapping() {
        let (root, store) = temp_store("mode-changed");
        let mapping = mapping(NetworkMode::None);
        store.insert(mapping.clone()).unwrap();

        record_network_mode(&store, &mapping, NetworkMode::Nat).unwrap();

        let stored = store.find_by_vm_name("dev").unwrap().unwrap();
        assert_eq!(stored.network_mode, NetworkMode::Nat);
        // Nothing else about the VM may move with its network mode.
        assert_eq!(stored.vm_id, mapping.vm_id);
        assert_eq!(stored.disk_gb, mapping.disk_gb);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_unchanged_network_mode_leaves_the_mapping_alone() {
        let (root, store) = temp_store("mode-unchanged");
        let mapping = mapping(NetworkMode::Nat);
        store.insert(mapping.clone()).unwrap();

        record_network_mode(&store, &mapping, NetworkMode::Nat).unwrap();

        assert_eq!(store.find_by_vm_name("dev").unwrap().unwrap(), mapping);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_summary_reports_the_network_mode_the_mapping_records() {
        // The edit form is filled from `VmSummary`, so a summary that always
        // said `None` would make an unrelated edit switch NAT off.
        let repository = repository();

        let summary = repository.summary(KnownVm {
            mapping: mapping(NetworkMode::Nat),
            state: None,
        });

        assert_eq!(summary.network_mode, NetworkMode::Nat);
        assert_eq!(summary.state, VmState::Stopped);
    }
```

- [ ] **Step 2: Убедиться, что тесты не собираются**

Run: `cargo test --no-run --target x86_64-pc-windows-gnu -p vmlord-platform`
Expected: FAIL — `cannot find function record_network_mode in module super`.

- [ ] **Step 3: Добавить `record_network_mode`**

В `crates/platform/src/repository.rs`, рядом со свободной функцией `vm_state` (после неё):

```rust
/// Records a VM's network mode in its mapping, if it changed.
///
/// The mapping, not `config.json`, is where the mode lives: the document
/// describes the adapter a VM already has, while the mode is what the next
/// start reads to decide whether it should have one at all.
fn record_network_mode(
    store: &MetadataStore,
    mapping: &VmComputeSystemMapping,
    network_mode: NetworkMode,
) -> Result<(), RepositoryError> {
    if mapping.network_mode == network_mode {
        return Ok(());
    }

    store.insert(VmComputeSystemMapping {
        network_mode,
        ..mapping.clone()
    })?;
    log::info!(
        "VM \"{}\" ({}) now asks for {:?} networking; the change applies the next time it starts",
        mapping.vm_name,
        mapping.vm_id,
        network_mode
    );
    Ok(())
}
```

- [ ] **Step 4: Вызвать её из `update_vm`**

В `crates/platform/src/repository.rs`, в `update_vm`, после блока `fs::write(&path, updated)...?;` и перед завершающим `log::info!`, вставить:

```rust
        record_network_mode(&self.store, &mapping, request.network_mode)?;
```

- [ ] **Step 5: Показывать режим в `summary`**

В `crates/platform/src/repository.rs`, в `summary`, заменить

```rust
            // GPU, networking and SSH are not wired to the native backend yet
            // and are reported as absent rather than guessed at.
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
            ip_address: None,
            ssh_port: None,
```

на

```rust
            // GPU, the guest's address and SSH are not wired to the native
            // backend yet and are reported as absent rather than guessed at.
            gpu_mode: GpuMode::None,
            network_mode,
            ip_address: None,
            ssh_port: None,
```

и добавить в начале `summary`, сразу после `let KnownVm { mapping, state } = known;`:

```rust
        // Read before `mapping.vm_name` is moved into the summary below.
        let network_mode = mapping.network_mode;
```

- [ ] **Step 6: Проверить сборку и clippy**

Run: `cargo build --target=x86_64-pc-windows-gnu && cargo test --no-run --target x86_64-pc-windows-gnu && cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
Expected: успешно, без предупреждений.

- [ ] **Step 7: Коммит**

```bash
git add crates/platform/src/repository.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-44: Record and report a VM's network mode"
```

---

### Task 4: UI — форма редактирования предлагает только реальные режимы

**Files:**
- Modify: `crates/ui/src/lib.rs:719-728` (комбобокс сети в форме редактирования)
- Modify: `crates/ui/src/lib.rs:1403-1417` (тест `edit_vm_request_accepts_supported_modes`)

**Interfaces:**
- Consumes: `edit_vm_request(&EditVmForm) -> Result<VmUpdateRequest, String>` (без изменений сигнатуры)

- [ ] **Step 1: Перенацелить тест с External на Nat**

В `crates/ui/src/lib.rs`, в `mod tests`, в `edit_vm_request_accepts_supported_modes` заменить оба упоминания `NetworkMode::External` на `NetworkMode::Nat`:

```rust
    #[test]
    fn edit_vm_request_accepts_supported_modes() {
        let request = edit_vm_request(&EditVmForm {
            name: "dev".into(),
            ram_mb: 8192,
            cpu_cores: 8,
            gpu_mode: GpuMode::TryAll,
            network_mode: NetworkMode::Nat,
            error: None,
        })
        .unwrap();

        assert_eq!(request.name, "dev");
        assert_eq!(request.gpu_mode, GpuMode::TryAll);
        assert_eq!(request.network_mode, NetworkMode::Nat);
    }
```

- [ ] **Step 2: Убрать External и Internal из комбобокса**

В `crates/ui/src/lib.rs`, в форме редактирования, заменить

```rust
                            ui.selectable_value(&mut form.network_mode, NetworkMode::Nat, "NAT");
                            ui.selectable_value(&mut form.network_mode, NetworkMode::None, "None");
                            ui.selectable_value(&mut form.network_mode, NetworkMode::External, "External");
                            ui.selectable_value(&mut form.network_mode, NetworkMode::Internal, "Internal");
```

на

```rust
                            // The same two modes the create form offers: the
                            // native backend refuses the rest until #10, and an
                            // option that always fails is a poor way to say so.
                            ui.selectable_value(&mut form.network_mode, NetworkMode::Nat, "NAT");
                            ui.selectable_value(&mut form.network_mode, NetworkMode::None, "None");
```

`network_mode_label` не трогать: он всё ещё показывает режим, прочитанный у старой VM.

- [ ] **Step 3: Собрать и проверить**

Run: `cargo build --target=x86_64-pc-windows-gnu && cargo test --no-run --target x86_64-pc-windows-gnu -p vmlord-ui && cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
Expected: успешно, без предупреждений.

- [ ] **Step 4: Коммит**

```bash
git add crates/ui/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-44: Offer only the network modes the backend implements"
```

---

### Task 5: Убрать обходы запрета из тестов и обновить ARCHITECTURE.md

**Files:**
- Modify: `crates/platform/tests/hyperv.rs:944-960` (обход запрета в `starts_a_nat_vm_on_its_endpoint`)
- Modify: `crates/app/tests/update_vm.rs:69-76` (`update_request` использует `Internal`)
- Modify: `ARCHITECTURE.md` (строки ~352-355, ~411-413, ~462-464)

**Interfaces:**
- Consumes: `VmCreationPipeline::create(&MetadataStore, &VmCreateRequest, &Path) -> Result<VmComputeSystemMapping, RepositoryError>` — теперь принимает запрос с `NetworkMode::Nat`

- [ ] **Step 1: Создавать NAT VM напрямую в e2e-тесте**

В `crates/platform/tests/hyperv.rs`, в `starts_a_nat_vm_on_its_endpoint`, заменить блок

```rust
    // Creation still rejects every mode but `None` until TASK-44 lifts that,
    // so the NAT mode is written into the mapping directly here.
    let created_request = VmCreateRequest {
        network_mode: NetworkMode::None,
        ..request.clone()
    };
    let mapping = VmCreationPipeline::production()
        .create(&store, &created_request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    store
        .insert(VmComputeSystemMapping {
            network_mode: NetworkMode::Nat,
            ..mapping.clone()
        })
        .expect("the NAT mode should be recorded");
```

на

```rust
    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    assert_eq!(
        store
            .find_by_vm_name(&mapping.vm_name)
            .expect("the mapping should be readable")
            .expect("creation should register the VM")
            .network_mode,
        NetworkMode::Nat,
        "creation must record the NAT mode the request asked for"
    );
```

После этого `request.clone()` может стать ненужным — если компилятор сообщит, что `request` больше не клонируется, оставить как есть; лишний `clone` удалять только если он остался без пользователей. Если `VmComputeSystemMapping` перестал использоваться в файле, удалить его из `use vmlord_platform::{...}`.

- [ ] **Step 2: Перевести фикстуру app-теста на принимаемый режим**

В `crates/app/tests/update_vm.rs`, в `update_request`, заменить `network_mode: NetworkMode::Internal,` на `network_mode: NetworkMode::Nat,`.

- [ ] **Step 3: Обновить ARCHITECTURE.md**

Три правки.

1. В абзаце про то, что нативный бэкенд сообщает меньше, заменить

```
The native backend deliberately reports less than AppSandbox did while the
remaining migration tasks land: GPU mode, network mode, guest IP address and SSH
port are `None`, guest agent status is `Unknown`, and display and SSH
connections report that the backend does not support them.
```

на

```
The native backend deliberately reports less than AppSandbox did while the
remaining migration tasks land: GPU mode, guest IP address and SSH port are
`None`, guest agent status is `Unknown`, and display and SSH connections report
that the backend does not support them. Network mode is reported from the VM's
mapping, because the edit form is filled from `VmSummary`: a summary that always
said `None` would make an unrelated edit switch a NAT VM off the network.
```

2. В абзаце про правку VM заменить

```
applies after a restart rather than refusing it. GPU and network modes other
than `None` are rejected until their own tasks land.
```

на

```
applies after a restart rather than refusing it. An edit also carries the
network mode, which is recorded in the VM's mapping rather than in its
`config.json` and reaches the VM the same way: the next start writes or removes
the `NetworkAdapters` section to match. GPU modes other than `None` are
rejected until their own task lands, as are the `External` and `Internal`
network modes.
```

3. В списке «VM update contract» заменить

```
* GPU and network modes are rejected by the native backend until their own
  migration tasks land.
```

на

```
* GPU modes other than `None` are rejected by the native backend until their own
  migration task lands.
* Network mode accepts `None` and `Nat`; `External` and `Internal` are rejected
  with a message naming the task that will add them.
```

- [ ] **Step 4: Собрать всё и проверить**

Run: `cargo build --target=x86_64-pc-windows-gnu && cargo test --no-run --target x86_64-pc-windows-gnu && cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
Expected: успешно, без предупреждений.

- [ ] **Step 5: Коммит**

```bash
git add crates/platform/tests/hyperv.rs crates/app/tests/update_vm.rs ARCHITECTURE.md
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-44: Document the network mode contract and drop its test workarounds"
```

---

## Проверка перед сдачей

- [ ] `cargo build --target=x86_64-pc-windows-gnu` — успешно
- [ ] `cargo clippy --target=x86_64-pc-windows-gnu --all-targets` — без предупреждений
- [ ] `cargo test --target x86_64-pc-windows-gnu` — все тесты проходят
- [ ] Ручная parity-проверка на Hyper-V и `#[ignore]`-тесты — за владельцем
- [ ] Merge request не открывается без явного разрешения владельца
