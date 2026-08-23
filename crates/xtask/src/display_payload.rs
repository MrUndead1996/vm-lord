//! `cargo xtask display-payload pack`, and the display half of `cargo dist`.

use std::{
    fs,
    path::{Path, PathBuf},
};

use vmlord_display_payload::{
    DisplayCatalogEntry, LOCAL_ARCHIVE_DIRECTORY,
    builder::{PackRequest, pack},
};
use vmlord_payload::{Sha256Digest, release};

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
            return Err(format!("repeated argument `{flag}`"));
        }
    }
    Ok(PackCommand {
        recipe: recipe.ok_or("missing --recipe")?,
        input: input.ok_or("missing --input")?,
        archive: archive.ok_or("missing --archive")?,
        catalog_entry: catalog_entry.ok_or("missing --catalog-entry")?,
    })
}

/// Whether a recipe's declared protocol range contains what this build speaks.
///
/// The host already declines a catalog entry whose range does not cover its
/// version. This is the other half of that claim, checked where an archive is
/// made rather than discovered inside a VM: the services in the archive are
/// what the range is a promise about, and they are built from this tree.
fn protocol_range_covers_this_build(
    major: u32,
    min_minor: u32,
    max_minor: u32,
) -> Result<(), String> {
    let current = vmlord_display_protocol::handshake::CURRENT_VERSION;
    if major != current.major || current.minor < min_minor || current.minor > max_minor {
        return Err(format!(
            "the recipe declares display protocol {major}.{min_minor}-{major}.{max_minor} and this build speaks {}.{}",
            current.major, current.minor
        ));
    }

    Ok(())
}

/// The `protocol` block of a recipe, and nothing else from it.
///
/// Read here rather than taken from the builder, which parses the recipe for
/// its own purposes and does not hand it back. Two readers of one small file
/// is cheaper than an accessor that exists for one caller.
#[derive(serde::Deserialize)]
struct RecipeProtocol {
    protocol: DeclaredRange,
}

#[derive(serde::Deserialize)]
struct DeclaredRange {
    major: u32,
    min_minor: u32,
    max_minor: u32,
}

/// Checks the recipe's claim before anything is packed.
fn check_recipe_protocol(recipe_path: &Path) -> Result<(), String> {
    let bytes = fs::read(recipe_path)
        .map_err(|error| format!("cannot read {}: {error}", recipe_path.display()))?;
    let recipe: RecipeProtocol = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "{} is not a display payload recipe: {error}",
            recipe_path.display()
        )
    })?;

    protocol_range_covers_this_build(
        recipe.protocol.major,
        recipe.protocol.min_minor,
        recipe.protocol.max_minor,
    )
}

pub(crate) fn run(arguments: impl IntoIterator<Item = String>) -> Result<(), String> {
    let command = parse(arguments)?;
    check_recipe_protocol(&command.recipe)?;
    pack(PackRequest {
        prepared_directory: &command.input,
        recipe_path: &command.recipe,
        archive_path: &command.archive,
        catalog_entry_path: &command.catalog_entry,
    })
    .map(|_| ())
    .map_err(|error| error.to_string())
}

/// Copies one packed display payload into a distribution, refusing anything
/// that is not exactly what `pack` wrote.
///
/// The deeper checks -- `payload.json`, `sources.json`, the expansion limits --
/// belong to `prepare` on the machine that will use the payload. Repeating them
/// here would be a second opinion that can drift from the first.
pub(crate) fn stage_release_payload(source: &Path, destination: &Path) -> Result<String, String> {
    let entry_path = source.join("catalog-entry.json");
    let archive_path = source.join("payload.zip");
    let entry_bytes = fs::read(&entry_path)
        .map_err(|error| format!("cannot read {}: {error}", entry_path.display()))?;
    let entry = DisplayCatalogEntry::from_json(&entry_bytes).map_err(|error| {
        format!(
            "{} is not a packed display catalog entry: {error}",
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

    let target = release::archive_path(destination, LOCAL_ARCHIVE_DIRECTORY, entry.payload_id());
    let directory = target
        .parent()
        .expect("a payload archive path always has a parent");
    fs::create_dir_all(directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    fs::write(&target, &archive)
        .map_err(|error| format!("cannot write {}: {error}", target.display()))?;
    let entry_target =
        release::entry_path(destination, LOCAL_ARCHIVE_DIRECTORY, entry.payload_id());
    fs::write(&entry_target, &entry_bytes)
        .map_err(|error| format!("cannot write {}: {error}", entry_target.display()))?;
    Ok(entry.payload_id().to_owned())
}

#[cfg(test)]
mod tests {
    use super::{parse, protocol_range_covers_this_build};

    #[test]
    fn a_recipe_whose_range_excludes_this_build_is_refused() {
        // The range used to be a placeholder. Now the services in the archive
        // are what makes it a claim, so packing is where it is checked.
        assert!(protocol_range_covers_this_build(1, 0, 0).is_ok());
        assert!(protocol_range_covers_this_build(2, 0, 0).is_err());
        assert!(protocol_range_covers_this_build(1, 3, 5).is_err());
    }

    fn arguments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn pack_needs_all_four_paths() {
        assert!(
            parse(arguments(&[
                "pack",
                "--recipe",
                "r.json",
                "--input",
                "prepared",
                "--archive",
                "p.zip",
                "--catalog-entry",
                "e.json"
            ]))
            .is_ok()
        );
        assert!(parse(arguments(&["pack", "--recipe", "r.json"])).is_err());
        assert!(parse(arguments(&["build"])).is_err());
        assert!(
            parse(arguments(&["pack", "--recipe", "a", "--recipe", "b"])).is_err(),
            "a repeated argument is a mistake worth naming"
        );
    }
}
