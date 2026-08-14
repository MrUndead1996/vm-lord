use std::path::PathBuf;
use vmlord_gpu_payload::builder::{PackRequest, pack};

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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

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
}
