use std::{
    fs,
    path::{Path, PathBuf},
};
use vmlord_gpu_payload::{
    CatalogEntry, Sha256Digest,
    builder::{PackRequest, pack},
    local_archive_path, local_entry_path,
};

pub(crate) struct PackCommand {
    pub recipe: PathBuf,
    pub input: PathBuf,
    pub archive: PathBuf,
    pub catalog_entry: PathBuf,
}
pub(crate) fn parse<I: IntoIterator<Item = String>>(arguments: I) -> Result<PackCommand, String> {
    let mut values = arguments.into_iter();
    if values.next().as_deref() != Some("pack") {
        return Err("expected `pack`".into());
    }
    let mut recipe = None;
    let mut input = None;
    let mut archive = None;
    let mut catalog_entry = None;
    while let Some(flag) = values.next() {
        let value = values
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        let target = match flag.as_str() {
            "--recipe" => &mut recipe,
            "--input" => &mut input,
            "--archive" => &mut archive,
            "--catalog-entry" => &mut catalog_entry,
            _ => return Err(format!("unknown argument `{flag}`")),
        };
        if target.replace(PathBuf::from(value)).is_some() {
            return Err(format!("repeated argument `{flag}"));
        }
    }
    Ok(PackCommand {
        recipe: recipe.ok_or("missing --recipe")?,
        input: input.ok_or("missing --input")?,
        archive: archive.ok_or("missing --archive")?,
        catalog_entry: catalog_entry.ok_or("missing --catalog-entry")?,
    })
}
pub(crate) fn run(arguments: impl IntoIterator<Item = String>) -> Result<(), String> {
    let command = parse(arguments)?;
    pack(PackRequest {
        prepared_directory: &command.input,
        recipe_path: &command.recipe,
        archive_path: &command.archive,
        catalog_entry_path: &command.catalog_entry,
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Copies one packed payload into a distribution, refusing anything that is
/// not exactly what `pack` wrote.
///
/// `source` is the directory the recipe's `pack` step wrote `payload.zip` and
/// `catalog-entry.json` into. Both travel, named by the payload's ID: the
/// application has no catalog of its own any more, and reads the one a release
/// carries. An entry beside the executable is no longer trusted for being
/// compiled in -- but whoever can write there can equally replace the
/// executable, so the trust boundary is unchanged and merely visible.
///
/// Deeper checks -- `payload.json`, `sources.json`, expansion limits -- belong
/// to `prepare` on the machine that will use the payload. Repeating them here
/// would be a second opinion that can drift from the first.
pub(crate) fn stage_release_payload(source: &Path, destination: &Path) -> Result<String, String> {
    let entry_path = source.join("catalog-entry.json");
    let archive_path = source.join("payload.zip");
    let entry_bytes = fs::read(&entry_path)
        .map_err(|error| format!("cannot read {}: {error}", entry_path.display()))?;
    let entry = CatalogEntry::from_json(&entry_bytes).map_err(|error| {
        format!(
            "{} is not a packed catalog entry: {error}",
            entry_path.display()
        )
    })?;

    let archive = fs::read(&archive_path)
        .map_err(|error| format!("cannot read {}: {error}", archive_path.display()))?;
    let digest = Sha256Digest::hash_reader(archive.as_slice())
        .map_err(|error| format!("cannot hash {}: {error}", archive_path.display()))?;
    if digest != *entry.archive_sha256() {
        return Err(format!(
            "{} hashes to {digest}; its entry says {}",
            archive_path.display(),
            entry.archive_sha256()
        ));
    }

    let target = local_archive_path(destination, entry.payload_id());
    let directory = target
        .parent()
        .expect("a payload archive path always has a parent");
    fs::create_dir_all(directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    fs::write(&target, &archive)
        .map_err(|error| format!("cannot write {}: {error}", target.display()))?;
    let entry_target = local_entry_path(destination, entry.payload_id());
    fs::write(&entry_target, &entry_bytes)
        .map_err(|error| format!("cannot write {}: {error}", entry_target.display()))?;
    Ok(entry.payload_id().to_owned())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use vmlord_gpu_payload::PayloadCatalog;
    use vmlord_gpu_payload::builder::{PackRequest, pack};

    use super::{parse, run};

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmlord-xtask-gpu-payload-{label}-{}-{sequence}",
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

    #[test]
    fn pack_arguments_are_explicit_and_complete() {
        let command = parse(
            [
                "pack",
                "--recipe",
                "recipe.json",
                "--input",
                "prepared",
                "--archive",
                "payload.zip",
                "--catalog-entry",
                "entry.json",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(command.archive, PathBuf::from("payload.zip"));
    }

    #[test]
    fn pack_rejects_unknown_missing_and_repeated_flags() {
        let complete = [
            ("--recipe", "recipe.json"),
            ("--input", "prepared"),
            ("--archive", "payload.zip"),
            ("--catalog-entry", "entry.json"),
        ];
        for missing in complete.map(|(flag, _)| flag) {
            let mut arguments = vec!["pack".to_owned()];
            for (flag, value) in complete {
                if flag != missing {
                    arguments.extend([flag.to_owned(), value.to_owned()]);
                }
            }
            assert!(parse(arguments).is_err(), "accepted missing {missing}");
        }
        for (repeated, repeated_value) in complete {
            let mut arguments = vec!["pack".to_owned()];
            for (flag, value) in complete {
                arguments.extend([flag.to_owned(), value.to_owned()]);
            }
            arguments.extend([repeated.to_owned(), repeated_value.to_owned()]);
            assert!(parse(arguments).is_err(), "accepted repeated {repeated}");
        }
        for arguments in [
            vec!["pack", "--unknown"],
            vec!["pack", "--unknown", "value"],
            vec!["unknown"],
            vec![],
        ] {
            assert!(parse(arguments.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn run_delegates_archive_and_catalog_outputs_to_the_builder() {
        let temporary = TemporaryDirectory::new("delegation");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gpu-payload/tests/fixtures");
        let prepared = fixture.join("prepared");
        let recipe = fixture.join("recipe.json");
        let cli_archive = temporary.path().join("cli.zip");
        let cli_entry = temporary.path().join("cli.json");
        let direct_archive = temporary.path().join("direct.zip");
        let direct_entry = temporary.path().join("direct.json");

        run([
            "pack".to_owned(),
            "--recipe".to_owned(),
            recipe.display().to_string(),
            "--input".to_owned(),
            prepared.display().to_string(),
            "--archive".to_owned(),
            cli_archive.display().to_string(),
            "--catalog-entry".to_owned(),
            cli_entry.display().to_string(),
        ])
        .unwrap();
        pack(PackRequest {
            prepared_directory: &prepared,
            recipe_path: &recipe,
            archive_path: &direct_archive,
            catalog_entry_path: &direct_entry,
        })
        .unwrap();

        assert_eq!(
            fs::read(cli_archive).unwrap(),
            fs::read(direct_archive).unwrap()
        );
        assert_eq!(
            fs::read(cli_entry).unwrap(),
            fs::read(direct_entry).unwrap()
        );
    }

    /// Packs the crate's fixture into `directory`, as the recipe's `pack` step
    /// does, and answers with the payload ID the entry carries.
    fn packed_pair(directory: &Path) -> String {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gpu-payload/tests/fixtures");
        pack(PackRequest {
            prepared_directory: &fixture.join("prepared"),
            recipe_path: &fixture.join("recipe.json"),
            archive_path: &directory.join("payload.zip"),
            catalog_entry_path: &directory.join("catalog-entry.json"),
        })
        .unwrap();
        let entry: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.join("catalog-entry.json")).unwrap())
                .unwrap();
        entry["payload_id"].as_str().unwrap().to_owned()
    }

    #[test]
    fn a_packed_pair_is_copied_under_its_payload_id() {
        let temporary = TemporaryDirectory::new("stage-ok");
        let source = temporary.path().join("built");
        let destination = temporary.path().join("dist");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let payload_id = packed_pair(&source);

        assert_eq!(
            super::stage_release_payload(&source, &destination).unwrap(),
            payload_id
        );
        assert_eq!(
            fs::read(
                destination
                    .join("gpu-payload")
                    .join(format!("{payload_id}.zip"))
            )
            .unwrap(),
            fs::read(source.join("payload.zip")).unwrap()
        );
        assert_eq!(
            fs::read(
                destination
                    .join("gpu-payload")
                    .join(format!("{payload_id}.json"))
            )
            .unwrap(),
            fs::read(source.join("catalog-entry.json")).unwrap()
        );
        // The catalog the application will assemble has to accept what dist
        // wrote, or the release ships a pair nothing can read.
        assert_eq!(
            PayloadCatalog::from_release_directory(&destination)
                .expect("a staged pair must read back as a catalog")
                .entries()
                .len(),
            1
        );
    }

    #[test]
    fn a_pair_that_is_not_what_pack_produced_fails_the_build() {
        let temporary = TemporaryDirectory::new("stage-bad");
        let destination = temporary.path().join("dist");
        fs::create_dir_all(&destination).unwrap();

        // Each case is a separate source directory: a build tool that accepted
        // any of these would put bytes nobody verified into a release.
        for (label, damage) in [
            (
                "truncated",
                Box::new(|source: &Path| {
                    let archive = fs::read(source.join("payload.zip")).unwrap();
                    fs::write(source.join("payload.zip"), &archive[..archive.len() - 1]).unwrap();
                }) as Box<dyn Fn(&Path)>,
            ),
            (
                "flipped",
                Box::new(|source: &Path| {
                    let mut archive = fs::read(source.join("payload.zip")).unwrap();
                    archive[0] ^= 0xFF;
                    fs::write(source.join("payload.zip"), archive).unwrap();
                }),
            ),
            (
                "entry-invalid",
                Box::new(|source: &Path| {
                    fs::write(source.join("catalog-entry.json"), b"{}").unwrap();
                }),
            ),
            (
                "archive-missing",
                Box::new(|source: &Path| {
                    fs::remove_file(source.join("payload.zip")).unwrap();
                }),
            ),
            (
                "entry-missing",
                Box::new(|source: &Path| {
                    fs::remove_file(source.join("catalog-entry.json")).unwrap();
                }),
            ),
        ] {
            let source = temporary.path().join(label);
            fs::create_dir_all(&source).unwrap();
            packed_pair(&source);
            damage(&source);

            assert!(
                super::stage_release_payload(&source, &destination).is_err(),
                "accepted a {label} pair"
            );
        }
    }
}
