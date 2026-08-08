# TASK-46: освобождение HNS endpoint при остановке VM — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Force Stop отцепляет сетевой адаптер до terminate, а старт, наткнувшийся на занятый endpoint, пересоздаёт его с тем же адресом — чтобы цикл start → Force Stop → start перестал падать с HRESULT 0x803B0014.

**Architecture:** Два независимых механизма. Первый — hot-detach через `HcsModifyComputeSystem` в `VmForceStopPipeline`: он покрывает остановку, которую командует сам VMLord. Второй — recovery в `VmStartPipeline`: старт различает отказ «endpoint занят» и один раз повторяет попытку, заменив endpoint новым с прочитанным у старого адресом. Второй нужен потому, что после краха гостя, самостоятельного shutdown или рестарта VMLord compute system уже уничтожена и отцеплять нечего.

**Tech Stack:** Rust 2024, `serde_json`, `windows` 0.61 (`Win32_System_HostComputeSystem`, `Win32_System_HostComputeNetwork`), `uuid`, `log`. Крейт: `vmlord-platform`.

**Spec:** `docs/superpowers/specs/2026-08-08-hns-release-endpoint-design.md`

## Global Constraints

* Ветка `task-46-hns-release-endpoint` (уже создана, ответвлена от `main`). Merge request — только по явному разрешению владельца.
* Каждый коммит: `TASK-46: <comment>`, автор — агент:
  `GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local git commit -m "..."`
* Сборка только под Windows-таргет: `cargo build --target=x86_64-pc-windows-gnu`.
* Unit-тесты запускаются прямо здесь: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform` — WSL исполняет Windows-бинарники через interop. `#[ignore]`-тесты в `crates/platform/tests/hyperv.rs` требуют настоящего Hyper-V и остаются за владельцем.
* Clippy без предупреждений: `cargo clippy --target=x86_64-pc-windows-gnu --all-targets`.
* Комментарии, сообщения об ошибках и логи — на английском, как во всём репозитории.
* Endpoint'ы в HNS удаляются только на пути recovery (`EndpointPolicy::Replace`). Очистка при delete/initialize — #42, её здесь не появляется.
* `VmShutdownPipeline` не меняется: graceful shutdown ничего не отцепляет (решение спеки).
* Неудавшийся detach не отменяет terminate и не превращается в отказ `force_stop` — только `WARN` в лог.
* Весь `unsafe` остаётся в `crates/platform/src/hcs.rs` и `crates/platform/src/hcn_endpoint.rs`, каждый блок с комментарием `// SAFETY:`.

## File Structure

| Файл | Роль в задаче |
| --- | --- |
| `crates/platform/src/hcs_config.rs` | Новая `adapter_key` — единственное место, где формируется ключ адаптера |
| `crates/platform/src/hcs.rs` | Обёртка `HcsSystem::remove_network_adapter`; классификация отказа `HcsStartFailure`; `start_and_wait`, `create_system_and_wait` |
| `crates/platform/src/hcn_endpoint.rs` | Константа `HCN_E_ENDPOINT_ALREADY_ATTACHED`; чтение адреса endpoint'а; создание endpoint'а с запрошенным адресом |
| `crates/platform/src/force_stop.rs` | Detach адаптера перед terminate |
| `crates/platform/src/start.rs` | `EndpointPolicy`, повтор старта при занятом endpoint'е, продакшн-реализация замены |
| `crates/platform/src/lib.rs` | Экспорт новых публичных типов |
| `crates/platform/tests/hyperv.rs` | `#[ignore]`-регрессия start → Force Stop → start |
| `ARCHITECTURE.md` | Жизненный цикл endpoint'а между остановкой и стартом |

---

### Task 1: Единый ключ адаптера и обёртка detach'а

**Files:**
- Modify: `crates/platform/src/hcs_config.rs:170-204` (`apply_network_adapter`), тесты в том же файле
- Modify: `crates/platform/src/hcs.rs:1-26` (импорты и константы), `crates/platform/src/hcs.rs:240-289` (методы `HcsSystem`), тесты в том же файле

**Interfaces:**
- Produces: `pub(crate) fn adapter_key(endpoint_id: Uuid) -> String` в `crates/platform/src/hcs_config.rs`
- Produces: `pub fn HcsSystem::remove_network_adapter(&self, endpoint_id: Uuid) -> Result<(), RepositoryError>`
- Produces: `fn detach_adapter_document(endpoint_id: Uuid) -> String` (приватная, в `hcs.rs`, покрыта тестами)

- [ ] **Step 1: Написать падающий тест на общий ключ адаптера**

В `crates/platform/src/hcs_config.rs`, в `mod tests`, добавить `adapter_key` в `use super::{...}` и добавить тест в конец модуля:

```rust
    #[test]
    fn the_adapter_key_is_how_the_section_names_the_adapter() {
        // A detach names the adapter by this key in its resource path. A
        // spelling that drifts from the one the section uses detaches nothing
        // and still reports success, so both sides read it from here.
        let document =
            HcsVmConfigBuilder::build(&request(), &PathBuf::from("C:\\vms\\a\\disks\\system.vhdx"))
                .unwrap();
        let updated: Value = serde_json::from_str(&with_adapter(&document)).unwrap();

        let key = adapter_key(ENDPOINT_ID);
        let adapters = updated
            .pointer("/VirtualMachine/Devices/NetworkAdapters")
            .and_then(Value::as_object)
            .unwrap();

        assert_eq!(key, ENDPOINT_GUID);
        assert_eq!(adapters.keys().collect::<Vec<_>>(), vec![&key]);
        assert_eq!(adapters[&key]["EndpointId"], key);
    }
```

- [ ] **Step 2: Убедиться, что тест не компилируется**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform hcs_config`
Expected: ошибка компиляции `cannot find function 'adapter_key' in module 'super'`.

- [ ] **Step 3: Выделить `adapter_key` и вызвать её из `apply_network_adapter`**

В `crates/platform/src/hcs_config.rs` добавить функцию сразу перед `apply_network_adapter`:

```rust
/// The key HCS's `NetworkAdapters` section uses for a VM's adapter.
///
/// HCS keys each adapter by a device identifier of the caller's choosing. The
/// endpoint's own id serves: it is unique, it is stable across starts, and
/// using it means nothing further has to be remembered to find the adapter
/// again.
///
/// Both the section a start writes and the resource path a detach names are
/// built from here. A spelling that drifted between them would detach nothing
/// while HCS still reported success.
pub(crate) fn adapter_key(endpoint_id: Uuid) -> String {
    format!("{:?}", GUID::from_u128(endpoint_id.as_u128()))
}
```

В `apply_network_adapter` заменить строки

```rust
    // HCS keys each adapter by a device identifier of the caller's choosing.
    // The endpoint's own id serves: it is unique, it is stable across starts,
    // and using it means nothing further has to be remembered to find the
    // adapter again.
    let id = format!("{:?}", GUID::from_u128(endpoint_id.as_u128()));
```

на

```rust
    let id = adapter_key(endpoint_id);
```

- [ ] **Step 4: Убедиться, что тест проходит**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform hcs_config`
Expected: PASS, включая уже существующие тесты `apply_network_adapter`.

- [ ] **Step 5: Написать падающий тест на документ detach'а**

В `crates/platform/src/hcs.rs`, в `mod tests`, добавить `detach_adapter_document` в `use super::{...}` и добавить тест:

```rust
    #[test]
    fn the_detach_document_removes_the_adapter_keyed_by_the_endpoint() {
        // The resource path has to spell the adapter exactly the way the stored
        // configuration keys it, or HCS removes nothing and reports success.
        let endpoint_id = uuid::Uuid::from_u128(0x3f2b_0c11_5c78_4c1b_9e2f_3a8b_7d4c_6e50);

        let document: serde_json::Value =
            serde_json::from_str(&detach_adapter_document(endpoint_id)).unwrap();

        assert_eq!(
            document["ResourcePath"],
            "VirtualMachine/Devices/NetworkAdapters/3F2B0C11-5C78-4C1B-9E2F-3A8B7D4C6E50"
        );
        assert_eq!(document["RequestType"], "Remove");
    }

    #[test]
    fn the_detach_path_uses_the_configurations_own_adapter_key() {
        let endpoint_id = uuid::Uuid::new_v4();

        let document: serde_json::Value =
            serde_json::from_str(&detach_adapter_document(endpoint_id)).unwrap();

        assert!(
            document["ResourcePath"]
                .as_str()
                .unwrap()
                .ends_with(&crate::hcs_config::adapter_key(endpoint_id)),
            "{document}"
        );
    }
```

- [ ] **Step 6: Убедиться, что тесты не компилируются**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform hcs::`
Expected: ошибка компиляции `cannot find function 'detach_adapter_document' in module 'super'`.

- [ ] **Step 7: Добавить документ detach'а и обёртку `HcsModifyComputeSystem`**

В `crates/platform/src/hcs.rs` расширить импорты: в блоке `System::HostComputeSystem::{...}` добавить `HcsModifyComputeSystem`, а после `use crate::error::windows_error;` добавить

```rust
use uuid::Uuid;

use crate::hcs_config::adapter_key;
```

Рядом с `const ENUMERATE_TIMEOUT` добавить:

```rust
/// A hot-detach needs nothing from the guest -- HCS removes the device itself
/// -- so the bound only guards against a wedged Host Compute Service.
const DETACH_TIMEOUT: Duration = Duration::from_secs(30);
```

Рядом с `fn shutdown_options()` добавить:

```rust
/// The document asking HCS to hot-detach the adapter keyed by `endpoint_id`.
///
/// `RequestType: "Remove"` against the adapter's own resource path: HCS takes
/// the device out of the running VM, and HNS releases the endpoint it was
/// attached to.
fn detach_adapter_document(endpoint_id: Uuid) -> String {
    format!(
        r#"{{"ResourcePath":"VirtualMachine/Devices/NetworkAdapters/{}","RequestType":"Remove"}}"#,
        adapter_key(endpoint_id)
    )
}
```

В `impl HcsSystem`, сразу после `terminate_and_wait`, добавить метод:

```rust
    /// Hot-detaches the network adapter keyed by `endpoint_id` from this
    /// running compute system and waits for HCS to report the outcome.
    ///
    /// HNS keeps an endpoint attached to the compute system it was handed to
    /// even after HCS destroys that system, so a VM terminated with its adapter
    /// still in place leaves the endpoint occupied: the next start fails with
    /// `HCN_E_ENDPOINT_ALREADY_ATTACHED`. Detaching before the VM stops is what
    /// keeps the endpoint -- and therefore the guest's address -- reusable.
    pub fn remove_network_adapter(&self, endpoint_id: Uuid) -> Result<(), RepositoryError> {
        log::debug!(
            "detaching the adapter of endpoint {endpoint_id} from HCS compute system \"{}\"",
            self.id
        );
        let operation = HcsOperation::new();
        let document = HSTRING::from(detach_adapter_document(endpoint_id));
        // SAFETY: `self.handle` and `operation.0` are valid owned handles for
        // the duration of this call, and `document` outlives it. A null
        // identity asks HCS to act as the calling process, which is what every
        // other call in this module does.
        unsafe { HcsModifyComputeSystem(self.handle, operation.0, &document, None) }.map_err(
            |error| {
                let error = windows_error("modify compute system", Some(&self.id), error);
                log::error!("{error}");
                error
            },
        )?;

        operation
            .wait_for_completion(DETACH_TIMEOUT)
            .map(|_document| ())
            .inspect_err(|error| {
                log::error!(
                    "detaching the adapter of HCS compute system \"{}\" failed: {error}",
                    self.id
                );
            })
    }
```

- [ ] **Step 8: Убедиться, что тесты проходят**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform`
Expected: PASS.

- [ ] **Step 9: Проверить clippy и закоммитить**

```bash
cargo clippy --target=x86_64-pc-windows-gnu --all-targets
git add crates/platform/src/hcs.rs crates/platform/src/hcs_config.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-46: Add a safe hot-detach of a running VM's network adapter"
```

---

### Task 2: Force Stop отцепляет адаптер перед terminate

**Files:**
- Modify: `crates/platform/src/force_stop.rs` целиком (тип пайплайна, `force_stop`, продакшн-детачер, тесты)

**Interfaces:**
- Consumes: `HcsSystem::remove_network_adapter(&self, endpoint_id: Uuid) -> Result<(), RepositoryError>` (Task 1)
- Produces: `VmForceStopPipeline::production()` с двумя швами; сигнатура `force_stop(&self, store: &MetadataStore, vm_name: &str) -> Result<(), RepositoryError>` не меняется

- [ ] **Step 1: Написать падающие тесты**

В `crates/platform/src/force_stop.rs`, в `mod tests`, заменить `struct Fixture`, `fn fixture` и `fn pipeline` целиком на:

```rust
    struct Fixture {
        _root: TempRoot,
        store: MetadataStore,
        mapping: VmComputeSystemMapping,
        /// Every step in the order it ran, so a test can assert that the
        /// adapter came off before the VM was destroyed rather than only that
        /// both happened.
        steps: Arc<Mutex<Vec<&'static str>>>,
        detaches: Arc<Mutex<Vec<(String, Uuid)>>>,
        terminations: Arc<Mutex<Vec<String>>>,
    }

    fn fixture(label: &str) -> Fixture {
        fixture_with(label, NetworkMode::None, None)
    }

    fn fixture_with(label: &str, network_mode: NetworkMode, endpoint_id: Option<Uuid>) -> Fixture {
        let root = temp_root(label);
        let mapping = VmComputeSystemMapping {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
            disk_gb: 20,
            endpoint_id,
            network_mode,
        };
        let store = MetadataStore::new(root.0.join("vm-mapping.json"));
        store
            .insert(mapping.clone())
            .expect("mapping should be persisted");

        Fixture {
            store,
            mapping,
            steps: Arc::new(Mutex::new(Vec::new())),
            detaches: Arc::new(Mutex::new(Vec::new())),
            terminations: Arc::new(Mutex::new(Vec::new())),
            _root: root,
        }
    }

    /// Which collaborators fail; by default none of them do.
    #[derive(Clone, Copy, Default)]
    struct Behavior {
        fail_detach: bool,
        fail_terminate: bool,
    }

    fn pipeline(fixture: &Fixture, behavior: Behavior) -> VmForceStopPipeline {
        let detach_steps = Arc::clone(&fixture.steps);
        let detaches = Arc::clone(&fixture.detaches);
        let terminate_steps = Arc::clone(&fixture.steps);
        let terminations = Arc::clone(&fixture.terminations);
        VmForceStopPipeline::for_test(
            move |id: &str, endpoint_id: Uuid| {
                detach_steps.lock().unwrap().push("detach");
                detaches
                    .lock()
                    .unwrap()
                    .push((id.to_owned(), endpoint_id));
                if behavior.fail_detach {
                    return Err(RepositoryError::new("injected detach failure"));
                }
                Ok(())
            },
            move |id: &str| {
                terminate_steps.lock().unwrap().push("terminate");
                terminations.lock().unwrap().push(id.to_owned());
                if behavior.fail_terminate {
                    return Err(RepositoryError::new("injected termination failure"));
                }
                Ok(())
            },
        )
    }
```

Заменить каждый существующий вызов `pipeline(&fixture.terminations, false)` на `pipeline(&fixture, Behavior::default())`, а `pipeline(&fixture.terminations, true)` (в `propagates_a_termination_failure`) — на

```rust
        pipeline(
            &fixture,
            Behavior {
                fail_terminate: true,
                ..Behavior::default()
            },
        )
```

Добавить в начало `mod tests` недостающий импорт: в строку `use uuid::Uuid;` ничего менять не нужно, она уже есть.

Затем добавить новые тесты в конец `mod tests`:

```rust
    #[test]
    fn a_nat_vm_loses_its_adapter_before_the_compute_system_is_destroyed() {
        // HNS keeps the endpoint attached to a compute system HCS has already
        // destroyed, so detaching after the termination is too late: the
        // endpoint stays occupied and the next start fails.
        let endpoint_id = Uuid::new_v4();
        let fixture = fixture_with("nat-order", NetworkMode::Nat, Some(endpoint_id));

        pipeline(&fixture, Behavior::default())
            .force_stop(&fixture.store, "dev")
            .expect("force stop should succeed");

        assert_eq!(
            fixture.steps.lock().unwrap().clone(),
            vec!["detach", "terminate"]
        );
        assert_eq!(
            fixture.detaches.lock().unwrap().clone(),
            vec![(fixture.mapping.hcs_compute_system_id.clone(), endpoint_id)]
        );
    }

    #[test]
    fn a_vm_without_networking_is_terminated_without_a_detach() {
        let fixture = fixture("no-network");

        pipeline(&fixture, Behavior::default())
            .force_stop(&fixture.store, "dev")
            .expect("force stop should succeed");

        assert!(fixture.detaches.lock().unwrap().is_empty());
        assert_eq!(fixture.steps.lock().unwrap().clone(), vec!["terminate"]);
    }

    #[test]
    fn a_nat_vm_that_never_started_has_no_endpoint_to_detach() {
        // `endpoint_id` is only recorded by the first start, so a NAT VM
        // terminated before it ever ran has nothing attached.
        let fixture = fixture_with("nat-unstarted", NetworkMode::Nat, None);

        pipeline(&fixture, Behavior::default())
            .force_stop(&fixture.store, "dev")
            .expect("force stop should succeed");

        assert!(fixture.detaches.lock().unwrap().is_empty());
        assert_eq!(fixture.steps.lock().unwrap().clone(), vec!["terminate"]);
    }

    #[test]
    fn a_failed_detach_still_stops_the_vm() {
        // Force stop is the last way to stop a wedged VM; refusing to terminate
        // because the adapter would not come off would take that away. The
        // consequence -- an occupied endpoint -- is what the start's recovery
        // exists for.
        let endpoint_id = Uuid::new_v4();
        let fixture = fixture_with("detach-failure", NetworkMode::Nat, Some(endpoint_id));

        pipeline(
            &fixture,
            Behavior {
                fail_detach: true,
                ..Behavior::default()
            },
        )
        .force_stop(&fixture.store, "dev")
        .expect("a failed detach must not keep the VM running");

        assert_eq!(fixture.terminations.lock().unwrap().len(), 1);
    }

    #[test]
    fn rejects_an_unmapped_vm_without_detaching_anything() {
        let fixture = fixture("unmapped-detach");

        pipeline(&fixture, Behavior::default())
            .force_stop(&fixture.store, "missing-vm")
            .expect_err("an unmapped VM must not be stopped");

        assert!(fixture.detaches.lock().unwrap().is_empty());
    }
```

Существующие тесты `terminates_the_compute_system_mapped_to_the_vm`, `rejects_an_unmapped_vm_without_touching_hcs`, `propagates_a_termination_failure` и `leaves_the_mapping_in_place_so_the_vm_can_be_started_again` остаются, только с новым вызовом `pipeline`; в них `fixture.terminations` уже есть.

- [ ] **Step 2: Убедиться, что тесты не компилируются**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform force_stop`
Expected: ошибка компиляции — `for_test` принимает один аргумент, а не два.

- [ ] **Step 3: Добавить шов detach'а в пайплайн**

В `crates/platform/src/force_stop.rs` заменить всё от `type SystemTerminator` до конца `fn terminate_hcs_system` на:

```rust
type AdapterDetacher = Box<dyn Fn(&str, Uuid) -> Result<(), RepositoryError>>;
type SystemTerminator = Box<dyn Fn(&str) -> Result<(), RepositoryError>>;

/// Forcibly stops VMs known to [`MetadataStore`].
pub struct VmForceStopPipeline {
    adapter_detacher: AdapterDetacher,
    system_terminator: SystemTerminator,
}

impl VmForceStopPipeline {
    /// Creates a pipeline backed by the real HCS API.
    #[must_use]
    pub fn production() -> Self {
        Self {
            adapter_detacher: Box::new(detach_network_adapter),
            system_terminator: Box::new(terminate_hcs_system),
        }
    }

    #[cfg(test)]
    fn for_test(
        detacher: impl Fn(&str, Uuid) -> Result<(), RepositoryError> + 'static,
        terminator: impl Fn(&str) -> Result<(), RepositoryError> + 'static,
    ) -> Self {
        Self {
            adapter_detacher: Box::new(detacher),
            system_terminator: Box::new(terminator),
        }
    }

    /// Stops the VM named `vm_name` without involving its guest, the
    /// equivalent of pulling a physical machine's power cord.
    ///
    /// This is the fallback for a guest that cannot or will not service a
    /// graceful shutdown (see [`crate::VmShutdownPipeline`]), so it discards
    /// whatever the guest had not yet flushed to disk.
    ///
    /// A VM on the network loses its adapter first. HNS keeps an endpoint
    /// attached to the compute system it was handed to even after HCS destroys
    /// that system, so terminating with the adapter in place leaves the
    /// endpoint occupied and the next start fails with
    /// `HCN_E_ENDPOINT_ALREADY_ATTACHED`.
    ///
    /// HCS destroys the compute system as it stops, exactly as it does when a
    /// guest powers itself off, but nothing the VM is made of is lost: its
    /// disks, its stored configuration and its [`MetadataStore`] mapping all
    /// survive, and [`crate::VmStartPipeline`] rebuilds the compute system from
    /// them on the next start.
    pub fn force_stop(&self, store: &MetadataStore, vm_name: &str) -> Result<(), RepositoryError> {
        let mapping = store.find_by_vm_name(vm_name)?.ok_or_else(|| {
            let error = RepositoryError::new(format!("no HCS mapping found for VM \"{vm_name}\""));
            log::error!("{error}");
            error
        })?;

        log::info!(
            "forcibly stopping VM \"{}\" ({}) as HCS compute system \"{}\"",
            mapping.vm_name,
            mapping.vm_id,
            mapping.hcs_compute_system_id
        );

        self.detach_adapter(&mapping);

        (self.system_terminator)(&mapping.hcs_compute_system_id).inspect_err(|error| {
            log::error!(
                "failed to forcibly stop VM \"{}\": {error}",
                mapping.vm_name
            );
        })?;

        log::info!(
            "forcibly stopped VM \"{}\" ({})",
            mapping.vm_name,
            mapping.vm_id
        );
        Ok(())
    }

    /// Takes the VM's adapter off before the compute system is destroyed.
    ///
    /// A failure is reported but not propagated: force stop is the last way to
    /// stop a wedged VM, and refusing to terminate because the adapter would
    /// not come off would take that away. The consequence of a detach that did
    /// not happen -- an endpoint HNS still considers attached -- is recovered
    /// by [`crate::VmStartPipeline`] on the next start.
    fn detach_adapter(&self, mapping: &VmComputeSystemMapping) {
        let attached = (mapping.network_mode == NetworkMode::Nat)
            .then_some(mapping.endpoint_id)
            .flatten();
        let Some(endpoint_id) = attached else {
            log::debug!(
                "VM \"{}\" has no attached endpoint; terminating it without a detach",
                mapping.vm_name
            );
            return;
        };

        if let Err(error) =
            (self.adapter_detacher)(&mapping.hcs_compute_system_id, endpoint_id)
        {
            log::warn!(
                "the adapter of VM \"{}\" could not be detached before it was forcibly stopped: \
                 {error}; HNS keeps endpoint {endpoint_id} attached to the compute system being \
                 destroyed, so the next start has to replace the endpoint and the guest may be \
                 offered a different address",
                mapping.vm_name
            );
        }
    }
}

impl Default for VmForceStopPipeline {
    fn default() -> Self {
        Self::production()
    }
}

/// Takes a running VM's adapter off, treating a compute system HCS no longer
/// knows as nothing to detach.
fn detach_network_adapter(id: &str, endpoint_id: Uuid) -> Result<(), RepositoryError> {
    match HcsSystem::open_if_present(id, HCS_ACCESS_ALL)? {
        Some(system) => system.remove_network_adapter(endpoint_id),
        None => {
            log::debug!("HCS no longer knows compute system \"{id}\"; nothing to detach");
            Ok(())
        }
    }
}

fn terminate_hcs_system(id: &str) -> Result<(), RepositoryError> {
    // The system handle must outlive the termination operation it issued.
    let system = HcsSystem::open(id, HCS_ACCESS_ALL)?;
    system.terminate_and_wait(FORCE_STOP_TIMEOUT)
}
```

Заменить блок импортов в начале файла на:

```rust
use std::time::Duration;

use uuid::Uuid;
use vmlord_core::{NetworkMode, RepositoryError};

use crate::{
    HcsSystem,
    hcs::HCS_ACCESS_ALL,
    metadata::{MetadataStore, VmComputeSystemMapping},
};
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform force_stop`
Expected: PASS, девять тестов.

- [ ] **Step 5: Проверить clippy и закоммитить**

```bash
cargo clippy --target=x86_64-pc-windows-gnu --all-targets
git add crates/platform/src/force_stop.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-46: Detach the adapter before a forced stop destroys the VM"
```

---

### Task 3: Адрес endpoint'а — чтение и запрос при создании

**Files:**
- Modify: `crates/platform/src/hcn_endpoint.rs` (константа, `EndpointAddress`, `address`, `create_with_address`, `endpoint_settings`, тесты)
- Modify: `crates/platform/src/lib.rs:40` (экспорт)

**Interfaces:**
- Produces: `pub struct EndpointAddress { pub ip_address: String, pub prefix_length: u8 }`
- Produces: `pub fn HcnEndpoint::address(&self) -> Result<Option<EndpointAddress>, RepositoryError>`
- Produces: `pub fn HcnEndpoint::create_with_address(network: &HcnNetwork, id: Uuid, vm_name: &str, address: Option<&EndpointAddress>) -> Result<Self, RepositoryError>`
- Produces: `pub(crate) const HCN_E_ENDPOINT_ALREADY_ATTACHED: HRESULT` в `crates/platform/src/hcn_endpoint.rs`
- `HcnEndpoint::create` сохраняется и делегирует в `create_with_address` с `None`

- [ ] **Step 1: Написать падающие тесты**

В `crates/platform/src/hcn_endpoint.rs`, в `mod tests`, заменить строку `use super::{endpoint_settings, mac_address};` на

```rust
    use super::{EndpointAddress, address, endpoint_settings, mac_address};
```

и заменить `fn settings` на

```rust
    fn settings(vm_name: &str) -> serde_json::Value {
        serde_json::from_str(&endpoint_settings(vm_name, None).unwrap())
            .expect("the settings document should be valid JSON")
    }
```

Добавить тесты в конец модуля:

```rust
    #[test]
    fn requested_settings_name_the_address_the_endpoint_must_take() {
        // The recovery path recreates an endpoint that HNS still considers
        // attached. Asking for the address the old one held is what keeps the
        // guest reachable at the same place afterwards.
        let requested = EndpointAddress {
            ip_address: "172.22.42.7".into(),
            prefix_length: 24,
        };

        let document: serde_json::Value = serde_json::from_str(
            &endpoint_settings("dev-linux", Some(&requested)).unwrap(),
        )
        .unwrap();

        assert_eq!(
            document["IpConfigurations"],
            serde_json::json!([{ "IpAddress": "172.22.42.7", "PrefixLength": 24 }])
        );
    }

    #[test]
    fn the_address_is_read_from_the_reported_properties() {
        let properties = serde_json::json!({
            "MacAddress": "00-15-5D-01-02-03",
            "IpConfigurations": [{ "IpAddress": "172.22.42.7", "PrefixLength": 24 }]
        });

        assert_eq!(
            address(&properties),
            Some(EndpointAddress {
                ip_address: "172.22.42.7".into(),
                prefix_length: 24,
            })
        );
    }

    #[test]
    fn properties_without_a_usable_address_report_none() {
        // An endpoint HNS has not finished setting up, or one whose properties
        // it no longer reports, leaves the recovery with no address to hold on
        // to; that is a warning, not a parse failure.
        for properties in [
            serde_json::json!({}),
            serde_json::json!({ "IpConfigurations": [] }),
            serde_json::json!({ "IpConfigurations": [{ "PrefixLength": 24 }] }),
            serde_json::json!({ "IpConfigurations": [{ "IpAddress": "" , "PrefixLength": 24 }] }),
            serde_json::json!({ "IpConfigurations": [{ "IpAddress": "172.22.42.7" }] }),
        ] {
            assert_eq!(address(&properties), None, "{properties}");
        }
    }
```

- [ ] **Step 2: Убедиться, что тесты не компилируются**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform hcn_endpoint`
Expected: ошибка компиляции — `EndpointAddress` и `address` не существуют, `endpoint_settings` принимает один аргумент.

- [ ] **Step 3: Реализовать адрес endpoint'а**

В `crates/platform/src/hcn_endpoint.rs` после константы `HCN_E_ENDPOINT_NOT_FOUND` добавить:

```rust
/// `HCN_E_ENDPOINT_ALREADY_ATTACHED` from `computenetwork.h` (facility 0x3B).
///
/// Reported through HCS rather than through an HCN call: a compute system whose
/// configuration names an endpoint HNS still has attached elsewhere fails to be
/// created or started with this code. HNS holds that attachment even after HCS
/// has destroyed the compute system it points at, which is what a VM stopped
/// without a detach leaves behind.
pub(crate) const HCN_E_ENDPOINT_ALREADY_ATTACHED: HRESULT = HRESULT(0x803B_0014_u32 as i32);

/// An address HNS assigned to an endpoint.
///
/// Read back rather than chosen: the network's IPAM allocates it, and VMLord
/// only ever repeats it when it has to recreate an endpoint that already had
/// one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointAddress {
    pub ip_address: String,
    pub prefix_length: u8,
}
```

В `impl HcnEndpoint` заменить `pub fn create` на:

```rust
    /// Creates the endpoint `id` for VM `vm_name` inside `network`.
    ///
    /// The caller allocates the identifier and is responsible for recording it
    /// before anything else can find the endpoint again: a VMLord that dies
    /// between this call and that write leaves an orphan behind, which is what
    /// the cleanup on `initialize` exists to collect.
    pub fn create(network: &HcnNetwork, id: Uuid, vm_name: &str) -> Result<Self, RepositoryError> {
        Self::create_with_address(network, id, vm_name, None)
    }

    /// Creates the endpoint `id`, asking HNS for `address` when one is given.
    ///
    /// Only the recovery path passes an address, and only one HNS itself
    /// assigned to the endpoint being replaced: repeating it is what keeps the
    /// guest reachable where it was. VMLord never invents an address, so it
    /// does not become a second allocator beside HNS's IPAM.
    pub fn create_with_address(
        network: &HcnNetwork,
        id: Uuid,
        vm_name: &str,
        address: Option<&EndpointAddress>,
    ) -> Result<Self, RepositoryError> {
        let settings = endpoint_settings(vm_name, address)?;
        log::debug!("creating HCN endpoint {id} for VM \"{vm_name}\"");

        let guid = GUID::from_u128(id.as_u128());
        let settings = HSTRING::from(settings);
        let mut endpoint = ptr::null_mut();
        // SAFETY: The network handle is owned by `network` and outlives the
        // call, as do `guid` and `settings`, and the output pointer is valid
        // for it. On success HCN transfers ownership of the returned handle to
        // this wrapper.
        unsafe { HcnCreateEndpoint(network.handle(), &guid, &settings, &mut endpoint, None) }
            .map_err(|error| {
                let error = windows_error("create HCN endpoint", Some(vm_name), error);
                log::error!("{error}");
                error
            })?;

        log::info!("created HCN endpoint {id} for VM \"{vm_name}\"");
        Ok(Self(endpoint))
    }
```

После `pub fn mac_address` добавить:

```rust
    /// The address HNS assigned to this endpoint, if it reports one.
    ///
    /// `Ok(None)` rather than an error when there is none: the only caller is
    /// the recovery that recreates an occupied endpoint, and a missing address
    /// costs the guest its old one but must not cost it the start.
    pub fn address(&self) -> Result<Option<EndpointAddress>, RepositoryError> {
        let properties = self.properties()?;
        let properties: serde_json::Value = serde_json::from_str(&properties).map_err(|error| {
            let error = RepositoryError::new(format!(
                "the properties HNS reported for the endpoint are not valid JSON: {error}"
            ));
            log::error!("{error}");
            error
        })?;

        Ok(address(&properties))
    }
```

Рядом с `fn mac_address` добавить:

```rust
/// Reads the first address out of the properties HNS reports for an endpoint.
///
/// One address: VMLord's endpoints are created with none of their own, so HNS's
/// IPAM assigns exactly one out of the network's subnet.
fn address(properties: &serde_json::Value) -> Option<EndpointAddress> {
    let configuration = properties.get("IpConfigurations")?.as_array()?.first()?;
    let ip_address = configuration.get("IpAddress")?.as_str()?;
    if ip_address.is_empty() {
        return None;
    }
    let prefix_length = u8::try_from(configuration.get("PrefixLength")?.as_u64()?).ok()?;

    Some(EndpointAddress {
        ip_address: ip_address.to_owned(),
        prefix_length,
    })
}
```

Заменить `fn endpoint_settings` и `struct EndpointSettings` на:

```rust
/// Builds the settings document for the endpoint of VM `vm_name`.
///
/// Without `address`, no address is asked for: the network's own IPAM assigns
/// one out of the subnet the network was created with, and that address -- not
/// one VMLord picked -- is what the guest is offered and what
/// `HcnQueryEndpointProperties` later reports. `address` is only ever an
/// address HNS assigned to an earlier endpoint of the same VM.
fn endpoint_settings(
    vm_name: &str,
    address: Option<&EndpointAddress>,
) -> Result<String, RepositoryError> {
    let settings = EndpointSettings {
        schema_version: SchemaVersion::V2,
        name: endpoint_name(vm_name),
        // The identifier goes to `HcnCreateEndpoint` as an argument, but the
        // network the endpoint joins is named only here.
        host_compute_network: GUID::from_u128(VMLORD_NETWORK_ID),
        flags: 0,
        ip_configurations: address
            .map(|address| IpConfiguration {
                ip_address: address.ip_address.clone(),
                prefix_length: address.prefix_length,
            })
            .into_iter()
            .collect(),
    };

    serde_json::to_string(&settings).map_err(|error| {
        RepositoryError::new(format!(
            "failed to serialize the HCN endpoint settings of VM \"{vm_name}\": {error}"
        ))
    })
}
```

```rust
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct EndpointSettings {
    schema_version: SchemaVersion,
    name: String,
    #[serde(serialize_with = "serialize_guid")]
    host_compute_network: GUID,
    flags: u32,
    /// Omitted entirely when empty: an `IpConfigurations: []` would be VMLord
    /// asking HNS for no address rather than leaving the choice to its IPAM.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ip_configurations: Vec<IpConfiguration>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct IpConfiguration {
    ip_address: String,
    prefix_length: u8,
}
```

В `crates/platform/src/lib.rs` заменить строку `pub use hcn_endpoint::HcnEndpoint;` на

```rust
pub use hcn_endpoint::{EndpointAddress, HcnEndpoint};
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform hcn_endpoint`
Expected: PASS. Существующий тест `the_settings_ask_for_no_address_of_their_own` должен остаться зелёным — `skip_serializing_if` убирает пустой ключ.

- [ ] **Step 5: Проверить clippy и закоммитить**

```bash
cargo clippy --target=x86_64-pc-windows-gnu --all-targets
git add crates/platform/src/hcn_endpoint.rs crates/platform/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-46: Read an endpoint's address and let a new one ask for it"
```

---

### Task 4: Старт различает отказ «endpoint занят»

**Files:**
- Modify: `crates/platform/src/hcs.rs` (`HcsStartFailure`, `call_failure`, `start_failure`, `start_and_wait`, `create_system_and_wait`, тесты)
- Modify: `crates/platform/src/start.rs:337-355` (`start_hcs_system`) и `crates/platform/src/start.rs:37` (тип шва)
- Modify: `crates/platform/src/lib.rs:41` (экспорт)

**Interfaces:**
- Consumes: `HCN_E_ENDPOINT_ALREADY_ATTACHED` (Task 3)
- Produces: `pub enum HcsStartFailure { EndpointBusy(RepositoryError), Failed(RepositoryError) }` с `pub fn into_error(self) -> RepositoryError`
- Produces: `pub fn HcsSystem::start_and_wait(&self, timeout: Duration) -> Result<(), HcsStartFailure>`
- Produces: `pub fn HcsClient::create_system_and_wait(&self, id: &str, configuration: &str, timeout: Duration) -> Result<HcsSystem, HcsStartFailure>`
- Produces: `type SystemStarter = Box<dyn Fn(&str, &str) -> Result<(), HcsStartFailure>>` в `start.rs`

- [ ] **Step 1: Написать падающий тест на классификацию**

В `crates/platform/src/hcs.rs`, в `mod tests`, добавить `HcsStartFailure`, `call_failure` в `use super::{...}` и добавить тесты:

```rust
    #[test]
    fn an_occupied_endpoint_is_classified_apart_from_every_other_failure() {
        // The start retries only this one code; misclassifying it either loses
        // the recovery or retries a start that will never succeed.
        let busy = call_failure(
            "start compute system",
            "vmlord-dev",
            windows::core::Error::from_hresult(windows::core::HRESULT(0x803B_0014_u32 as i32)),
        );

        assert!(matches!(busy, HcsStartFailure::EndpointBusy(_)));
        let message = busy.into_error().to_string();
        assert!(message.contains("0x803B0014"), "{message}");
        assert!(message.contains("vmlord-dev"), "{message}");
    }

    #[test]
    fn any_other_hresult_is_an_ordinary_failure() {
        let denied = call_failure(
            "start compute system",
            "vmlord-dev",
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8007_0005_u32 as i32)),
        );

        assert!(matches!(denied, HcsStartFailure::Failed(_)));
        assert!(denied.into_error().to_string().contains("0x80070005"));
    }
```

- [ ] **Step 2: Убедиться, что тесты не компилируются**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform hcs::`
Expected: ошибка компиляции — `HcsStartFailure` и `call_failure` не существуют.

- [ ] **Step 3: Добавить классификацию и ждущие обёртки в `hcs.rs`**

В `crates/platform/src/hcs.rs` добавить импорт рядом с `use crate::hcs_config::adapter_key;`:

```rust
use crate::hcn_endpoint::HCN_E_ENDPOINT_ALREADY_ATTACHED;
```

После функции `wait_failure` добавить:

```rust
/// Why a compute system could not be created or started.
///
/// One cause is worth separating from every other: an endpoint HNS still has
/// attached to a compute system that no longer exists cannot be attached again,
/// and no retry of the same start fixes it -- only replacing the endpoint does.
pub enum HcsStartFailure {
    /// HNS reported `HCN_E_ENDPOINT_ALREADY_ATTACHED`.
    EndpointBusy(RepositoryError),
    Failed(RepositoryError),
}

impl HcsStartFailure {
    /// The failure as the repository boundary reports it, whatever its cause.
    #[must_use]
    pub fn into_error(self) -> RepositoryError {
        match self {
            Self::EndpointBusy(error) | Self::Failed(error) => error,
        }
    }
}

/// Classifies a call HCS refused outright.
fn call_failure(operation: &str, id: &str, error: windows::core::Error) -> HcsStartFailure {
    let endpoint_busy = error.code() == HCN_E_ENDPOINT_ALREADY_ATTACHED;
    let error = windows_error(operation, Some(id), error);
    log::error!("{error}");
    if endpoint_busy {
        HcsStartFailure::EndpointBusy(error)
    } else {
        HcsStartFailure::Failed(error)
    }
}

/// Classifies an operation HCS accepted and then failed.
fn operation_failure(
    operation: &str,
    id: &str,
    timeout: Duration,
    failure: WaitFailure,
) -> HcsStartFailure {
    match failure {
        WaitFailure::Windows(error) if error.code() == HCN_E_ENDPOINT_ALREADY_ATTACHED => {
            let error = windows_error(operation, Some(id), error);
            log::error!("{error}");
            HcsStartFailure::EndpointBusy(error)
        }
        failure => {
            let error = wait_failure(timeout, failure);
            log::error!("the {operation} of \"{id}\" failed: {error}");
            HcsStartFailure::Failed(error)
        }
    }
}
```

В `impl HcsSystem`, после `start`, добавить:

```rust
    /// Starts the compute system and waits up to `timeout`, saying whether the
    /// start failed because the VM's endpoint is still attached elsewhere.
    ///
    /// This is what [`crate::VmStartPipeline`] uses: an occupied endpoint is
    /// the one failure it can recover from, and it can only recognise it here,
    /// where the raw HRESULT is still available.
    pub fn start_and_wait(&self, timeout: Duration) -> Result<(), HcsStartFailure> {
        log::debug!("starting HCS compute system \"{}\"", self.id);
        let operation = HcsOperation::new();
        // SAFETY: `self.handle` and `operation.0` are valid owned handles for
        // the duration of this call. Null options are accepted here: a start
        // takes its parameters from the compute system's own configuration.
        unsafe { HcsStartComputeSystem(self.handle, operation.0, PCWSTR::null()) }
            .map_err(|error| call_failure("start compute system", &self.id, error))?;

        operation
            .wait(timeout)
            .map(|_document| ())
            .map_err(|failure| {
                operation_failure("start compute system", &self.id, timeout, failure)
            })
    }
```

В `impl HcsClient`, после `create_system`, добавить:

```rust
    /// Creates a compute system and waits up to `timeout` for the creation to
    /// complete, saying whether it failed because the VM's endpoint is still
    /// attached elsewhere.
    ///
    /// Unlike [`HcsClient::create_system`], the creation is awaited here rather
    /// than handed back: the returned system is one the caller can start, and
    /// keeping the handle alive across the wait is this method's business.
    pub fn create_system_and_wait(
        &self,
        id: &str,
        configuration: &str,
        timeout: Duration,
    ) -> Result<HcsSystem, HcsStartFailure> {
        log::debug!("creating HCS compute system \"{id}\"");
        let operation = HcsOperation::new();
        let hcs_id = HSTRING::from(id);
        let hcs_configuration = HSTRING::from(configuration);
        // SAFETY: `hcs_id` and `hcs_configuration` remain valid for the
        // duration of the call. On success the returned system handle is
        // transferred to `HcsSystem` for ownership.
        let handle =
            unsafe { HcsCreateComputeSystem(&hcs_id, &hcs_configuration, operation.0, None) }
                .map_err(|error| call_failure("create compute system", id, error))?;
        let system = HcsSystem {
            handle,
            id: id.to_owned(),
        };

        operation
            .wait(timeout)
            .map_err(|failure| operation_failure("create compute system", id, timeout, failure))?;

        Ok(system)
    }
```

В `crates/platform/src/lib.rs` заменить строку экспорта `hcs` на:

```rust
pub use hcs::{
    HcsClient, HcsOperation, HcsStartFailure, HcsSystem, HcsSystemState, HcsSystemSummary,
};
```

- [ ] **Step 4: Перевести `start_hcs_system` на классифицированный отказ**

В `crates/platform/src/start.rs` заменить `type SystemStarter` на

```rust
type SystemStarter = Box<dyn Fn(&str, &str) -> Result<(), HcsStartFailure>>;
```

и расширить импорт `crate::{...}`: заменить `hcs::HCS_ACCESS_ALL,` на

```rust
    cleanup,
    hcs::{HCS_ACCESS_ALL, HcsStartFailure},
```

Заменить `fn start_hcs_system` целиком на:

```rust
/// Starts the compute system `id`, re-creating it from `configuration` first
/// if HCS no longer knows it.
///
/// HCS destroys a compute system when it exits, so every VM that has been
/// stopped -- by its guest or by a forced stop -- has to be rebuilt before it
/// can run again. Re-creating from the stored configuration keeps the VM's id,
/// disks and metadata mapping unchanged, so a stop stays a stop rather than
/// becoming an implicit delete.
fn start_hcs_system(id: &str, configuration: &str) -> Result<(), HcsStartFailure> {
    // The system handle must outlive the start operation it issued.
    let existing =
        HcsSystem::open_if_present(id, HCS_ACCESS_ALL).map_err(HcsStartFailure::Failed)?;
    let system = match existing {
        Some(system) => system,
        None => {
            log::info!(
                "HCS no longer knows compute system \"{id}\"; \
                 re-creating it from the stored configuration before starting it"
            );
            HcsClient::new()
                .create_system_and_wait(id, configuration, CREATE_TIMEOUT)
                .map_err(|failure| tear_down_after_a_failed_creation(id, failure))?
        }
    };

    system.start_and_wait(START_TIMEOUT)
}

/// Removes a compute system HCS may have created before the creation failed.
///
/// `HcsCreateComputeSystem` can succeed while its operation fails, leaving a
/// system that holds the very configuration -- and therefore the very endpoint
/// -- the failed attempt named. A retry with a replaced endpoint would find
/// that system through `open_if_present` and start it with the stale adapter,
/// so it has to go first. The teardown is best-effort: it explains a start that
/// failed, it does not decide it.
fn tear_down_after_a_failed_creation(id: &str, failure: HcsStartFailure) -> HcsStartFailure {
    if let Err(error) = cleanup::teardown_compute_system(id) {
        log::warn!(
            "cleanup of the ambiguously-created compute system \"{id}\" also failed: {error}"
        );
    }
    failure
}
```

- [ ] **Step 5: Починить компиляцию `VmStartPipeline` и его тестов**

В `crates/platform/src/start.rs`, в `pub fn start`, заменить вызов стартера

```rust
        (self.system_starter)(&mapping.hcs_compute_system_id, &configuration).inspect_err(
            |error| {
                log::error!("failed to start VM \"{}\": {error}", mapping.vm_name);
            },
        )?;
```

на

```rust
        (self.system_starter)(&mapping.hcs_compute_system_id, &configuration).map_err(
            |failure| {
                let error = failure.into_error();
                log::error!("failed to start VM \"{}\": {error}", mapping.vm_name);
                error
            },
        )?;
```

В `mod tests` заменить сигнатуру стартера в `fn for_test` и в `fn pipeline`:

* в `for_test`: `system_starter: impl Fn(&str, &str) -> Result<(), HcsStartFailure> + 'static,`
* в `pipeline`, внутри замыкания стартера, заменить

```rust
                    if behavior.fail_start {
                        return Err(RepositoryError::new("injected start failure"));
                    }
```

на

```rust
                    if behavior.fail_start {
                        return Err(HcsStartFailure::Failed(RepositoryError::new(
                            "injected start failure",
                        )));
                    }
```

* добавить `HcsStartFailure` в `use crate::...` внутри `mod tests`:

```rust
    use crate::{
        hcs::HcsStartFailure,
        metadata::{MetadataStore, VmComputeSystemMapping},
    };
```

- [ ] **Step 6: Убедиться, что всё компилируется и тесты проходят**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform`
Expected: PASS. Существующий тест `propagates_a_start_failure` продолжает проверять текст «injected start failure».

- [ ] **Step 7: Проверить clippy и закоммитить**

```bash
cargo clippy --target=x86_64-pc-windows-gnu --all-targets
git add crates/platform/src/hcs.rs crates/platform/src/start.rs crates/platform/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-46: Tell an occupied endpoint apart from any other failed start"
```

---

### Task 5: Старт один раз заменяет занятый endpoint

**Files:**
- Modify: `crates/platform/src/start.rs` (`EndpointPolicy`, `attach_network`, `start`, `ensure_endpoint`, тесты)

**Interfaces:**
- Consumes: `HcsStartFailure::EndpointBusy` (Task 4); `HcnEndpoint::address`, `HcnEndpoint::create_with_address`, `EndpointAddress` (Task 3)
- Produces: `pub(crate) enum EndpointPolicy { Reuse, Replace }`
- Produces: `type EndpointProvider = Box<dyn Fn(&str, Option<Uuid>, EndpointPolicy) -> Result<VmNetworkAdapter, RepositoryError>>`
- Produces: `fn VmStartPipeline::attach_network(&self, store, mapping, vm_directory, configuration: String, recorded: Option<Uuid>, policy: EndpointPolicy) -> Result<(String, Option<Uuid>), RepositoryError>`

- [ ] **Step 1: Написать падающие тесты**

В `crates/platform/src/start.rs`, в `mod tests`, заменить `struct Calls` и `fn pipeline` на версии, знающие про политику, и добавить новые тесты.

Заменить поле `endpoint` в `struct Calls` на:

```rust
        endpoint: Arc<Mutex<Vec<(Option<Uuid>, EndpointPolicy)>>>,
```

Расширить `struct Behavior`:

```rust
    /// Which collaborators fail; by default none of them do.
    #[derive(Clone, Copy, Default)]
    struct Behavior {
        fail_start: bool,
        fail_endpoint: bool,
        /// How many leading starts fail with an occupied endpoint before the
        /// starter accepts one.
        busy_starts: usize,
    }
```

Заменить замыкание стартера в `fn pipeline` на:

```rust
            {
                let calls = calls.clone();
                move |id: &str, configuration: &str| {
                    calls.steps.lock().unwrap().push("start");
                    let mut started = calls.start.lock().unwrap();
                    started.push((id.to_owned(), configuration.to_owned()));
                    if started.len() <= behavior.busy_starts {
                        return Err(HcsStartFailure::EndpointBusy(RepositoryError::new(
                            "injected endpoint-busy failure",
                        )));
                    }
                    if behavior.fail_start {
                        return Err(HcsStartFailure::Failed(RepositoryError::new(
                            "injected start failure",
                        )));
                    }
                    Ok(())
                }
            },
```

Заменить замыкание провайдера endpoint'а на:

```rust
            {
                let calls = calls.clone();
                move |_vm_name: &str, recorded: Option<Uuid>, policy: EndpointPolicy| {
                    calls.steps.lock().unwrap().push("endpoint");
                    calls.endpoint.lock().unwrap().push((recorded, policy));
                    if behavior.fail_endpoint {
                        return Err(RepositoryError::new("injected endpoint failure"));
                    }
                    // A recorded endpoint is the one that gets reused; a VM
                    // without one, and a replacement, are handed a fresh id.
                    let endpoint_id = match policy {
                        EndpointPolicy::Reuse => recorded.unwrap_or(NEW_ENDPOINT_ID),
                        EndpointPolicy::Replace => REPLACEMENT_ENDPOINT_ID,
                    };
                    Ok(VmNetworkAdapter {
                        endpoint_id,
                        mac_address: MAC_ADDRESS.to_owned(),
                    })
                }
            },
```

Рядом с `const NEW_ENDPOINT_ID` добавить:

```rust
    /// The endpoint the test provider hands out when it replaces an occupied one.
    const REPLACEMENT_ENDPOINT_ID: Uuid =
        Uuid::from_u128(0x7a1c_44e0_5c78_4c1b_9e2f_3a8b_7d4c_6e50);
    const REPLACEMENT_ENDPOINT_GUID: &str = "7A1C44E0-5C78-4C1B-9E2F-3A8B7D4C6E50";
```

Заменить сигнатуру провайдера в `fn for_test`:

```rust
        endpoint_provider: impl Fn(&str, Option<Uuid>, EndpointPolicy) -> Result<VmNetworkAdapter, RepositoryError>
        + 'static,
```

Заменить в `use super::{...}` строку на:

```rust
    use super::{EndpointPolicy, VmNetworkAdapter, VmStartPipeline, attachment_paths};
```

Обновить существующие проверки списка вызовов провайдера:

* в `a_new_endpoint_is_recorded_in_the_mapping`:

```rust
        assert_eq!(
            calls.endpoint.lock().unwrap().clone(),
            vec![(None, EndpointPolicy::Reuse)]
        );
```

* в `a_recorded_endpoint_is_offered_for_reuse_rather_than_replaced`:

```rust
        assert_eq!(
            calls.endpoint.lock().unwrap().clone(),
            vec![(Some(recorded), EndpointPolicy::Reuse)]
        );
```

Добавить новые тесты в конец `mod tests`:

```rust
    #[test]
    fn an_occupied_endpoint_is_replaced_and_the_start_retried_once() {
        // A VM stopped without a detach leaves HNS holding its endpoint against
        // a compute system that no longer exists. Nothing can attach to it
        // again, so the only way back is a different endpoint.
        let recorded = Uuid::new_v4();
        let fixture = fixture_with("busy-retry", NetworkMode::Nat, Some(recorded));
        let calls = fixture.calls.clone();

        pipeline(
            &calls,
            Behavior {
                busy_starts: 1,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect("a start blocked by an occupied endpoint must recover");

        assert_eq!(
            calls.endpoint.lock().unwrap().clone(),
            vec![
                (Some(recorded), EndpointPolicy::Reuse),
                (Some(recorded), EndpointPolicy::Replace),
            ]
        );
        assert_eq!(
            calls.steps.lock().unwrap().clone(),
            vec!["endpoint", "grant", "grant", "start", "endpoint", "grant", "grant", "start"]
        );
    }

    #[test]
    fn the_replacement_endpoint_reaches_the_mapping_and_the_configuration() {
        let recorded = Uuid::new_v4();
        let fixture = fixture_with("busy-recorded", NetworkMode::Nat, Some(recorded));
        let calls = fixture.calls.clone();

        pipeline(
            &calls,
            Behavior {
                busy_starts: 1,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect("a start blocked by an occupied endpoint must recover");

        assert_eq!(fixture.recorded_endpoint(), Some(REPLACEMENT_ENDPOINT_ID));
        assert_eq!(
            fixture
                .configuration()
                .pointer("/VirtualMachine/Devices/NetworkAdapters"),
            Some(&serde_json::json!({
                REPLACEMENT_ENDPOINT_GUID: {
                    "EndpointId": REPLACEMENT_ENDPOINT_GUID,
                    "MacAddress": MAC_ADDRESS
                }
            }))
        );
        let started = calls.start.lock().unwrap().clone();
        assert_eq!(
            started[1].1,
            fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap()
        );
    }

    #[test]
    fn a_second_occupied_endpoint_is_not_retried_again() {
        // One replacement is a recovery; a second means something other than a
        // stale attachment is wrong, and retrying forever would create an
        // endpoint per attempt.
        let fixture = fixture_with("busy-twice", NetworkMode::Nat, Some(Uuid::new_v4()));
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                busy_starts: 2,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect_err("a second occupied endpoint must fail the start");

        assert!(error.to_string().contains("injected endpoint-busy failure"));
        assert_eq!(calls.start.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_vm_without_networking_never_replaces_an_endpoint() {
        // Without NAT there is no endpoint to blame, so the failure is reported
        // as it came rather than retried.
        let fixture = fixture("busy-no-network");
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                busy_starts: 1,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect_err("a VM without an endpoint has no recovery");

        assert!(error.to_string().contains("injected endpoint-busy failure"));
        assert!(calls.endpoint.lock().unwrap().is_empty());
        assert_eq!(calls.start.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_ordinary_start_failure_is_not_retried() {
        let fixture = fixture_with("plain-failure", NetworkMode::Nat, Some(Uuid::new_v4()));
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                fail_start: true,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect_err("a failed start must be reported");

        assert!(error.to_string().contains("injected start failure"));
        assert_eq!(calls.start.lock().unwrap().len(), 1);
    }
```

- [ ] **Step 2: Убедиться, что тесты не компилируются**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform start`
Expected: ошибка компиляции — `EndpointPolicy` не существует, провайдер принимает два аргумента.

- [ ] **Step 3: Ввести политику и вернуть подключённый endpoint из `attach_network`**

В `crates/platform/src/start.rs` расширить импорт крейта: заменить `hcn_endpoint::HcnEndpoint,` на

```rust
    hcn_endpoint::{EndpointAddress, HcnEndpoint},
```

После `pub(crate) struct VmNetworkAdapter` добавить:

```rust
/// Whether a start may reuse the endpoint the VM already has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointPolicy {
    /// Reuse the recorded endpoint, creating one only when the VM has none.
    Reuse,
    /// Replace the recorded endpoint: HNS still has it attached to a compute
    /// system that no longer exists, so nothing can attach to it again.
    Replace,
}
```

Заменить `type EndpointProvider` на:

```rust
type EndpointProvider =
    Box<dyn Fn(&str, Option<Uuid>, EndpointPolicy) -> Result<VmNetworkAdapter, RepositoryError>>;
```

Заменить `fn attach_network` целиком на:

```rust
    /// Gives the VM its endpoint and writes the adapter into `configuration`,
    /// returning the updated document and the endpoint the VM will start on.
    ///
    /// A VM that asked for no network is left off HNS entirely and reports no
    /// endpoint; the adapter an earlier start may have written is removed from
    /// its configuration.
    ///
    /// `recorded` is the endpoint to reuse or replace, which is not always the
    /// one the mapping was read with: a retry replaces the endpoint the failed
    /// attempt actually used.
    ///
    /// Neither the endpoint nor the recorded `endpoint_id` is undone when a
    /// later step fails: the endpoint outlives stops and lives until the VM is
    /// deleted, and dropping it after a failed start would hand the guest a new
    /// address on the next attempt.
    fn attach_network(
        &self,
        store: &MetadataStore,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
        configuration: String,
        recorded: Option<Uuid>,
        policy: EndpointPolicy,
    ) -> Result<(String, Option<Uuid>), RepositoryError> {
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
            return Ok((updated, None));
        }

        let adapter = (self.endpoint_provider)(&mapping.vm_name, recorded, policy)?;
        if recorded != Some(adapter.endpoint_id) {
            store.insert(VmComputeSystemMapping {
                endpoint_id: Some(adapter.endpoint_id),
                ..mapping.clone()
            })?;
        }

        let updated = hcs_config::apply_network_adapter(
            &configuration,
            adapter.endpoint_id,
            &adapter.mac_address,
        )?;
        if updated != configuration {
            self.write_configuration(mapping, vm_directory, &updated)?;
        }

        log::info!(
            "VM \"{}\" ({}) starts on endpoint {}",
            mapping.vm_name,
            mapping.vm_id,
            adapter.endpoint_id
        );
        Ok((updated, Some(adapter.endpoint_id)))
    }
```

- [ ] **Step 4: Дать `start` один повтор**

В `crates/platform/src/start.rs` заменить тело `pub fn start` после блока `log::info!("starting VM ...")` на:

```rust
        let stored = self.read_configuration(&mapping, vm_directory)?;
        let (configuration, endpoint) = self.attach_network(
            store,
            &mapping,
            vm_directory,
            stored.clone(),
            mapping.endpoint_id,
            EndpointPolicy::Reuse,
        )?;
        self.grant_access_to_attachments(&mapping, &configuration)?;

        let failure =
            match (self.system_starter)(&mapping.hcs_compute_system_id, &configuration) {
                Ok(()) => {
                    log::info!("started VM \"{}\" ({})", mapping.vm_name, mapping.vm_id);
                    return Ok(());
                }
                Err(failure) => failure,
            };

        // Only one failure is recoverable, and only for a VM that has an
        // endpoint to replace.
        let busy = match failure {
            HcsStartFailure::EndpointBusy(error) => error,
            HcsStartFailure::Failed(error) => {
                log::error!("failed to start VM \"{}\": {error}", mapping.vm_name);
                return Err(error);
            }
        };
        let Some(endpoint) = endpoint else {
            log::error!("failed to start VM \"{}\": {busy}", mapping.vm_name);
            return Err(busy);
        };

        log::warn!(
            "VM \"{}\" could not start because HNS still has endpoint {endpoint} attached to a \
             compute system that no longer exists: {busy}; replacing the endpoint and retrying \
             the start once",
            mapping.vm_name
        );

        let (configuration, _) = self.attach_network(
            store,
            &mapping,
            vm_directory,
            stored,
            Some(endpoint),
            EndpointPolicy::Replace,
        )?;
        self.grant_access_to_attachments(&mapping, &configuration)?;
        (self.system_starter)(&mapping.hcs_compute_system_id, &configuration).map_err(
            |failure| {
                let error = failure.into_error();
                log::error!("failed to start VM \"{}\": {error}", mapping.vm_name);
                error
            },
        )?;

        log::info!("started VM \"{}\" ({})", mapping.vm_name, mapping.vm_id);
        Ok(())
```

- [ ] **Step 5: Научить продакшн-провайдер заменять endpoint**

В `crates/platform/src/start.rs` заменить `fn ensure_endpoint` целиком на:

```rust
/// Resolves the endpoint VM `vm_name` starts on.
///
/// `recorded` is the identifier the VM's mapping remembers, if any.
///
/// Under [`EndpointPolicy::Reuse`] the recorded endpoint is opened rather than
/// trusted: one deleted outside VMLord, or lost to an HNS reset, is replaced by
/// a new one instead of failing the start. That hands the guest a different
/// address, but the alternative is a VM that can no longer start at all.
///
/// Under [`EndpointPolicy::Replace`] the recorded endpoint is deleted and a new
/// one created in its place, because HNS still has it attached to a compute
/// system that no longer exists. The address of the old endpoint is read first
/// and asked for again, so the guest keeps the address it had.
fn ensure_endpoint(
    vm_name: &str,
    recorded: Option<Uuid>,
    policy: EndpointPolicy,
) -> Result<VmNetworkAdapter, RepositoryError> {
    // The network first: an endpoint cannot be created outside one, and an
    // installation that has never had a VM on the network has none yet.
    let network = HcnNetwork::ensure()?;

    let existing = match recorded {
        Some(id) => HcnEndpoint::open_if_present(id)?.map(|endpoint| (id, endpoint)),
        None => None,
    };

    let (endpoint_id, endpoint) = match (policy, existing) {
        (EndpointPolicy::Reuse, Some(existing)) => existing,
        (EndpointPolicy::Reuse, None) => {
            if let Some(id) = recorded {
                log::warn!(
                    "HNS no longer knows endpoint {id} of VM \"{vm_name}\"; \
                     creating a new one, which changes the address the guest is offered"
                );
            }
            let id = Uuid::new_v4();
            (id, HcnEndpoint::create(&network, id, vm_name)?)
        }
        (EndpointPolicy::Replace, existing) => {
            let address = replaced_address(vm_name, existing.as_ref())?;
            if let Some(id) = recorded {
                HcnEndpoint::delete(id)?;
            }
            let id = Uuid::new_v4();
            log::info!(
                "replacing the occupied endpoint of VM \"{vm_name}\" with {id}{}",
                match &address {
                    Some(address) => format!(" on {}", address.ip_address),
                    None => String::new(),
                }
            );
            (
                id,
                HcnEndpoint::create_with_address(&network, id, vm_name, address.as_ref())?,
            )
        }
    };

    Ok(VmNetworkAdapter {
        endpoint_id,
        mac_address: endpoint.mac_address()?,
    })
}

/// The address a replacement endpoint should ask for.
///
/// `None` when HNS no longer has the old endpoint or reports no address for it:
/// the guest then gets whatever the network's IPAM assigns, which is worse than
/// keeping its address but far better than not starting.
fn replaced_address(
    vm_name: &str,
    existing: Option<&(Uuid, HcnEndpoint)>,
) -> Result<Option<EndpointAddress>, RepositoryError> {
    let Some((id, endpoint)) = existing else {
        log::warn!(
            "HNS no longer knows the occupied endpoint of VM \"{vm_name}\"; \
             its replacement is created without an address of its own"
        );
        return Ok(None);
    };

    let address = endpoint.address()?;
    if address.is_none() {
        log::warn!(
            "HNS reports no address for endpoint {id} of VM \"{vm_name}\"; \
             its replacement cannot ask for the old one, so the guest is offered a new address"
        );
    }
    Ok(address)
}
```

- [ ] **Step 6: Убедиться, что тесты проходят**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform`
Expected: PASS.

- [ ] **Step 7: Проверить clippy и закоммитить**

```bash
cargo clippy --target=x86_64-pc-windows-gnu --all-targets
git add crates/platform/src/start.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-46: Replace an occupied endpoint and retry the start once"
```

---

### Task 6: Регрессия на живом Hyper-V и документация

**Files:**
- Modify: `crates/platform/tests/hyperv.rs` (новый `#[ignore]`-тест, импорты)
- Modify: `ARCHITECTURE.md:291-295`

**Interfaces:**
- Consumes: всё, что построили Task 1–5; публичные `HcnEndpoint`, `EndpointAddress`, `MetadataStore`, `HcsVmRepository`

- [ ] **Step 1: Добавить `#[ignore]`-регрессию**

В `crates/platform/tests/hyperv.rs` заменить строку импорта `HcnEndpoint, HcnNetwork, ...` так, чтобы она включала `EndpointAddress`:

```rust
use vmlord_platform::{
    EndpointAddress, HcnEndpoint, HcnNetwork, HcsClient, HcsOperation, HcsSystem, HcsSystemState,
    HcsVmRepository, MetadataStore, ReconnectOutcome, VMLORD_NETWORK_ID, VmComputeSystemMapping,
    VmCreationPipeline, VmDeletionPipeline, VmEventSink, VmForceStopPipeline, VmShutdownPipeline,
    VmStartPipeline, list_known_vms, open_by_vm_id, open_by_vm_name, reconnect_known_vms,
};
```

Добавить тест в конец файла:

```rust
/// Exercises TASK-46 against a real Hyper-V host: the cycle that used to fail.
///
/// Before this task, a forced stop destroyed the compute system with the
/// adapter still attached, HNS kept the endpoint bound to it, and the next
/// start failed with `HCN_E_ENDPOINT_ALREADY_ATTACHED` (0x803B0014). The detach
/// before the termination is what makes the second start work; the endpoint and
/// its address surviving it is what makes the guest reachable where it was.
///
/// Set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact a_forcibly_stopped_vm_starts_again_on_the_same_endpoint --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and VMLORD_TEST_IMAGE_PATH set"]
fn a_forcibly_stopped_vm_starts_again_on_the_same_endpoint() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root =
        std::env::temp_dir().join(format!("vmlord-hns-restart-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-restart-test-{}", std::process::id()),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
        username: "admin".into(),
        password: "not used by this test".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let vm_name = request.name.clone();
    let store = MetadataStore::new(root.join("vm-mapping.json"));

    let mut repository = HcsVmRepository::new(root.clone());
    repository
        .initialize()
        .expect("the HCS backend should initialize on a live host");
    repository
        .create_vm(request)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    repository
        .start_vm(&vm_name)
        .expect("a NAT VM must start on a freshly created endpoint");

    let first = endpoint_of(&store, &vm_name);
    let first_address = address_of(first);

    repository
        .force_stop_vm(&vm_name)
        .expect("a running VM must accept a forced stop");

    let restarted = repository.start_vm(&vm_name);
    let second = endpoint_of(&store, &vm_name);
    let second_address = address_of(second);

    let _ = repository.delete_vm(vmlord_core::VmDeleteRequest {
        name: vm_name.clone(),
        delete_disks: true,
    });
    let _ = fs::remove_dir_all(&root);

    restarted.expect(
        "the second start must not fail with HCN_E_ENDPOINT_ALREADY_ATTACHED (0x803B0014)",
    );
    assert_eq!(
        first, second,
        "a forced stop must leave the endpoint in place for the next start"
    );
    assert_eq!(
        first_address, second_address,
        "the guest must be offered the address it had before the forced stop"
    );
}

/// The endpoint the store records for `vm_name`, which must exist by the time
/// a NAT VM has started.
fn endpoint_of(store: &MetadataStore, vm_name: &str) -> Uuid {
    store
        .find_by_vm_name(vm_name)
        .expect("the mapping store should be readable")
        .expect("a started VM must stay known to the store")
        .endpoint_id
        .expect("a started NAT VM must have a recorded endpoint")
}

/// The address HNS reports for `endpoint`.
fn address_of(endpoint: Uuid) -> Option<EndpointAddress> {
    HcnEndpoint::open(endpoint)
        .expect("HNS should still have the endpoint of a known VM")
        .address()
        .expect("HNS should answer a properties query for an endpoint it has")
}
```

- [ ] **Step 2: Проверить, что тест компилируется и не запускается по умолчанию**

Run: `cargo test --target x86_64-pc-windows-gnu -p vmlord-platform --test hyperv`
Expected: сборка проходит, все тесты помечены `ignored`.

`VmDeleteRequest` и `VmRepository` объявлены в `crates/core/src/lib.rs:65` и `crates/core/src/lib.rs:160`; `vmlord_core` уже в зависимостях теста, но `VmDeleteRequest` в его `use` не импортирован — в тесте он назван полным путём намеренно, чтобы не трогать общий блок импортов.

- [ ] **Step 3: Обновить ARCHITECTURE.md**

Заменить абзац на строках 291–295 (начинается с «Whether HCS still delivers `SystemExited`…») на:

```markdown
The endpoint has to come off the VM before the VM is destroyed.
`platform::VmForceStopPipeline` therefore hot-detaches the adapter through
`HcsModifyComputeSystem` -- `RequestType: "Remove"` against
`VirtualMachine/Devices/NetworkAdapters/<endpoint id>` -- before it terminates
the compute system. HNS keeps an endpoint attached to the compute system it was
handed to even after HCS has destroyed that system, so a termination with the
adapter in place leaves the endpoint occupied and the next start fails with
`HCN_E_ENDPOINT_ALREADY_ATTACHED` (0x803B0014). The resource path is built from
`hcs_config::adapter_key`, the same function that keys the section in
`config.json`: a spelling that drifted between them would detach nothing while
HCS still reported success.

A detach that fails does not keep the VM running. A forced stop is the last way
to stop a wedged VM, so it terminates anyway and reports the failed detach as a
warning naming its consequence. `platform::VmShutdownPipeline` detaches nothing
at all: `HcsShutDownComputeSystem` returns once the request reaches the guest,
not once the guest is down, so there is no moment at which the guest is still
running and no longer needs its network -- and a guest that refuses to shut down
would be left running without one. The legacy AppSandbox backend made the same
choice for the same reason.

What is left over is recovered on the next start. A guest that powers itself
off, a crash, or a VMLord restart leaves no compute system to detach from, so
`platform::VmStartPipeline` recognises `HCN_E_ENDPOINT_ALREADY_ATTACHED` --
from either the re-creation or the start -- and retries exactly once with a
replaced endpoint: it reads the occupied endpoint's address, deletes it, and
creates a new one asking for that same address. This is the one place VMLord
names a guest address, and it names one HNS assigned rather than one it chose,
so HNS's IPAM remains the sole allocator. A second occupied endpoint fails the
start: one replacement is a recovery, a loop of them would create an endpoint
per attempt. When the old address cannot be read, the replacement is created
without one and the guest is warned that its address changed.

AppSandbox's `hcs_detach_network` is not the precedent it looks like: the
function exists but is never called, and its comment -- that a detach is what
lets HCS deliver `SystemExited` -- is an untested hypothesis. AppSandbox avoids
the collision by never reusing an endpoint at all: it creates one per start,
deletes it on every stop, and keeps addresses stable by requesting a static IP.
VMLord keeps its endpoints instead, so it has to release them explicitly.
```

- [ ] **Step 4: Проверить сборку, тесты и clippy целиком**

```bash
cargo build --target=x86_64-pc-windows-gnu
cargo test --target x86_64-pc-windows-gnu
cargo clippy --target=x86_64-pc-windows-gnu --all-targets
```
Expected: сборка без ошибок, все не-`ignore` тесты зелёные, clippy молчит.

- [ ] **Step 5: Закоммитить**

```bash
git add crates/platform/tests/hyperv.rs ARCHITECTURE.md
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-46: Document the endpoint lifecycle and cover the restart cycle"
```

---

## После плана

Ручная parity-проверка на Hyper-V — за владельцем проекта:
`cargo test -p vmlord-platform --test hyperv -- --ignored --exact a_forcibly_stopped_vm_starts_again_on_the_same_endpoint --nocapture`

Merge request открывается только по явному разрешению владельца, с назначением на `mrundead` и запросом ревью у `mrundead`.
