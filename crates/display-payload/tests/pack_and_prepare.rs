#![cfg(feature = "builder")]
//! Packing a display payload, and carrying it through the shared mechanism.
//!
//! One test file because they are one story: what `pack` writes is exactly
//! what `prepare` and `stage_payload` must accept, and a fixture that only one
//! of the two agrees with would prove nothing.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use vmlord_display_payload::{
    DisplayCatalogEntry, DisplayPayloadCatalog, GuestSelector, ProtocolVersionParts,
    builder::{PackRequest, pack},
};
use vmlord_payload::Sha256Digest;

const COMMIT: &str = "14794180686c2fb6307fbe359c359bec765249f3";
const SPEAKS_1_0: ProtocolVersionParts = ProtocolVersionParts { major: 1, minor: 0 };
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vmlord-display-payload-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(path: PathBuf, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// A prepared tree in the shape the per-release container produces.
fn prepared_tree(root: &Path, version: &str, with_module: bool) {
    if with_module {
        write(root.join("content/drm/Kbuild"), b"obj-m += vmlord_drm.o\n");
        write(
            root.join("content/drm/dkms.conf"),
            format!("PACKAGE_NAME=\"vmlord-display\"\nPACKAGE_VERSION=\"{version}\"\n").as_bytes(),
        );
        write(
            root.join("content/drm/vmlord_drm.c"),
            b"/* the module, as far as this test is concerned */\n",
        );
    }
    write(root.join("licenses/GPL-2.0.txt"), b"GPL-2.0 text\n");
    write(
        root.join("sources.json"),
        format!(
            r#"{{"schema_version":1,"sources":[{{"url":"https://vmlord.invalid/display","commit":"{COMMIT}","version":"{version}"}}]}}"#
        )
        .as_bytes(),
    );
}

fn recipe(path: PathBuf, version: &str) {
    write(
        path,
        format!(
            r#"{{"schema_version":1,"version":"{version}",
             "target":{{"distribution":"ubuntu","release":"24.04","architecture":"amd64","payload_abi":1}},
             "proven_on":"6.8.0-137-generic",
             "protocol":{{"major":1,"min_minor":0,"max_minor":0}},
             "sources":[{{"url":"https://vmlord.invalid/display","commit":"{COMMIT}","version":"{version}"}}],
             "licenses":[{{"spdx":"GPL-2.0","path":"licenses/GPL-2.0.txt"}}]}}"#
        )
        .as_bytes(),
    );
}

/// Packs one payload and answers with its directory, entry and archive.
fn packed(temporary: &TemporaryDirectory, version: &str) -> (DisplayCatalogEntry, PathBuf) {
    let prepared = temporary.path().join(format!("prepared-{version}"));
    prepared_tree(&prepared, version, true);
    let recipe_path = temporary.path().join(format!("recipe-{version}.json"));
    recipe(recipe_path.clone(), version);
    let archive = temporary.path().join(format!("payload-{version}.zip"));
    let entry_path = temporary.path().join(format!("entry-{version}.json"));

    pack(PackRequest {
        prepared_directory: &prepared,
        recipe_path: &recipe_path,
        archive_path: &archive,
        catalog_entry_path: &entry_path,
    })
    .expect("a well-formed tree packs");

    let entry = DisplayCatalogEntry::from_json(&fs::read(&entry_path).unwrap())
        .expect("pack writes an entry its own reader accepts");
    (entry, archive)
}

#[test]
fn packing_produces_an_entry_that_describes_its_own_archive() {
    let temporary = TemporaryDirectory::new("pack");

    let (entry, archive) = packed(&temporary, "0.1.0");

    assert_eq!(entry.payload_id(), "display-ubuntu-24.04-amd64-0.1.0");
    assert_eq!(entry.version().to_string(), "0.1.0");
    assert_eq!(entry.proven_on(), "6.8.0-137-generic");
    assert_eq!(
        entry.archive_sha256(),
        &Sha256Digest::hash_reader(File::open(&archive).unwrap()).unwrap(),
        "the entry is written after the archive is closed and measured"
    );
}

#[test]
fn packing_a_tree_with_no_module_is_refused() {
    let temporary = TemporaryDirectory::new("pack-no-module");
    let prepared = temporary.path().join("prepared");
    prepared_tree(&prepared, "0.1.0", false);
    let recipe_path = temporary.path().join("recipe.json");
    recipe(recipe_path.clone(), "0.1.0");

    let error = pack(PackRequest {
        prepared_directory: &prepared,
        recipe_path: &recipe_path,
        archive_path: &temporary.path().join("payload.zip"),
        catalog_entry_path: &temporary.path().join("entry.json"),
    })
    .expect_err("a payload with nothing to build is not a display payload");

    assert!(error.to_string().contains("content/drm"));
}

#[test]
fn a_packed_display_payload_prepares_and_stages() {
    let temporary = TemporaryDirectory::new("prepare");
    let (entry, archive) = packed(&temporary, "0.1.0");

    let ready = vmlord_payload::prepare(vmlord_payload::PrepareRequest {
        entry: &entry,
        cache_root: &temporary.path().join("cache"),
        archive: &archive,
        progress: &|_| {},
        cancel: &AtomicBool::new(false),
    })
    .expect("what pack wrote, prepare must accept");

    assert!(
        ready
            .files_directory()
            .join("content/drm/dkms.conf")
            .is_file()
    );
    assert!(
        ready
            .manifest()
            .files()
            .iter()
            .any(|file| file.path() == "sources.json")
    );

    let staging_root = temporary.path().join("vm").join("display-payload");
    vmlord_payload::ensure_staging_root(&staging_root).unwrap();
    let staged =
        vmlord_payload::stage_payload(&ready, &staging_root, &|_| {}, &AtomicBool::new(false))
            .expect("a ready payload stages");

    assert!(staged.generation_directory().join("payload.json").is_file());
    assert!(
        staged.generation_directory().starts_with(&staging_root),
        "a generation lives under the VM's own staging root and nowhere else"
    );
}

#[test]
fn a_release_directory_holding_two_versions_offers_the_newer_one() {
    let temporary = TemporaryDirectory::new("release");
    let release = temporary.path().join("release");
    for version in ["0.1.0", "0.2.0"] {
        let (entry, archive) = packed(&temporary, version);
        let directory = release.join("display-payload");
        fs::create_dir_all(&directory).unwrap();
        fs::copy(
            &archive,
            directory.join(format!("{}.zip", entry.payload_id())),
        )
        .unwrap();
        fs::write(
            directory.join(format!("{}.json", entry.payload_id())),
            serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();
    }

    let catalog = DisplayPayloadCatalog::from_release_directory(&release)
        .expect("two versions of one payload are a catalog");

    assert_eq!(
        catalog
            .select_for_guest(
                &GuestSelector {
                    distribution: "ubuntu",
                    release: "24.04",
                    architecture: "amd64",
                },
                SPEAKS_1_0,
            )
            .unwrap()
            .version()
            .to_string(),
        "0.2.0"
    );
}
