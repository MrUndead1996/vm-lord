# TASK-55: пара ключей ed25519 на VM — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** каждая VM получает собственную пару ключей ed25519: приватная половина
лежит в каталоге VM с ужатым DACL, публичная доступна для `ssh_authorized_keys`.

**Architecture:** генерация отделена от хранения. Новый крейт `vmlord-keys`
делает пару и сериализует её в OpenSSH-формат — он собирается и тестируется на
любой платформе. Новый модуль `platform::vm_key` кладёт пару в каталог VM и
ужимает права Win32-вызовами. Конвейер создания VM в этой задаче не трогается:
вызов появится вместе со сборкой seed.

**Tech Stack:** Rust 2024, `ssh-key` 0.6, `zeroize` 1, `windows` 0.61
(`Win32_Security_Authorization`).

## Global Constraints

* Спека: `docs/superpowers/specs/2026-08-10-vm-ssh-key-pair-design.md`.
* Ветка `task-55-vm-key-pair`, уже создана от `main`. Коммиты — `TASK-55: <comment>`, subject по-английски.
* Сборка и тесты только под Windows-таргет: `cargo test --target=x86_64-pc-windows-gnu`. Без префикса `timeout`.
* `ssh-key` версии **0.6** (не 0.7 — такой версии в реестре нет), с `default-features = false` и фичами `alloc`, `std`, `ed25519`, `getrandom`.
* Приватный ключ не попадает ни в одну строку лога и ни в один `Debug`-вывод.
* Уровни логов — DEBUG..ERROR, TRACE не используется.
* Комментарии в коде и docstring'и — по-английски, как во всём репозитории.
* `VmCreationPipeline`, `create.rs`, `repository.rs` в этой задаче не меняются.

---

### Task 1: крейт `vmlord-keys`

**Files:**
- Create: `crates/keys/Cargo.toml`
- Create: `crates/keys/src/lib.rs`
- Modify: `Cargo.toml` (список `members`)

**Interfaces:**
- Consumes: `vmlord_core::RepositoryError`.
- Produces: `vmlord_keys::VmKeyPair` с методами `private_openssh(&self) -> &str`
  и `public_openssh(&self) -> &str`; свободная функция
  `vmlord_keys::generate(vm_name: &str) -> Result<VmKeyPair, RepositoryError>`.

- [ ] **Step 1: Завести крейт и включить его в workspace**

`crates/keys/Cargo.toml`:

```toml
[package]
name = "vmlord-keys"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
log.workspace = true
# `ecdsa` and `rsa` are off deliberately: a VM key is always ed25519, and the
# unused algorithms would drag in their own crypto stacks.
ssh-key = { version = "0.6", default-features = false, features = [
    "alloc",
    "ed25519",
    "getrandom",
    "std",
] }
vmlord-core = { path = "../core" }
zeroize = "1"

[lints]
workspace = true
```

В корневом `Cargo.toml` добавить `"crates/keys",` в `members` — по алфавиту,
между `"crates/image"` и `"crates/legacy-backend"`.

- [ ] **Step 2: Написать падающие тесты**

`crates/keys/src/lib.rs` — пока только модуль тестов, без реализации:

```rust
#[cfg(test)]
mod tests {
    use ssh_key::{Algorithm, PrivateKey, PublicKey};

    use super::generate;

    #[test]
    fn the_private_half_is_an_openssh_document_openssh_itself_can_read() {
        let pair = generate("dev-linux").expect("a key pair should be generated");

        assert!(
            pair.private_openssh()
                .starts_with("-----BEGIN OPENSSH PRIVATE KEY-----\n"),
            "{}",
            pair.private_openssh()
        );
        let parsed = PrivateKey::from_openssh(pair.private_openssh())
            .expect("the private key should parse back");
        assert_eq!(parsed.algorithm(), Algorithm::Ed25519);
        assert!(!parsed.is_encrypted(), "the key must carry no passphrase");
    }

    #[test]
    fn the_public_half_is_one_authorized_keys_line() {
        let pair = generate("dev-linux").expect("a key pair should be generated");

        assert!(pair.public_openssh().starts_with("ssh-ed25519 "));
        // The line goes into a YAML list inside user-data; a newline in it
        // would end the entry rather than sit inside it.
        assert!(!pair.public_openssh().contains('\n'));
        let parsed =
            PublicKey::from_openssh(pair.public_openssh()).expect("the public key should parse");
        assert_eq!(parsed.algorithm(), Algorithm::Ed25519);
    }

    /// The whole point of a key pair: the half deployed into the guest has to
    /// be the half the host's private key opens.
    #[test]
    fn the_two_halves_belong_to_each_other() {
        let pair = generate("dev-linux").expect("a key pair should be generated");

        let private =
            PrivateKey::from_openssh(pair.private_openssh()).expect("the private key should parse");
        let public =
            PublicKey::from_openssh(pair.public_openssh()).expect("the public key should parse");

        assert_eq!(private.public_key().key_data(), public.key_data());
    }

    #[test]
    fn every_vm_gets_a_key_of_its_own() {
        let one = generate("dev-linux").expect("a key pair should be generated");
        let other = generate("dev-linux").expect("a key pair should be generated");

        assert_ne!(one.public_openssh(), other.public_openssh());
    }

    #[test]
    fn the_comment_names_the_vm_the_key_belongs_to() {
        let pair = generate("dev-linux").expect("a key pair should be generated");

        assert!(
            pair.public_openssh().ends_with(" vmlord@dev-linux"),
            "{}",
            pair.public_openssh()
        );
    }

    /// The comment is the tail of an `authorized_keys` line, so a newline in
    /// the VM name is not a typo but a second entry in the file.
    #[test]
    fn a_control_character_in_the_name_never_reaches_the_comment() {
        let pair = generate("dev\nssh-rsa AAAA").expect("a key pair should be generated");

        assert_eq!(pair.public_openssh().lines().count(), 1);
        assert!(pair.public_openssh().ends_with(" vmlord@devssh-rsa AAAA"));
    }
}
```

- [ ] **Step 3: Убедиться, что тесты не собираются**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-keys`
Expected: FAIL, `cannot find function 'generate' in this scope`.

- [ ] **Step 4: Реализовать генерацию**

Вставить перед `#[cfg(test)] mod tests` в `crates/keys/src/lib.rs`:

```rust
//! The SSH key pair a single VM is reachable by.
//!
//! One pair per VM rather than one pair for all of them: AppSandbox kept a
//! single key under `%ProgramData%\AppSandbox\ssh\id_appsandbox`, where the
//! compromise of one sandbox reached every other one.
//!
//! Generating a pair is portable and has nothing to say about Windows, so it
//! lives here; putting the private half on disk under a restricted ACL is
//! `vmlord-platform`'s business.

use ssh_key::{Algorithm, LineEnding, PrivateKey, rand_core::OsRng};
use vmlord_core::RepositoryError;
use zeroize::Zeroizing;

/// A VM's key pair, already in the two textual forms it is used in.
///
/// No `Debug`, by design: the private half must have no way of printing
/// itself. `Password` in `vmlord-core` protects the same thing the same way.
pub struct VmKeyPair {
    private_openssh: Zeroizing<String>,
    public_openssh: String,
}

impl VmKeyPair {
    /// The private half, as an OpenSSH PEM document with LF line endings and
    /// no passphrase.
    ///
    /// Unencrypted deliberately: VMLord connects to the guest by itself,
    /// without anyone to type a passphrase, and a passphrase stored next to
    /// the key it protects protects nothing. The file's DACL is the defence.
    #[must_use]
    pub fn private_openssh(&self) -> &str {
        &self.private_openssh
    }

    /// The public half, as a single `authorized_keys` line without a trailing
    /// newline.
    #[must_use]
    pub fn public_openssh(&self) -> &str {
        &self.public_openssh
    }
}

/// Generates a fresh ed25519 pair for the VM named `vm_name`.
pub fn generate(vm_name: &str) -> Result<VmKeyPair, RepositoryError> {
    let mut key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .map_err(|error| failed("generate an ed25519 key pair", vm_name, &error))?;
    key.set_comment(comment_for(vm_name));

    let private_openssh = key
        .to_openssh(LineEnding::LF)
        .map_err(|error| failed("serialize the private key", vm_name, &error))?;
    let public_openssh = key
        .public_key()
        .to_openssh()
        .map_err(|error| failed("serialize the public key", vm_name, &error))?;

    log::debug!("generated an ed25519 key pair for VM \"{vm_name}\"");
    Ok(VmKeyPair {
        private_openssh,
        public_openssh,
    })
}

/// The comment the public key carries, so that a key found in a guest's
/// `authorized_keys` names where it came from.
///
/// Control characters are dropped rather than escaped: the comment is the tail
/// of a line in `authorized_keys`, and a newline inside it would start a
/// second entry.
fn comment_for(vm_name: &str) -> String {
    let name: String = vm_name.chars().filter(|c| !c.is_control()).collect();
    format!("vmlord@{name}")
}

fn failed(operation: &str, vm_name: &str, error: &ssh_key::Error) -> RepositoryError {
    let error =
        RepositoryError::new(format!("failed to {operation} for VM \"{vm_name}\": {error}"));
    log::error!("{error}");
    error
}
```

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-keys`
Expected: PASS, 6 тестов.

Если `set_comment` не примет `String`, передать `comment_for(vm_name).as_str()`.

- [ ] **Step 6: Проверить линты**

Run: `cargo clippy --target=x86_64-pc-windows-gnu -p vmlord-keys --all-targets`
Expected: без предупреждений.

- [ ] **Step 7: Коммит**

```bash
git add Cargo.toml Cargo.lock crates/keys
git commit -m "TASK-55: Generate an ed25519 key pair for a VM"
```

---

### Task 2: ужатие прав на файл ключа

**Files:**
- Create: `crates/platform/src/vm_key.rs`
- Modify: `crates/platform/src/lib.rs` (объявление модуля)
- Modify: `crates/platform/Cargo.toml` (фича `Win32_Security_Authorization`)

**Interfaces:**
- Consumes: `crate::error::windows_error`.
- Produces: `fn restrict_to_owner(path: &Path) -> Result<(), RepositoryError>` —
  внутренняя функция модуля, используется задачей 3; плюс тестовый помощник
  `#[cfg(test)] fn security_descriptor(path: &Path) -> Result<String, RepositoryError>`.

**Контекст.** Каталог VM лежит под настраиваемым storage root, и файл наследует
оттуда DACL, который обычно включает лишние учётные записи. Проверено на живой
системе: у только что созданного файла DACL содержал семь унаследованных ACE.
После ужатия он читается обратно как
`O:<user>D:PAI(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;<user>)`.

- [ ] **Step 1: Включить нужную фичу `windows`**

В `crates/platform/Cargo.toml`, в список `features` крейта `windows`, добавить
`"Win32_Security_Authorization",` — по алфавиту, сразу после `"Win32_Security",`.

- [ ] **Step 2: Написать падающий тест**

Создать `crates/platform/src/vm_key.rs` с одним лишь модулем тестов:

```rust
#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{restrict_to_owner, security_descriptor};

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> TempRoot {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vmlord-vm-key-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    #[test]
    fn a_restricted_file_is_reachable_by_system_administrators_and_the_owner_only() {
        let root = temp_root("restrict");
        let path = root.0.join("id_ed25519");
        File::create_new(&path).expect("the file should be created");

        restrict_to_owner(&path).expect("the DACL should be restricted");

        let descriptor = security_descriptor(&path).expect("the DACL should be read back");
        // `D:P` is what makes the list protected; without it the entries below
        // would sit on top of everything the storage root hands down.
        assert!(descriptor.contains("D:P"), "{descriptor}");
        assert!(descriptor.contains("(A;;FA;;;SY)"), "{descriptor}");
        assert!(descriptor.contains("(A;;FA;;;BA)"), "{descriptor}");
        assert!(
            !descriptor.contains(";ID;"),
            "no entry may be inherited any more: {descriptor}"
        );
        assert_eq!(
            descriptor.matches("(A;;FA;;;").count(),
            3,
            "SYSTEM, Administrators and the owner, and nobody else: {descriptor}"
        );
    }

    #[test]
    fn restricting_a_file_that_is_not_there_fails_with_the_path_in_the_message() {
        let root = temp_root("absent");
        let path = root.0.join("never-created");

        let message = restrict_to_owner(&path)
            .expect_err("a missing file cannot be restricted")
            .to_string();

        assert!(message.contains("never-created"), "{message}");
    }
}
```

- [ ] **Step 3: Убедиться, что тест не собирается**

Сначала объявить модуль: в `crates/platform/src/lib.rs` добавить строку
`mod vm_key;` между `mod subnet;` и `mod vhd;`.

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform vm_key`
Expected: FAIL, `cannot find function 'restrict_to_owner'`.

- [ ] **Step 4: Реализовать ужатие**

Вставить в начало `crates/platform/src/vm_key.rs`:

```rust
//! The VM's own SSH key pair on disk.
//!
//! The private half is the one secret VMLord stores in a file, so the file
//! carries an explicit DACL rather than whatever the storage root hands down.

use std::path::Path;

use vmlord_core::RepositoryError;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
            GetTokenInformation, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{BOOL, HSTRING, PWSTR},
};

use crate::error::windows_error;

/// A wide string the Windows API allocated and this process has to release.
struct LocalString(PWSTR);

impl Drop for LocalString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from an API documented to allocate it
            // with `LocalAlloc`, and this runs exactly once per value.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0.cast())));
            }
        }
    }
}

impl LocalString {
    fn to_owned_string(&self) -> String {
        // SAFETY: the pointer is a NUL-terminated wide string owned by `self`.
        unsafe { self.0.to_string() }.unwrap_or_default()
    }
}

/// A security descriptor the Windows API allocated and this process has to
/// release.
struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: as for `LocalString` -- API-allocated, released once.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0)));
            }
        }
    }
}

/// Restricts `path` to SYSTEM, the Administrators group and the user VMLord
/// runs as, and makes that user the file's owner.
///
/// The user's own entry is not a concession: an administrator's unelevated
/// token does not carry the Administrators group, so without it
/// `ssh -i <path>` from an ordinary console fails with access denied -- and
/// being used by hand is what the key is for. It is also the exact shape
/// Win32-OpenSSH insists on: it refuses a key whose DACL is wider than the
/// owner plus SYSTEM and Administrators.
pub(crate) fn restrict_to_owner(path: &Path) -> Result<(), RepositoryError> {
    let sid = current_user_sid()?;
    // `FA` is full access; `P` protects the list from everything the parent
    // directory would otherwise hand down.
    let sddl = format!("O:{sid}D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{sid})");
    apply_security_descriptor(path, &sddl)?;
    log::debug!(
        "restricted {} to SYSTEM, Administrators and {sid}",
        path.display()
    );
    Ok(())
}

/// The SID of the user this process runs as, in string form.
fn current_user_sid() -> Result<String, RepositoryError> {
    let mut token = HANDLE::default();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no
    // closing; `token` is closed below on both paths.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|error| fail("open the process token", None, error))?;

    let mut needed = 0u32;
    // SAFETY: the first call is the documented way of asking for the size; it
    // fails with ERROR_INSUFFICIENT_BUFFER and fills `needed`.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: `buffer` is `needed` bytes long, which is the size the call
    // above asked for.
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    };
    // SAFETY: `token` came from the successful `OpenProcessToken` above and is
    // closed exactly once here.
    let closed = unsafe { CloseHandle(token) };
    result.map_err(|error| fail("read the token user", None, error))?;
    closed.map_err(|error| fail("close the process token", None, error))?;

    // SAFETY: on success the buffer holds a `TOKEN_USER` followed by the SID
    // it points at, and `buffer` outlives the read below.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    // SAFETY: `user.User.Sid` points into `buffer`, which is still alive.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }
        .map_err(|error| fail("convert the user SID to a string", None, error))?;
    Ok(LocalString(text).to_owned_string())
}

/// Applies the owner and the DACL spelled by `sddl` to `path`.
fn apply_security_descriptor(path: &Path, sddl: &str) -> Result<(), RepositoryError> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the string outlives the call, and the descriptor it allocates is
    // taken over by `LocalDescriptor` immediately below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(|error| fail("parse the security descriptor", Some(path), error))?;
    let descriptor = LocalDescriptor(descriptor);

    let mut dacl = std::ptr::null_mut();
    let mut present = BOOL(0);
    let mut defaulted = BOOL(0);
    // SAFETY: `descriptor` is a valid descriptor; the returned ACL points into
    // it and is used only while it is alive.
    unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) }
        .map_err(|error| fail("read the parsed DACL", Some(path), error))?;

    let mut owner = PSID::default();
    let mut owner_defaulted = BOOL(0);
    // SAFETY: as above -- the SID points into the live `descriptor`.
    unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut owner_defaulted) }
        .map_err(|error| fail("read the parsed owner", Some(path), error))?;

    let wide = HSTRING::from(path.as_os_str().to_string_lossy().as_ref());
    // SAFETY: every pointer passed in outlives the call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            &wide,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            Some(owner),
            None,
            Some(dacl),
            None,
        )
    };
    status
        .ok()
        .map_err(|error| fail("set the file's security descriptor", Some(path), error))
}

fn fail(operation: &str, path: Option<&Path>, error: windows::core::Error) -> RepositoryError {
    let error = match path {
        Some(path) => {
            let described = windows_error(operation, None, error);
            RepositoryError::new(format!("{described} on {}", path.display()))
        }
        None => windows_error(operation, None, error),
    };
    log::error!("{error}");
    error
}
```

И тестовый помощник — в конец файла, **перед** `#[cfg(test)] mod tests`:

```rust
/// Reads the owner and the DACL of `path` back as an SDDL string.
///
/// Only the tests need this: production sets the descriptor and never asks
/// what it became.
#[cfg(test)]
fn security_descriptor(path: &Path) -> Result<String, RepositoryError> {
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
    };

    let wide = HSTRING::from(path.as_os_str().to_string_lossy().as_ref());
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the path outlives the call; the descriptor it allocates is taken
    // over by `LocalDescriptor` below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            &wide,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &mut descriptor,
        )
    };
    status
        .ok()
        .map_err(|error| fail("read the file's security descriptor", Some(path), error))?;
    let descriptor = LocalDescriptor(descriptor);

    let mut text = PWSTR::null();
    // SAFETY: `descriptor` is alive for the duration of the call.
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.0,
            SDDL_REVISION_1,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut text,
            None,
        )
    }
    .map_err(|error| fail("format the security descriptor", Some(path), error))?;
    Ok(LocalString(text).to_owned_string())
}
```

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform vm_key`
Expected: PASS, 2 теста.

- [ ] **Step 6: Проверить линты**

Run: `cargo clippy --target=x86_64-pc-windows-gnu -p vmlord-platform --all-targets`
Expected: без новых предупреждений. `restrict_to_owner` пока никем не вызывается
из production-кода — если clippy или rustc ругнётся на неиспользуемую функцию,
не глушить атрибутом: следующая задача её вызовет, и предупреждение исчезнет.
Если задача 3 выполняется не сразу следом, коммит всё равно делается.

- [ ] **Step 7: Коммит**

```bash
git add crates/platform/Cargo.toml crates/platform/src/lib.rs crates/platform/src/vm_key.rs
git commit -m "TASK-55: Restrict a file to SYSTEM, Administrators and the owner"
```

---

### Task 3: запись и чтение пары в каталоге VM

**Files:**
- Modify: `crates/platform/src/layout.rs` (два новых пути и тест)
- Modify: `crates/platform/src/vm_key.rs` (`write_key_pair`, `read_public_key`, тесты)
- Modify: `crates/platform/src/lib.rs` (реэкспорт)
- Modify: `crates/platform/Cargo.toml` (зависимость `vmlord-keys`)

**Interfaces:**
- Consumes: `vmlord_keys::{VmKeyPair, generate}`; `super::restrict_to_owner` из задачи 2;
  `layout::vm_directory` (уже есть).
- Produces:
  `pub fn write_key_pair(vm_directory: &Path, pair: &VmKeyPair) -> Result<(), RepositoryError>`,
  `pub fn read_public_key(vm_directory: &Path) -> Result<Option<String>, RepositoryError>`,
  `layout::ssh_key_path(vm_directory: &Path) -> PathBuf`,
  `layout::ssh_public_key_path(vm_directory: &Path) -> PathBuf`.

- [ ] **Step 1: Добавить зависимость**

В `crates/platform/Cargo.toml`, в `[dependencies]`, после `vmlord-core`:

```toml
vmlord-keys = { path = "../keys" }
```

- [ ] **Step 2: Написать падающий тест на пути**

В `crates/platform/src/layout.rs`, в `mod tests`, дополнить импорт
`use super::{configuration_path, ssh_key_path, ssh_public_key_path, system_disk_path, vm_directory};`
и добавить тест:

```rust
    #[test]
    fn a_vms_key_pair_lives_beside_its_disks() {
        let directory = vm_directory(Path::new("/vms"), "dev-linux").unwrap();

        assert_eq!(
            ssh_key_path(&directory),
            PathBuf::from("/vms")
                .join("dev-linux")
                .join("keys")
                .join("id_ed25519")
        );
        assert_eq!(
            ssh_public_key_path(&directory),
            PathBuf::from("/vms")
                .join("dev-linux")
                .join("keys")
                .join("id_ed25519.pub")
        );
    }
```

- [ ] **Step 3: Убедиться, что тест не собирается**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform layout`
Expected: FAIL, `cannot find function 'ssh_key_path'`.

- [ ] **Step 4: Добавить пути**

В `crates/platform/src/layout.rs`, после `system_disk_path`:

```rust
/// Returns the path of the VM's own SSH private key.
pub(crate) fn ssh_key_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("keys").join("id_ed25519")
}

/// Returns the path of the VM's own SSH public key.
///
/// The public half is derivable from the private one in microseconds, so this
/// file is a convenience rather than a necessity: it lets a person see which
/// key went into the guest without starting the VM.
pub(crate) fn ssh_public_key_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("keys").join("id_ed25519.pub")
}
```

- [ ] **Step 5: Прогнать тест путей**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform layout`
Expected: PASS.

- [ ] **Step 6: Написать падающие тесты на запись и чтение**

В `crates/platform/src/vm_key.rs`, в `mod tests`, дополнить импорт
`use super::{read_public_key, restrict_to_owner, security_descriptor, write_key_pair};`
и добавить:

```rust
    #[test]
    fn writing_a_pair_leaves_both_halves_in_the_vms_keys_directory() {
        let root = temp_root("write");
        let pair = vmlord_keys::generate("dev-linux").expect("a key pair should be generated");

        write_key_pair(&root.0, &pair).expect("the key pair should be written");

        let private = fs::read_to_string(crate::layout::ssh_key_path(&root.0))
            .expect("the private key should be on disk");
        assert_eq!(private, pair.private_openssh());
        assert_eq!(
            read_public_key(&root.0).expect("the public key should be readable"),
            Some(pair.public_openssh().to_string())
        );
    }

    #[test]
    fn the_private_key_is_written_with_a_restricted_dacl() {
        let root = temp_root("write-dacl");
        let pair = vmlord_keys::generate("dev-linux").expect("a key pair should be generated");

        write_key_pair(&root.0, &pair).expect("the key pair should be written");

        let descriptor = security_descriptor(&crate::layout::ssh_key_path(&root.0))
            .expect("the DACL should be read back");
        assert!(descriptor.contains("D:P"), "{descriptor}");
        assert!(!descriptor.contains(";ID;"), "{descriptor}");
    }

    /// A guest already holds the public half of the key it was given; handing
    /// the VM a new pair would leave the guest trusting a key the host no
    /// longer has. Re-keying is deleting the VM and creating it again.
    #[test]
    fn a_vm_that_already_has_a_key_is_never_given_another_one() {
        let root = temp_root("existing");
        let first = vmlord_keys::generate("dev-linux").expect("a key pair should be generated");
        let second = vmlord_keys::generate("dev-linux").expect("a key pair should be generated");
        write_key_pair(&root.0, &first).expect("the first key pair should be written");

        let message = write_key_pair(&root.0, &second)
            .expect_err("a second key pair must be refused")
            .to_string();

        assert!(message.contains("already has an SSH key"), "{message}");
        assert_eq!(
            read_public_key(&root.0).expect("the public key should be readable"),
            Some(first.public_openssh().to_string()),
            "the existing key must survive the refusal"
        );
    }

    /// A VM created with `ssh_enabled: false` has no key, and that is a state
    /// rather than a failure.
    #[test]
    fn a_vm_without_a_key_reports_no_key_rather_than_an_error() {
        let root = temp_root("keyless");

        assert_eq!(
            read_public_key(&root.0).expect("a keyless VM is not a failure"),
            None
        );
    }
```

- [ ] **Step 7: Убедиться, что тесты не собираются**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform vm_key`
Expected: FAIL, `cannot find function 'write_key_pair'`.

- [ ] **Step 8: Реализовать запись и чтение**

В `crates/platform/src/vm_key.rs` расширить импорты:

```rust
use std::{
    fs::{self, File},
    io::{self, ErrorKind, Write},
    path::Path,
};

use vmlord_keys::VmKeyPair;

use crate::{error::windows_error, layout};
```

И добавить перед `restrict_to_owner`:

```rust
/// Writes `pair` into the VM's directory: the private half under a restricted
/// DACL, the public half beside it.
///
/// The order matters. The file is created empty, its DACL is narrowed, and
/// only then does the key go in: between `create_new` and
/// `SetNamedSecurityInfoW` the file carries whatever the storage root hands
/// down, and what sits there during that window must not be a private key.
pub fn write_key_pair(vm_directory: &Path, pair: &VmKeyPair) -> Result<(), RepositoryError> {
    let private_path = layout::ssh_key_path(vm_directory);
    let keys_directory = private_path
        .parent()
        .expect("the private key path always has a parent under vm_directory");
    fs::create_dir_all(keys_directory)
        .map_err(|error| io_failure("create the keys directory", keys_directory, &error))?;

    let mut file = match File::create_new(&private_path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let error = RepositoryError::new(format!(
                "VM directory {} already has an SSH key",
                vm_directory.display()
            ));
            log::error!("{error}");
            return Err(error);
        }
        Err(error) => return Err(io_failure("create the private key", &private_path, &error)),
    };

    // A failure here is a failure of the whole write: the file exists, and
    // leaving a private key under inherited permissions is worse than failing.
    if let Err(error) = restrict_to_owner(&private_path) {
        drop(file);
        let _ = fs::remove_file(&private_path);
        return Err(error);
    }

    file.write_all(pair.private_openssh().as_bytes())
        .and_then(|()| file.flush())
        .map_err(|error| io_failure("write the private key", &private_path, &error))?;

    let public_path = layout::ssh_public_key_path(vm_directory);
    fs::write(&public_path, pair.public_openssh())
        .map_err(|error| io_failure("write the public key", &public_path, &error))?;

    log::debug!(
        "wrote an SSH key pair into {}",
        keys_directory.display()
    );
    Ok(())
}

/// Reads the VM's public key back, as an `authorized_keys` line.
///
/// A VM with no key at all is a normal state -- SSH can be turned off -- so an
/// absent file is `None` rather than a failure.
pub fn read_public_key(vm_directory: &Path) -> Result<Option<String>, RepositoryError> {
    let path = layout::ssh_public_key_path(vm_directory);
    match fs::read_to_string(&path) {
        Ok(key) => Ok(Some(key.trim_end().to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            log::debug!("VM directory {} holds no SSH key", vm_directory.display());
            Ok(None)
        }
        Err(error) => Err(io_failure("read the public key", &path, &error)),
    }
}

fn io_failure(operation: &str, path: &Path, error: &io::Error) -> RepositoryError {
    let error = RepositoryError::new(format!(
        "failed to {operation} at {}: {error}",
        path.display()
    ));
    log::error!("{error}");
    error
}
```

В `crates/platform/src/lib.rs`, к списку `pub use`, добавить:

```rust
pub use vm_key::{read_public_key, write_key_pair};
```

- [ ] **Step 9: Прогнать тесты**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform vm_key`
Expected: PASS, 6 тестов.

- [ ] **Step 10: Прогнать весь пакет и линты**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform`
Expected: PASS, ничего не сломано.

Run: `cargo clippy --target=x86_64-pc-windows-gnu -p vmlord-platform --all-targets`
Expected: без предупреждений.

- [ ] **Step 11: Коммит**

```bash
git add Cargo.lock crates/platform
git commit -m "TASK-55: Store a VM's key pair in its own directory"
```

---

### Task 4: ключ уходит вместе с VM, и документация

**Files:**
- Modify: `crates/platform/src/cleanup.rs` (тест)
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Consumes: `cleanup::remove_vm_directory`, `layout::ssh_key_path` из задачи 3.
- Produces: ничего нового.

- [ ] **Step 1: Написать тест на удаление**

В `crates/platform/src/cleanup.rs`, в `mod tests`, после
`removes_a_vm_directory_with_everything_under_it`:

```rust
    /// The key must not outlive the VM it belongs to: nothing else ever
    /// deletes it, so deleting the directory is the whole of its lifecycle.
    #[test]
    fn a_removed_vm_takes_its_ssh_key_with_it() {
        let root = temp_root("keys");
        let vm_directory = root.0.join("vm");
        let pair = vmlord_keys::generate("dev-linux").expect("a key pair should be generated");
        crate::vm_key::write_key_pair(&vm_directory, &pair)
            .expect("the key pair should be written");

        remove_vm_directory(&vm_directory).expect("a VM directory with a key should be removed");

        assert!(!crate::layout::ssh_key_path(&vm_directory).exists());
        assert!(!vm_directory.exists());
    }
```

- [ ] **Step 2: Убедиться, что тест падает**

Run: `cargo test --target=x86_64-pc-windows-gnu -p vmlord-platform cleanup`
Expected: PASS сразу — `remove_dir_all` уже уносит всё. Это регрессионный тест,
фиксирующий требование задачи; если он падает, значит `write_key_pair` кладёт
ключ не под каталог VM, и это баг задачи 3.

- [ ] **Step 3: Обновить ARCHITECTURE.md**

Вставить новый раздел непосредственно **перед** строкой `### The cloud-init seed`:

```markdown
### The VM's SSH key pair

Every VM gets its own ed25519 pair rather than sharing one. AppSandbox kept a
single key under `%ProgramData%\AppSandbox\ssh\id_appsandbox`
(`legacy-backend/src/windows.rs:691`), where the compromise of one sandbox
reached every other one.

`vmlord-keys` generates the pair and serialises it: an OpenSSH PEM document for
the private half, one `authorized_keys` line commented `vmlord@<vm>` for the
public one. It depends on `core` alone, so its tests run on any host. The pair
carries no passphrase -- VMLord connects to the guest unattended, and a
passphrase stored beside the key it protects protects nothing. `VmKeyPair` has
no `Debug`, for the reason `Password` and `Seed` have none.

`platform::vm_key` puts the pair under `keys/` in the VM's directory:
`id_ed25519` and `id_ed25519.pub`, both named by `platform::layout`. The private
file is created empty, its DACL is narrowed to SYSTEM, the Administrators group
and the user VMLord runs as -- who also becomes its owner -- and only then does
the key go in; the window between creating the file and setting its permissions
must not hold a private key. The user's own entry is what lets `ssh -i` work
from an unelevated console, and it is the exact shape Win32-OpenSSH accepts. A
VM that already has a key is never given another: the guest holds the public
half, and a new pair would leave it trusting a key the host no longer has.
Deleting the VM deletes the key with the directory.
```

Кроме того, в разделе `### The cloud-init seed` фраза «and the public key
(#55)» уже ссылается на эту задачу — менять её не нужно.

- [ ] **Step 4: Прогнать всю сборку**

Run: `cargo test --target=x86_64-pc-windows-gnu`
Expected: PASS во всех пакетах.

Run: `cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
Expected: без предупреждений.

- [ ] **Step 5: Коммит**

```bash
git add crates/platform/src/cleanup.rs ARCHITECTURE.md
git commit -m "TASK-55: Document the per-VM SSH key pair"
```

---

## Что осталось за границей задачи

Вызов `write_key_pair` из `VmCreationPipeline` и передача `read_public_key` в
`SeedRequest::authorized_key` — это сборка seed в конвейере. Решение
«генерировать пару или нет» принимает вызывающий, по
`SshAccess::Enabled { deploy_key }`; модули этой задачи генерируют и пишут
тогда, когда их просят.

## Отклонение от спеки

Спека перечисляет тест «приватный ключ не появляется в выводе форматирования».
Такого теста в плане нет: `VmKeyPair` не реализует `Debug`, поэтому
`format!("{pair:?}")` не компилируется, и проверить это можно только
compile-fail-тестом, которого в репозитории нет и который не окупается.
Гарантия остаётся структурной — приватные поля плюс отсутствие `Debug`, — и
названа в docstring'е типа.
