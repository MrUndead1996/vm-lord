# Seed in the Creation Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Научить `VmCreationPipeline` создавать VM из cloud-образа: системный диск из скачанного образа, `seed.iso` рядом с `config.json` и вторым SCSI-attachment'ом, пара ключей и хеш пароля по месту, всё под существующим rollback'ом.

**Architecture:** Склейка «резолв → скачивание → qcow2» становится одной функцией `vmlord_image::open_cloud_image`. `platform` получает `vmlord-seed` обычной зависимостью и один новый boxed-шов `CloudDiskImporter`; продакшн-замыкание поверх `open_cloud_image` + `import_image` собирает композиционный корень `crates/vmlord`, куда и заезжает `vmlord-image`. `hcs_config` строит список attachment'ов от источника: `LocalMedia` → установочный ISO, `CloudImage` → `seed.iso`.

**Tech Stack:** Rust 2024, workspace `vmlord`, `log`, `serde_json`, `uuid`. Новых внешних зависимостей нет — только новые рёбра между крейтами workspace.

**Спека:** `docs/superpowers/specs/2026-08-10-seed-in-creation-pipeline-design.md`

## Global Constraints

- Ветка задачи: `task-61-seed-in-creation-pipeline` (уже создана от `main`, спека закоммичена).
- Префикс каждого коммита — `TASK-61: `, сообщение на английском, в конце `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
- Комментарии и документация в коде — на английском, объясняют «почему», а не «что».
- Логирование только через фасад `log`, уровни DEBUG..ERROR. `TRACE` не используется.
- В лог, в `config.json` и в метаданные не попадают ни открытый пароль, ни `$6$`-хеш, ни приватный ключ.
- `unsafe` запрещён везде, кроме `crates/platform` и `crates/legacy-backend`; новый код в `platform` его не вводит.
- `crates/platform` собирается только под Windows (`compile_error!` на прочих). Его тесты **не запускаются в WSL**: проверка — `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --no-run` (собирает тестовые бинарники) и `cargo clippy -p vmlord-platform --target x86_64-pc-windows-gnu --all-targets`. Фактический прогон — на Windows, за владельцем проекта.
- Портируемые крейты (`core`, `image`, `keys`, `seed`) тестируются на хосте: `cargo test -p vmlord-image` и т.п.
- Сборка проекта целиком — `cargo build --target=x86_64-pc-windows-gnu`.
- `vmlord-image` не становится обычной зависимостью `platform`: в `platform` он остаётся dev-зависимостью, как сейчас.

---

### Task 1: `open_cloud_image` — склейка резолва, скачивания и qcow2

Три написанных куска `vmlord-image` нигде не соединены. Соединение живёт в самом `image`, а не в композиционном корне: корень — место сборки зависимостей, а не логики, и тестировать его нечем (`crates/vmlord` — бинарь с `test = false`).

**Files:**
- Create: `crates/image/src/open.rs`
- Modify: `crates/image/src/lib.rs` (объявление и реэкспорт модуля)
- Modify: `crates/image/tests/support/mod.rs` (сервер, отвечающий разными телами на разные пути)
- Test: `crates/image/tests/open.rs`

**Interfaces:**
- Consumes: `resolve_image(&DistroProfile, &str) -> Result<ResolvedImage, ResolveError>`, `fetch_image(ImageDownloadRequest, &ProgressPublisher, &AtomicBool) -> Result<PathBuf, DownloadError>`, `Qcow2Image::open(&Path, u64) -> Result<Qcow2Image, Qcow2Error>`.
- Produces: `vmlord_image::open_cloud_image(profile: &DistroProfile, release: &str, cache_directory: &Path, capacity: u64, progress: &ProgressPublisher, cancel: &AtomicBool) -> Result<Qcow2Image, RepositoryError>`; в `tests/support` — `TestServer::start_directory(files: Vec<(String, Vec<u8>)>) -> TestServer`.

- [ ] **Step 1: Научить тестовый сервер отвечать по путям**

Сейчас `TestServer` отдаёт одно тело на любой путь, а `open_cloud_image` за один вызов просит два разных файла — `SHA256SUMS` и сам образ. В `crates/image/tests/support/mod.rs` добавить рядом с `start`:

```rust
/// Serves a directory: each request is answered with the file whose name it
/// asks for, and with 404 when there is no such file.
///
/// `start` above answers every path with one body, which is enough for a
/// resolver or a download on its own but not for `open_cloud_image`, which
/// fetches the checksum list and the image it names in one call.
pub fn start_directory(files: Vec<(String, Vec<u8>)>) -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").expect("the loopback port should bind");
    let url = format!(
        "http://{}/noble-cloudimg-amd64.img",
        listener.local_addr().unwrap()
    );
    let base_url = format!("http://{}/", listener.local_addr().unwrap());
    let ranges = Arc::new(Mutex::new(Vec::new()));

    let recorded = Arc::clone(&ranges);
    thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let (range, path) = read_request(&stream);
            let file = files
                .iter()
                .find(|(name, _)| path.rsplit('/').next() == Some(name.as_str()));
            match file {
                Some((_, body)) => answer(stream, body, Behaviour::IgnoresRange),
                None => answer(stream, &[], Behaviour::NotFound),
            };
            recorded.lock().unwrap().push(range);
        }
    });

    Self { url, base_url, ranges }
}
```

И заменить `read_range_header` на функцию, которая читает и путь, и заголовок, сохранив прежнего вызывающего:

```rust
/// Reads the request, returning its `range` header and its path.
///
/// Header names are matched case-insensitively because `ureq` sends them
/// lowercase (`range: bytes=1000-`). A server looking for `Range: ` would
/// silently answer 200 to every resume, and the resume test would pass while
/// testing nothing.
fn read_request(stream: &TcpStream) -> (Option<String>, String) {
    let mut reader = BufReader::new(stream.try_clone().expect("the stream should clone"));
    let mut range = None;
    let mut path = String::new();
    let mut first = true;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line == "\r\n" {
            break;
        }
        if first {
            first = false;
            path = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("range")
        {
            range = Some(value.trim().to_owned());
        }
    }
    (range, path)
}

fn read_range_header(stream: &TcpStream) -> Option<String> {
    read_request(stream).0
}
```

- [ ] **Step 2: Написать падающий тест**

Создать `crates/image/tests/open.rs`:

```rust
//! Fetching the image a release means and opening it as a disk, in one call.

mod support;

use std::{fs, path::PathBuf, sync::atomic::AtomicBool};

use sha2::{Digest, Sha256};
use support::TestServer;
use vmlord_core::ProgressPublisher;
use vmlord_image::{DistroProfile, open_cloud_image, ubuntu};

/// Bigger than the fixture's disk, so capacity is not what is under test.
const CAPACITY: u64 = 1024 * 1024;

fn fixture() -> Vec<u8> {
    fs::read(
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/qcow2"))
            .join("sparse.qcow2"),
    )
    .expect("the qcow2 fixture should be readable")
}

/// The checksum list the server publishes, computed from the fixture itself so
/// the two cannot drift apart.
fn checksums(image: &[u8], file_name: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(image);
    let sum: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{sum} *{file_name}\n").into_bytes()
}

fn cache_directory(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("vmlord-open-{tag}-{unique}"));
    fs::create_dir_all(&path).expect("the cache directory should be created");
    path
}

/// A profile pointing at the loopback server instead of the internet.
fn profile_for(server: &TestServer) -> DistroProfile {
    DistroProfile {
        directory_template: format!("{}{{release}}/", server.base_url()),
        ..ubuntu()
    }
}

fn served(image: &[u8]) -> Vec<(String, Vec<u8>)> {
    let file_name = ubuntu().file_name("24.04");
    vec![
        ("SHA256SUMS".to_owned(), checksums(image, &file_name)),
        (file_name, image.to_vec()),
    ]
}

#[test]
fn a_release_becomes_an_open_disk() {
    let image = fixture();
    let server = TestServer::start_directory(served(&image));
    let directory = cache_directory("open");

    let opened = open_cloud_image(
        &profile_for(&server),
        "24.04",
        &directory,
        CAPACITY,
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect("the server publishes this release");

    assert_eq!(opened.virtual_size(), 64 * 1024 + 512);
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn a_release_the_server_does_not_publish_is_reported_by_name() {
    let server = TestServer::start_directory(Vec::new());
    let directory = cache_directory("missing");

    let error = open_cloud_image(
        &profile_for(&server),
        "24.04",
        &directory,
        CAPACITY,
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("there is no checksum list to read");

    assert!(error.to_string().contains("404"), "got {error}");
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn an_image_that_does_not_hash_to_what_the_list_says_is_refused() {
    let image = fixture();
    let mut served = served(&image);
    // The list stays as it is; the body served under that name changes.
    served[1].1 = vec![0; image.len()];
    let server = TestServer::start_directory(served);
    let directory = cache_directory("mismatch");

    let error = open_cloud_image(
        &profile_for(&server),
        "24.04",
        &directory,
        CAPACITY,
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("the body is not the image the list names");

    assert!(error.to_string().contains("hashes to"), "got {error}");
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn an_image_too_big_for_the_disk_is_refused_before_a_byte_is_copied() {
    let image = fixture();
    let server = TestServer::start_directory(served(&image));
    let directory = cache_directory("capacity");

    let error = open_cloud_image(
        &profile_for(&server),
        "24.04",
        &directory,
        64 * 1024,
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("the image's disk does not fit in 64 KiB");

    assert!(error.to_string().contains("64"), "got {error}");
    fs::remove_dir_all(&directory).unwrap();
}
```

- [ ] **Step 3: Убедиться, что тест падает**

Run: `cargo test -p vmlord-image --test open`
Expected: FAIL — `cannot find function open_cloud_image in crate vmlord_image`.

- [ ] **Step 4: Написать `open_cloud_image`**

Создать `crates/image/src/open.rs`:

```rust
//! Getting from "Ubuntu 24.04" to a disk that can be read cluster by cluster.
//!
//! Three steps that are written separately and belong together: read the
//! checksum list to learn which file a release means, fetch that file into the
//! cache, open it as the guest's disk. Joining them here rather than in the
//! composition root keeps the root a place where dependencies are assembled and
//! leaves this testable against the fixture server the other tests already use.

use std::{path::Path, sync::atomic::AtomicBool};

use vmlord_core::{DistroProfile, ProgressPublisher, RepositoryError};

use crate::{
    download::{ImageDownloadRequest, fetch_image},
    qcow2::Qcow2Image,
    resolve::resolve_image,
};

/// Fetches the image `release` means and opens it as the guest's disk.
///
/// `capacity` is the size of the VM's disk, not of the image: opening refuses
/// an image whose disk would not fit, before a byte is copied anywhere.
///
/// `progress` and `cancel` are passed through to the download, which is the
/// only step long enough to have either. #61 hands in a publisher nobody reads
/// and a flag nobody sets; #64 hands in the real ones without this signature
/// changing.
///
/// The typed errors of the three steps end here: the caller is the creation
/// pipeline, whose contract across the project is `RepositoryError`. Nothing is
/// lost -- each error's `Display` names its own cause, and the crate logs it
/// where it happens.
pub fn open_cloud_image(
    profile: &DistroProfile,
    release: &str,
    cache_directory: &Path,
    capacity: u64,
    progress: &ProgressPublisher,
    cancel: &AtomicBool,
) -> Result<Qcow2Image, RepositoryError> {
    log::debug!(
        "preparing {} {release} for a {capacity}-byte disk, cached in {}",
        profile.name,
        cache_directory.display()
    );

    let resolved = resolve_image(profile, release).map_err(at_the_boundary)?;
    let path = fetch_image(
        ImageDownloadRequest {
            url: &resolved.url,
            expected_sha256: &resolved.sha256,
            cache_directory,
        },
        progress,
        cancel,
    )
    .map_err(at_the_boundary)?;

    let image = Qcow2Image::open(&path, capacity).map_err(at_the_boundary)?;
    log::debug!(
        "opened {} as a {}-byte disk",
        path.display(),
        image.virtual_size()
    );
    Ok(image)
}

fn at_the_boundary(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::new(error.to_string())
}
```

В `crates/image/src/lib.rs` объявить модуль (алфавитный порядок, между `mod http;` и `mod part;`):

```rust
mod open;
```

и реэкспортировать рядом с остальными:

```rust
pub use open::open_cloud_image;
```

- [ ] **Step 5: Убедиться, что тесты проходят**

Run: `cargo test -p vmlord-image`
Expected: PASS, включая прежние тесты `resolve`, `download` и `resume` — правка сервера не должна их сдвинуть.

- [ ] **Step 6: Проверить линты**

Run: `cargo clippy -p vmlord-image --all-targets`
Expected: без предупреждений.

- [ ] **Step 7: Коммит**

```bash
git add crates/image/src/open.rs crates/image/src/lib.rs crates/image/tests/open.rs crates/image/tests/support/mod.rs
git commit -m "$(cat <<'EOF'
TASK-61: Open a release as the guest's disk in one call

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Место seed'а на диске и в HCS-документе

Путь и attachment'ы — то, что должно быть верным до того, как появится хоть один байт seed'а: документ строится раньше каталога VM, и ошибка здесь ломает VM молча, а не отказом.

**Files:**
- Modify: `crates/platform/src/layout.rs`
- Modify: `crates/platform/src/hcs_config.rs`
- Modify: `crates/platform/src/create.rs:88-92,129-144` (вызовы `build` и `local_media_path`)
- Test: тесты внутри `crates/platform/src/layout.rs` и `crates/platform/src/hcs_config.rs`

**Interfaces:**
- Consumes: `layout::vm_directory`, `VmSource`.
- Produces: `layout::seed_path(&Path) -> PathBuf`; `hcs_config::media_path<'a>(&'a VmCreateRequest, &'a Path) -> &'a Path`; `HcsVmConfigBuilder::build(&VmCreateRequest, &Path, &Path) -> Result<String, RepositoryError>` (третий аргумент — путь seed'а). `hcs_config::local_media_path` удаляется.

- [ ] **Step 1: Написать падающий тест на путь seed'а**

В `crates/platform/src/layout.rs`, в `mod tests`, рядом с `a_vms_key_pair_lives_beside_its_disks`:

```rust
#[test]
fn the_seed_lives_beside_the_configuration_not_among_the_disks() {
    // Not under `disks/`: the seed is a configuration medium, and `disks/` is
    // what `delete_vm` removes when asked to remove a VM's disks.
    let directory = vm_directory(Path::new("/vms"), "dev-linux").unwrap();

    assert_eq!(
        seed_path(&directory),
        PathBuf::from("/vms").join("dev-linux").join("seed.iso")
    );
}
```

и добавить `seed_path` в список импортов теста.

- [ ] **Step 2: Убедиться, что тест не собирается**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --no-run`
Expected: FAIL — `cannot find function seed_path in this scope`.

- [ ] **Step 3: Добавить `seed_path`**

В `crates/platform/src/layout.rs`, после `system_disk_path`:

```rust
/// Returns the path of the NoCloud seed the guest's cloud-init reads.
///
/// Beside `config.json` rather than under `disks/`: this is a configuration
/// medium, not one of the VM's disks, and `disks/` is what a deletion removes
/// when it is told to remove the VM's disks.
pub(crate) fn seed_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("seed.iso")
}
```

- [ ] **Step 4: Написать падающие тесты на attachment'ы**

В `crates/platform/src/hcs_config.rs`, в `mod tests`: удалить тест `a_cloud_image_is_refused_with_the_task_that_will_support_it` целиком (запрет снимается этой задачей) и добавить вместо него:

```rust
fn cloud_request() -> VmCreateRequest {
    VmCreateRequest {
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
    }
}

#[test]
fn a_cloud_vm_boots_its_disk_with_the_seed_beside_it() {
    // Two attachments, not three: a cloud image has no installer ISO, and a
    // numbering hole reserved for one would have to be explained to everybody
    // who opens `config.json`.
    let system_disk_path = PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx");
    let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");

    let json: Value = serde_json::from_str(
        &HcsVmConfigBuilder::build(&cloud_request(), &system_disk_path, &seed_path).unwrap(),
    )
    .unwrap();

    assert_eq!(
        json.pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments"),
        Some(&json!({
            "0": { "Type": "VirtualDisk", "Path": system_disk_path },
            "1": { "Type": "Iso", "Path": seed_path }
        }))
    );
}

#[test]
fn a_cloud_vms_document_carries_no_secret_of_its_provisioning() {
    let document = HcsVmConfigBuilder::build(
        &cloud_request(),
        &PathBuf::from("C:\\vms\\test-vm\\disks\\system.vhdx"),
        &PathBuf::from("C:\\vms\\test-vm\\seed.iso"),
    )
    .unwrap();

    // The password travels to the guest inside the seed volume alone. Anyone
    // who can read the compute system's configuration must learn nothing.
    assert!(!document.contains("secret"), "got {document}");
    assert!(!document.contains("$6$"), "got {document}");
    assert!(!document.contains("user"), "got {document}");
}

#[test]
fn the_media_a_vm_boots_is_its_installer_or_its_seed() {
    let seed_path = PathBuf::from("C:\\vms\\test-vm\\seed.iso");

    assert_eq!(
        media_path(&request(), &seed_path),
        Path::new("C:\\images\\installer.iso")
    );
    assert_eq!(media_path(&cloud_request(), &seed_path), seed_path);
}
```

Импорты теста дополнить `std::path::Path` и `super::media_path`.

- [ ] **Step 5: Убедиться, что тесты не собираются**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --no-run`
Expected: FAIL — `media_path` не найден, `build` принимает два аргумента.

- [ ] **Step 6: Строить attachment'ы от источника**

В `crates/platform/src/hcs_config.rs` заменить `local_media_path` на:

```rust
/// The ISO the VM boots with: the installer for local media, the seed for a
/// cloud image.
///
/// One place decides it, because two need it: the configuration document below
/// and the pipeline, which grants the VM access to the same file.
pub(crate) fn media_path<'a>(request: &'a VmCreateRequest, seed_path: &'a Path) -> &'a Path {
    match &request.source {
        VmSource::LocalMedia { path } => Path::new(path),
        VmSource::CloudImage { .. } => seed_path,
    }
}
```

Правку `build`: сигнатура и построение карты.

```rust
    pub(crate) fn build(
        request: &VmCreateRequest,
        system_disk_path: &Path,
        seed_path: &Path,
    ) -> Result<String, RepositoryError> {
        request.validate()?;

        if request.gpu_mode != GpuMode::None {
            return Err(RepositoryError::new(format!(
                "HCS configuration does not support GPU mode: {:?}",
                request.gpu_mode
            )));
        }
        ensure_supported_network_mode(request.network_mode)?;

        let attachments = BTreeMap::from([
            (
                "0".to_string(),
                Attachment {
                    attachment_type: "VirtualDisk",
                    path: system_disk_path.to_path_buf(),
                },
            ),
            (
                "1".to_string(),
                Attachment {
                    attachment_type: "Iso",
                    path: media_path(request, seed_path).to_path_buf(),
                },
            ),
        ]);
```

Заодно обновить doc-комментарий `build`: второй attachment — установочный ISO для локального носителя и `seed.iso` для cloud-образа.

- [ ] **Step 7: Поправить остальные тесты `hcs_config`**

Все прочие вызовы `HcsVmConfigBuilder::build(&request(), &system_disk_path)` в `mod tests` получают третьим аргументом `&PathBuf::from("C:\\vms\\test-vm\\seed.iso")` (для `LocalMedia` он не используется, но должен быть передан). Затрагиваются `builds_the_minimal_configuration`, `serializes_ram_cpu_and_disk_path`, `omits_request_secrets`, `rejects_each_unsupported_gpu_mode`, `accepts_nat_without_writing_a_network_adapter`, `rejects_each_network_mode_that_waits_for_its_own_task`, `removes_the_network_adapter_section_and_nothing_else`, `removing_an_absent_network_adapter_returns_the_document_unchanged`, `rejects_an_invalid_request_before_serializing`, `reads_back_the_topology_it_built`, `applying_a_topology_changes_only_memory_and_processors`, `attaching_an_adapter_names_the_endpoint_and_its_mac_address`, `attaching_an_adapter_changes_nothing_else`, `attaching_the_same_adapter_twice_yields_the_same_document`, `the_adapter_key_is_how_the_section_names_the_adapter`.

- [ ] **Step 8: Поправить `create.rs` под новую сигнатуру**

Это ещё не поведение cloud-образа — только компиляция. В `crates/platform/src/create.rs`:

```rust
        let system_disk_path = layout::system_disk_path(vm_directory);
        let seed_path = layout::seed_path(vm_directory);
        // Rejects an unsupported request (name, GPU/network mode, ...) before
        // any filesystem or HCS side effect.
        let configuration = HcsVmConfigBuilder::build(request, &system_disk_path, &seed_path)?;
        let media_path = hcs_config::media_path(request, &seed_path).to_path_buf();
```

Импорт в шапке файла заменить на `hcs_config::{self, HcsVmConfigBuilder}`.

Внутри замыкания заменить проверку существования и грант:

```rust
            if matches!(request.source, VmSource::LocalMedia { .. }) && !media_path.is_file() {
                return Err(RepositoryError::new(format!(
                    "VM image no longer exists: {}",
                    media_path.display()
                )));
            }
```

```rust
            (self.access_granter)(&hcs_compute_system_id, &system_disk_path)?;
            (self.access_granter)(&hcs_compute_system_id, &media_path)?;
```

Импорт `VmSource` добавить в `use vmlord_core::{...}`.

- [ ] **Step 9: Собрать и проверить линты**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --no-run && cargo clippy -p vmlord-platform --target x86_64-pc-windows-gnu --all-targets`
Expected: собирается, без предупреждений. (Прогон тестов — на Windows.)

- [ ] **Step 10: Коммит**

```bash
git add crates/platform/src/layout.rs crates/platform/src/hcs_config.rs crates/platform/src/create.rs
git commit -m "$(cat <<'EOF'
TASK-61: Attach the seed a cloud VM boots beside its disk

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Шов импортёра системного диска

Отдельная задача, потому что она меняет сигнатуры трёх публичных точек и ни одного байта поведения: рецензент, которому не нравится форма шва, отвергает её, не разбираясь в провизионинге.

**Files:**
- Modify: `crates/platform/src/create.rs` (тип, поле, `production`, `for_test`)
- Modify: `crates/platform/src/lib.rs` (реэкспорт)
- Modify: `crates/platform/src/repository.rs:66-75` (`HcsVmRepository::new`)
- Modify: `crates/platform/src/repository.rs:673` (конструктор в тестах)
- Modify: `crates/platform/tests/hyperv.rs` (все вызовы `production()` и `HcsVmRepository::new`)
- Modify: `crates/vmlord/src/main.rs:55-57` (композиционный корень — временная заглушка до Task 5)
- Test: тесты внутри `crates/platform/src/create.rs`

**Interfaces:**
- Consumes: `layout::seed_path`, `hcs_config::media_path` из Task 2.
- Produces: `vmlord_platform::CloudDiskImporter = Box<dyn Fn(&CloudImage, u64, &Path) -> Result<(), RepositoryError>>`; `VmCreationPipeline::production(CloudDiskImporter)`; `HcsVmRepository::new(impl Into<PathBuf>, CloudDiskImporter)`; `VmCreationPipeline::for_test(vhd_creator, cloud_disk, access_granter, system_creator, system_teardown)`.

- [ ] **Step 1: Написать падающий тест на отказ по умолчанию**

В `crates/platform/src/create.rs`, в `mod tests`, добавить к `Calls` поле и тест. Сначала поле:

```rust
    #[derive(Clone, Default)]
    struct Calls {
        vhd: Arc<Mutex<Vec<(PathBuf, u64)>>>,
        cloud: Arc<Mutex<Vec<(String, u64, PathBuf)>>>,
        grant: Arc<Mutex<Vec<(String, PathBuf)>>>,
        create: Arc<Mutex<Vec<(String, String)>>>,
        teardown: Arc<Mutex<Vec<String>>>,
    }
```

затем тест:

```rust
    #[test]
    fn a_local_media_vm_never_reaches_the_cloud_image_importer() {
        let fixture = fixture("no-cloud");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false);

        pipeline
            .create(&fixture.store, &fixture.request, &fixture.vm_directory)
            .expect("creation should succeed");

        assert!(calls.cloud.lock().unwrap().is_empty());
        assert_eq!(calls.vhd.lock().unwrap().len(), 1);
    }
```

и научить фабрику `pipeline` записывать вызовы импортёра — вставить новым вторым замыканием, после `vhd_creator`:

```rust
            {
                let calls = calls.clone();
                move |image: &vmlord_core::CloudImage, size, path: &std::path::Path| {
                    calls.cloud.lock().unwrap().push((
                        image.release.clone(),
                        size,
                        path.to_path_buf(),
                    ));
                    if fail_cloud {
                        return Err(vmlord_core::RepositoryError::new(
                            "injected cloud image failure",
                        ));
                    }
                    fs::write(path, b"imported vhdx").map_err(|error| {
                        vmlord_core::RepositoryError::new(format!("import: {error}"))
                    })
                }
            },
```

Сигнатура фабрики становится `fn pipeline(calls: &Calls, fail_vhd: bool, fail_cloud: bool, fail_create: bool)`; все существующие вызовы `pipeline(&calls, X, Y)` получают `false` третьим аргументом: `pipeline(&calls, X, false, Y)`.

- [ ] **Step 2: Убедиться, что тест не собирается**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --no-run`
Expected: FAIL — `for_test` принимает четыре замыкания.

- [ ] **Step 3: Ввести тип и поле**

В `crates/platform/src/create.rs`, рядом с прочими типами швов:

```rust
type VhdCreator = Box<dyn Fn(&Path, u64) -> Result<(), RepositoryError>>;
type AccessGranter = Box<dyn Fn(&str, &Path) -> Result<(), RepositoryError>>;
type SystemCreator = Box<dyn Fn(&str, &str) -> Result<(), RepositoryError>>;

/// Makes the VM's system disk out of a cloud image: fetch the image the release
/// means, then write it into a VHDX at the given path, sized for the VM rather
/// than for the image.
///
/// Injected rather than called directly because the fetching half is not
/// Windows's business: it lives in `vmlord-image`, which knows no Windows API,
/// and the composition root joins the two. The pipeline keeps the half that is
/// Windows -- writing into a VHDX through the disk it is attached as.
pub type CloudDiskImporter =
    Box<dyn Fn(&CloudImage, u64, &Path) -> Result<(), RepositoryError>>;
```

Поле и конструкторы:

```rust
pub struct VmCreationPipeline {
    vhd_creator: VhdCreator,
    cloud_disk: CloudDiskImporter,
    access_granter: AccessGranter,
    system_creator: SystemCreator,
    system_teardown: SystemTeardown,
}

impl VmCreationPipeline {
    /// Creates a pipeline backed by the real VHDX and HCS APIs, importing cloud
    /// images through `cloud_disk`.
    ///
    /// The importer is required rather than optional: a pipeline that silently
    /// cannot build a VM from a cloud image is a state better left unspellable.
    #[must_use]
    pub fn production(cloud_disk: CloudDiskImporter) -> Self {
        Self {
            vhd_creator: Box::new(create_dynamic_vhdx),
            cloud_disk,
            access_granter: Box::new(grant_vm_access),
            system_creator: Box::new(create_hcs_system),
            system_teardown: Box::new(cleanup::teardown_compute_system),
        }
    }

    #[cfg(test)]
    fn for_test(
        vhd_creator: impl Fn(&Path, u64) -> Result<(), RepositoryError> + 'static,
        cloud_disk: impl Fn(&CloudImage, u64, &Path) -> Result<(), RepositoryError> + 'static,
        access_granter: impl Fn(&str, &Path) -> Result<(), RepositoryError> + 'static,
        system_creator: impl Fn(&str, &str) -> Result<(), RepositoryError> + 'static,
        system_teardown: impl Fn(&str) -> Result<(), RepositoryError> + 'static,
    ) -> Self {
        Self {
            vhd_creator: Box::new(vhd_creator),
            cloud_disk: Box::new(cloud_disk),
            access_granter: Box::new(access_granter),
            system_creator: Box::new(system_creator),
            system_teardown: Box::new(system_teardown),
        }
    }
```

Импорт `CloudImage` добавить в `use vmlord_core::{...}`. Удалить `impl Default for VmCreationPipeline` — конвейеру больше неоткуда взять импортёр по умолчанию, а `Default`, придумывающий отказ, лгал бы о своей готовности.

- [ ] **Step 4: Реэкспорт**

В `crates/platform/src/lib.rs`:

```rust
pub use create::{CloudDiskImporter, VmCreationPipeline};
```

- [ ] **Step 5: Провести шов через репозиторий**

В `crates/platform/src/repository.rs`:

```rust
    /// Creates a repository storing its VMs under `storage_root`, importing
    /// cloud images through `cloud_disk`.
    pub fn new(storage_root: impl Into<PathBuf>, cloud_disk: CloudDiskImporter) -> Self {
```

и в теле — `creation: VmCreationPipeline::production(cloud_disk),`. Импорт `CloudDiskImporter` добавить к остальным из `crate`.

Конструктор в тестах репозитория (`repository.rs:673`) получает отказывающее замыкание:

```rust
        HcsVmRepository::new(
            std::env::temp_dir().join("vmlord-repository-test"),
            Box::new(|_, _, _| {
                Err(RepositoryError::new("this test creates no VM from a cloud image"))
            }),
        )
```

- [ ] **Step 6: Поправить `tests/hyperv.rs`**

Добавить в файл общий помощник рядом с прочими:

```rust
/// The importer for tests that create VMs from local media only.
///
/// A cloud image would download hundreds of megabytes; the tests that need one
/// build their own importer.
fn no_cloud_images() -> vmlord_platform::CloudDiskImporter {
    Box::new(|_, _, _| {
        Err(vmlord_core::RepositoryError::new(
            "this test creates no VM from a cloud image",
        ))
    })
}
```

и передать его в каждый из двенадцати вызовов `VmCreationPipeline::production()` (строки 182, 235, 308, 363, 426, 488, 569, 765, 919, 1038, 1150, 1370) и в оба вызова `HcsVmRepository::new` (строки 108, 1249, 1452).

- [ ] **Step 7: Заглушка в композиционном корне**

В `crates/vmlord/src/main.rs` — временно, до Task 5:

```rust
        return Box::new(vmlord_platform::HcsVmRepository::new(
            settings.vm_storage_path.clone(),
            Box::new(|_, _, _| {
                Err(vmlord_core::RepositoryError::new(
                    "cloud images are not wired up yet",
                ))
            }),
        ));
```

- [ ] **Step 8: Собрать и проверить линты**

Run: `cargo build --target=x86_64-pc-windows-gnu && cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --no-run && cargo clippy --target x86_64-pc-windows-gnu --all-targets`
Expected: собирается, без предупреждений.

- [ ] **Step 9: Коммит**

```bash
git add crates/platform/src/create.rs crates/platform/src/lib.rs crates/platform/src/repository.rs crates/platform/tests/hyperv.rs crates/vmlord/src/main.rs
git commit -m "$(cat <<'EOF'
TASK-61: Give the pipeline a seam for cloud image disks

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Провизионинг внутри `create`

Сердце задачи: диск из образа, ключи, хеш пароля и seed на диске — всё под rollback'ом.

**Files:**
- Modify: `crates/platform/src/create.rs`
- Modify: `crates/platform/Cargo.toml` (зависимость `vmlord-seed`)
- Test: тесты внутри `crates/platform/src/create.rs`

**Interfaces:**
- Consumes: `CloudDiskImporter` (Task 3), `layout::seed_path` (Task 2), `vmlord_keys::generate`, `vm_key::write_key_pair`, `password_hash::hash_password`, `vmlord_seed::{SeedRequest, build, image}`.
- Produces: поведение `VmCreationPipeline::create` для `VmSource::CloudImage`.

- [ ] **Step 1: Зависимость**

В `crates/platform/Cargo.toml`, в `[dependencies]`, после `vmlord-keys`:

```toml
# The seed documents and their ISO image. The crate is pure -- no filesystem,
# no network, no Windows API -- and writing its bytes to disk is what this
# layer is for.
vmlord-seed = { path = "../seed" }
```

- [ ] **Step 2: Написать падающие тесты**

В `crates/platform/src/create.rs`, в `mod tests`, добавить cloud-фикстуру и тесты:

```rust
    fn cloud_request(name: &str) -> VmCreateRequest {
        VmCreateRequest {
            name: name.into(),
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: vmlord_core::ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "dev".into(),
                    password: Some(Password::new("secret")),
                    ssh: SshAccess::Enabled { deploy_key: true },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                },
            },
            ram_mb: 512,
            disk_gb: 1,
            cpu_cores: 1,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        }
    }

    /// The bytes of the seed volume the pipeline wrote.
    fn seed_bytes(vm_directory: &std::path::Path) -> String {
        String::from_utf8_lossy(&fs::read(vm_directory.join("seed.iso")).unwrap()).into_owned()
    }

    #[test]
    fn a_cloud_vm_gets_an_imported_disk_a_key_pair_and_a_seed() {
        let fixture = fixture("cloud-happy");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);
        let request = cloud_request("cloud-vm");

        let mapping = pipeline
            .create(&fixture.store, &request, &fixture.vm_directory)
            .expect("creation should succeed");

        // The disk comes from the importer, not from an empty VHDX.
        assert!(calls.vhd.lock().unwrap().is_empty());
        assert_eq!(
            calls.cloud.lock().unwrap().as_slice(),
            &[(
                "24.04".to_owned(),
                1024 * 1024 * 1024,
                fixture.vm_directory.join("disks").join("system.vhdx")
            )]
        );

        assert!(fixture.vm_directory.join("seed.iso").is_file());
        assert!(
            fixture
                .vm_directory
                .join("keys")
                .join("id_ed25519")
                .is_file()
        );
        let public_key = fs::read_to_string(
            fixture.vm_directory.join("keys").join("id_ed25519.pub"),
        )
        .unwrap();

        // What the guest is told is in the seed, and only there.
        let seed = seed_bytes(&fixture.vm_directory);
        assert!(seed.contains("$6$"), "the seed carries the password hash");
        assert!(seed.contains(public_key.trim_end()), "and the public key");
        assert!(seed.contains("instance-id: 'vmlord-"));

        let stored = fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap();
        let create_calls = calls.create.lock().unwrap();
        for document in [&create_calls[0].1, &stored] {
            assert!(!document.contains("secret"), "got {document}");
            assert!(!document.contains("$6$"), "got {document}");
        }
        drop(create_calls);

        // The VM must be able to open the seed, exactly as it opens its disk.
        assert_eq!(
            calls.grant.lock().unwrap().as_slice(),
            &[
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.vm_directory.join("disks").join("system.vhdx")
                ),
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.vm_directory.join("seed.iso")
                ),
            ]
        );
    }

    #[test]
    fn a_key_only_cloud_vm_leaves_no_password_hash_anywhere() {
        let fixture = fixture("cloud-key-only");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);
        let request = VmCreateRequest {
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
            ..cloud_request("cloud-key-only-vm")
        };

        pipeline
            .create(&fixture.store, &request, &fixture.vm_directory)
            .expect("a key-only login is a valid VM");

        assert!(!seed_bytes(&fixture.vm_directory).contains("$6$"));
    }

    #[test]
    fn a_cloud_vm_without_ssh_gets_no_key_pair() {
        let fixture = fixture("cloud-no-ssh");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);
        let request = VmCreateRequest {
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: vmlord_core::ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "dev".into(),
                    password: Some(Password::new("secret")),
                    ssh: SshAccess::Disabled,
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                },
            },
            ..cloud_request("cloud-no-ssh-vm")
        };

        pipeline
            .create(&fixture.store, &request, &fixture.vm_directory)
            .expect("a password-only VM is a valid VM");

        assert!(!fixture.vm_directory.join("keys").exists());
        assert!(fixture.vm_directory.join("seed.iso").is_file());
    }

    #[test]
    fn a_failed_import_leaves_neither_seed_nor_keys_behind() {
        let fixture = fixture("cloud-import-failure");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, true, false);

        let error = pipeline
            .create(
                &fixture.store,
                &cloud_request("cloud-doomed"),
                &fixture.vm_directory,
            )
            .expect_err("an import failure must abort creation");

        assert!(error.to_string().contains("injected cloud image failure"));
        assert!(!fixture.vm_directory.exists());
        assert!(calls.create.lock().unwrap().is_empty());
        assert!(fixture.store.list().unwrap().is_empty());
    }

    #[test]
    fn a_failed_hcs_create_takes_the_seed_and_the_keys_with_it() {
        let fixture = fixture("cloud-create-failure");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, true);

        let error = pipeline
            .create(
                &fixture.store,
                &cloud_request("cloud-rollback"),
                &fixture.vm_directory,
            )
            .expect_err("an HCS create failure must abort creation");

        assert!(error.to_string().contains("timed out"));
        // The whole directory goes, seed and private key included: nothing is
        // left of a VM that does not exist.
        assert!(!fixture.vm_directory.exists());
        assert!(fixture.store.list().unwrap().is_empty());
    }
```

Импорты `mod tests` дополнить: `use vmlord_core::{CloudImage, GpuMode, NetworkMode, Password, Provisioning, SshAccess, VmCreateRequest, VmSource};`.

- [ ] **Step 3: Убедиться, что тесты не собираются**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --no-run`
Expected: FAIL — `cloud_request` использует `Provisioning`, а конвейер про него ничего не знает: `seed.iso` не пишется, и тесты не проходят даже после сборки.

- [ ] **Step 4: Написать провизионинг**

В `crates/platform/src/create.rs` добавить после `create_hcs_system` приватную функцию:

```rust
/// Writes everything the guest's first boot reads: the VM's key pair, and the
/// seed volume carrying the cloud-config documents.
///
/// The password is hashed here rather than carried further: what reaches
/// `vmlord-seed` -- and through it the volume that stays attached to a running
/// VM -- is a `$6$` entry, never the plaintext.
fn write_provisioning(
    vm_directory: &Path,
    seed_path: &Path,
    vm_name: &str,
    instance_id: &str,
    image: &CloudImage,
    provisioning: &Provisioning,
) -> Result<(), RepositoryError> {
    let authorized_key = match provisioning.ssh {
        SshAccess::Enabled { deploy_key: true } => {
            let pair = vmlord_keys::generate(vm_name)?;
            vm_key::write_key_pair(vm_directory, &pair)?;
            Some(pair.public_openssh().to_owned())
        }
        _ => None,
    };

    let password_hash = provisioning
        .password
        .as_ref()
        .map(password_hash::hash_password)
        .transpose()?;

    let seed = vmlord_seed::build(&vmlord_seed::SeedRequest {
        vm_name,
        instance_id,
        username: &provisioning.username,
        password_hash: password_hash.as_deref(),
        authorized_key: authorized_key.as_deref(),
        ssh: provisioning.ssh,
        locale: &provisioning.locale,
        keyboard: &provisioning.keyboard,
        timezone: &provisioning.timezone,
        admin_group: &image.profile.admin_group,
        ssh_units: &image.profile.ssh_units,
    });

    fs::write(seed_path, vmlord_seed::image(&seed)).map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to write the cloud-init seed at {}: {error}",
            seed_path.display()
        ));
        log::error!("{error}");
        error
    })?;
    log::debug!("wrote the cloud-init seed at {}", seed_path.display());
    Ok(())
}
```

Импорты файла дополнить: `use crate::{..., password_hash, vm_key};` и `use vmlord_core::{CloudImage, Provisioning, SshAccess, ...};`.

- [ ] **Step 5: Развилка по источнику внутри замыкания**

В `VmCreationPipeline::create` заменить безусловное создание диска:

```rust
            match &request.source {
                VmSource::LocalMedia { .. } => {
                    (self.vhd_creator)(
                        &system_disk_path,
                        u64::from(request.disk_gb) * BYTES_PER_GIB,
                    )?;
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
                    (self.cloud_disk)(
                        image,
                        u64::from(request.disk_gb) * BYTES_PER_GIB,
                        &system_disk_path,
                    )?;
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
```

Прежняя проверка `if !Path::new(image_path).is_file()` из Task 2 переезжает внутрь ветки `LocalMedia` и здесь удаляется из своего прежнего места.

- [ ] **Step 6: Собрать и проверить линты**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --no-run && cargo clippy -p vmlord-platform --target x86_64-pc-windows-gnu --all-targets`
Expected: собирается, без предупреждений.

- [ ] **Step 7: Коммит**

```bash
git add crates/platform/Cargo.toml crates/platform/src/create.rs Cargo.lock
git commit -m "$(cat <<'EOF'
TASK-61: Provision a cloud VM as it is created

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Композиционный корень и интеграционный тест на живом Hyper-V

**Files:**
- Modify: `crates/vmlord/Cargo.toml` (зависимость `vmlord-image`)
- Modify: `crates/vmlord/src/main.rs`
- Modify: `crates/platform/tests/hyperv.rs`

**Interfaces:**
- Consumes: `vmlord_image::open_cloud_image` (Task 1), `vmlord_platform::{CloudDiskImporter, import_image}` (Task 3), `AppSettings::image_cache_path`.
- Produces: `fn cloud_disk_importer(cache_directory: PathBuf) -> vmlord_platform::CloudDiskImporter` в `crates/vmlord/src/main.rs`.

- [ ] **Step 1: Зависимость корня**

В `crates/vmlord/Cargo.toml`, в `[dependencies]`, по алфавиту после `vmlord-core`:

```toml
vmlord-image = { path = "../image" }
```

- [ ] **Step 2: Собрать продакшн-импортёр**

В `crates/vmlord/src/main.rs` заменить заглушку Task 3:

```rust
        return Box::new(vmlord_platform::HcsVmRepository::new(
            settings.vm_storage_path.clone(),
            cloud_disk_importer(settings.image_cache_path.clone()),
        ));
```

и добавить рядом с `load_backend`:

```rust
/// Joins the two halves of getting a cloud image onto a VM's disk: fetching it,
/// which knows nothing of Windows and lives in `vmlord-image`, and writing it
/// into a VHDX, which is `vmlord-platform`'s business.
///
/// The composition root is where they meet, which is what keeps the network out
/// of the Windows layer.
///
/// Progress and cancellation are stubbed here: creation still runs in the
/// calling thread, so there is nobody to read a publisher and nobody to set a
/// flag. #64 moves creation onto a worker thread and hands in the real ones.
fn cloud_disk_importer(cache_directory: PathBuf) -> vmlord_platform::CloudDiskImporter {
    Box::new(move |image, disk_size_bytes, target| {
        let mut source = vmlord_image::open_cloud_image(
            &image.profile,
            &image.release,
            &cache_directory,
            disk_size_bytes,
            &ProgressPublisher::default(),
            &AtomicBool::new(false),
        )?;
        vmlord_platform::import_image(&mut source, target, disk_size_bytes).map(|_summary| ())
    })
}
```

Импорты `main.rs` дополнить: `use std::{path::PathBuf, sync::atomic::AtomicBool};` и `ProgressPublisher` из `vmlord_core`.

- [ ] **Step 3: Написать `#[ignore]`-тест**

В конец `crates/platform/tests/hyperv.rs`:

```rust
/// The whole path on a real host: a real Ubuntu cloud image becomes a VM's
/// disk, and the seed the guest reads sits beside it.
///
/// Ignored by default -- it needs Hyper-V, an elevated process and a few
/// hundred megabytes off the network. The importer is assembled from the same
/// two calls the composition root makes, so what is exercised is what ships.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS enabled"]
fn a_vm_is_created_from_a_real_cloud_image() {
    let root = std::env::temp_dir().join(format!("vmlord-cloud-image-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let cache = root.join("cache");
    let vm_directory = root.join("cloud-vm");
    let store = MetadataStore::new(root.join("vm-mapping.json"));

    let pipeline = VmCreationPipeline::production(Box::new(move |image, size, target| {
        let mut source = vmlord_image::open_cloud_image(
            &image.profile,
            &image.release,
            &cache,
            size,
            &vmlord_core::ProgressPublisher::default(),
            &std::sync::atomic::AtomicBool::new(false),
        )?;
        vmlord_platform::import_image(&mut source, target, size).map(|_| ())
    }));

    let request = VmCreateRequest {
        name: "vmlord-cloud-test".into(),
        source: vmlord_core::VmSource::CloudImage {
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
        gpu_mode: vmlord_core::GpuMode::None,
        network_mode: vmlord_core::NetworkMode::Nat,
    };

    let mapping = pipeline
        .create(&store, &request, &vm_directory)
        .expect("a cloud image should become a VM");

    let seed = std::fs::read(vm_directory.join("seed.iso")).expect("the seed should be written");
    assert_eq!(&seed[16 * 2048 + 40..16 * 2048 + 46], b"CIDATA");
    let text = String::from_utf8_lossy(&seed);
    assert!(text.contains("user-data") && text.contains("meta-data"));

    let document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(vm_directory.join("config.json")).unwrap())
            .unwrap();
    assert_eq!(
        document.pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments/1/Path"),
        Some(&serde_json::json!(vm_directory.join("seed.iso")))
    );

    // Best-effort cleanup, in the shape the other tests in this file use.
    if let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }
    let _ = fs::remove_dir_all(&root);
}
```

Тест повторяет принятую в файле форму: временный каталог из `std::env::temp_dir()` с идентификатором процесса в имени, уборка best-effort в конце. Новых помощников не заводить.

- [ ] **Step 4: Собрать всё**

Run: `cargo build --target=x86_64-pc-windows-gnu && cargo test --target x86_64-pc-windows-gnu --no-run && cargo clippy --target x86_64-pc-windows-gnu --all-targets`
Expected: собирается, без предупреждений.

- [ ] **Step 5: Коммит**

```bash
git add crates/vmlord/Cargo.toml crates/vmlord/src/main.rs crates/platform/tests/hyperv.rs Cargo.lock
git commit -m "$(cat <<'EOF'
TASK-61: Wire cloud image creation up in the composition root

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Документация

**Files:**
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Consumes: всё вышенаписанное.
- Produces: ничего в коде.

- [ ] **Step 1: Обновить граф крейтов**

В блоке зависимостей (`ARCHITECTURE.md:175-190`) добавить `image` в список того, что тянет композиционный корень, и `seed` — в то, от чего зависит `platform`. Уточнить фразу «`platform` depends only on `core`»: он зависит также от `keys` и `seed` — оба портируемые и без I/O, — но не от `image`, и почему.

- [ ] **Step 2: Дописать раздел о создании VM из cloud-образа**

После раздела «The cloud-init seed» добавить раздел «Creating a VM from a cloud image»: порядок шагов конвейера, два attachment'а вместо трёх и почему, место `seed.iso` в каталоге VM, участие в rollback через `remove_vm_directory`, шов `CloudDiskImporter` и граница «сеть снаружи платформы», отсутствие прогресса и потока до #64.

- [ ] **Step 3: Снять устаревшие обещания**

Найти в `ARCHITECTURE.md` упоминания «the first failure any of this can produce belongs to #61» и подобные отсылки к #61 как к будущему и переписать в настоящем времени.

- [ ] **Step 4: Коммит**

```bash
git add ARCHITECTURE.md
git commit -m "$(cat <<'EOF'
TASK-61: Document creating a VM from a cloud image

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Проверка перед завершением

- [ ] `cargo test -p vmlord-image` — зелено на хосте.
- [ ] `cargo test -p vmlord-seed -p vmlord-core -p vmlord-keys` — зелено на хосте.
- [ ] `cargo build --target=x86_64-pc-windows-gnu` — собирается.
- [ ] `cargo test --target x86_64-pc-windows-gnu --no-run` — тестовые бинарники собираются.
- [ ] `cargo clippy --target x86_64-pc-windows-gnu --all-targets` — без предупреждений.
- [ ] Прогон unit-тестов `vmlord-platform` на Windows — за владельцем проекта.
- [ ] Ручная проверка создания VM из cloud-образа на живом Hyper-V — за владельцем проекта.
