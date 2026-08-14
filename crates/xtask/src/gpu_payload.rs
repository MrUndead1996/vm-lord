use std::path::PathBuf;
use vmlord_gpu_payload::builder::{pack, PackRequest};

pub(crate) struct PackCommand { pub recipe: PathBuf, pub input: PathBuf, pub archive: PathBuf, pub catalog_entry: PathBuf }
pub(crate) fn parse<I: IntoIterator<Item = String>>(arguments: I) -> Result<PackCommand, String> { let mut values=arguments.into_iter(); if values.next().as_deref()!=Some("pack"){return Err("expected `pack`".into())} let mut recipe=None;let mut input=None;let mut archive=None;let mut catalog_entry=None;while let Some(flag)=values.next(){let value=values.next().ok_or_else(||format!("missing value for {flag}"))?;let target=match flag.as_str(){"--recipe"=>&mut recipe,"--input"=>&mut input,"--archive"=>&mut archive,"--catalog-entry"=>&mut catalog_entry,_=>return Err(format!("unknown argument `{flag}`"))};if target.replace(PathBuf::from(value)).is_some(){return Err(format!("repeated argument `{flag}"))}}Ok(PackCommand{recipe:recipe.ok_or("missing --recipe")?,input:input.ok_or("missing --input")?,archive:archive.ok_or("missing --archive")?,catalog_entry:catalog_entry.ok_or("missing --catalog-entry")?}) }
pub(crate) fn run(arguments: impl IntoIterator<Item=String>)->Result<(),String>{let command=parse(arguments)?;pack(PackRequest{prepared_directory:&command.input,recipe_path:&command.recipe,archive_path:&command.archive,catalog_entry_path:&command.catalog_entry}).map(|_|()).map_err(|e|e.to_string())}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use super::parse;

    #[test]
    fn pack_arguments_are_explicit_and_complete() {
        let command = parse(["pack", "--recipe", "recipe.json", "--input", "prepared", "--archive", "payload.zip", "--catalog-entry", "entry.json"].into_iter().map(str::to_owned)).unwrap();
        assert_eq!(command.archive, PathBuf::from("payload.zip"));
    }

    #[test]
    fn pack_rejects_unknown_missing_and_repeated_flags() {
        for arguments in [
            vec!["pack", "--recipe", "a", "--unknown", "b"],
            vec!["pack", "--recipe"],
            vec!["pack", "--recipe", "a", "--recipe", "b", "--input", "in", "--archive", "out", "--catalog-entry", "entry"],
        ] {
            assert!(parse(arguments.into_iter().map(str::to_owned)).is_err());
        }
    }
}
