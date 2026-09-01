//! `cargo appsandbox-convert` -- the offline conversion, run against a root
//! mounted on this machine.
//!
//! The mount is not this command's business: under WSL the copy is attached
//! with `wsl --mount --vhd <copy> --bare` and its root partition mounted by
//! hand, and the same conversion runs against whatever root it is given.

use std::{fs, path::PathBuf};

use vmlord_appsandbox_convert::{Conversion, convert, system_ldconfig, verify};

/// A path and a flag: nothing here is a secret.
#[derive(Debug)]
pub(crate) struct Arguments {
    pub(crate) input: PathBuf,
    pub(crate) verify_only: bool,
}

const USAGE: &str = "usage: cargo appsandbox-convert --input <document.json> [--verify-only]";

impl Arguments {
    pub(crate) fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut input = None;
        let mut verify_only = false;
        let mut arguments = arguments;
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--input" => {
                    input = Some(PathBuf::from(
                        arguments
                            .next()
                            .ok_or_else(|| format!("--input needs a path\n{USAGE}"))?,
                    ));
                }
                "--verify-only" => verify_only = true,
                other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
            }
        }
        Ok(Self {
            input: input.ok_or_else(|| format!("--input is required\n{USAGE}"))?,
            verify_only,
        })
    }
}

pub(crate) fn run(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let arguments = Arguments::parse(arguments)?;
    let document = fs::read_to_string(&arguments.input)
        .map_err(|error| format!("{} could not be read: {error}", arguments.input.display()))?;
    let conversion = Conversion::from_json(&document).map_err(|error| error.to_string())?;

    if arguments.verify_only {
        return verify(&conversion).map_err(|error| error.to_string());
    }
    convert(&conversion, &system_ldconfig()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::Arguments;

    #[test]
    fn a_document_and_a_mode_are_parsed() {
        let arguments = Arguments::parse(
            ["--input", "/tmp/input.json"]
                .into_iter()
                .map(ToOwned::to_owned),
        )
        .expect("parsed");
        assert_eq!(arguments.input.to_string_lossy(), "/tmp/input.json");
        assert!(!arguments.verify_only);
    }

    #[test]
    fn verify_only_is_recognised() {
        let arguments = Arguments::parse(
            ["--input", "/tmp/input.json", "--verify-only"]
                .into_iter()
                .map(ToOwned::to_owned),
        )
        .expect("parsed");
        assert!(arguments.verify_only);
    }

    #[test]
    fn a_missing_input_is_refused_with_the_usage() {
        let error = Arguments::parse(std::iter::empty()).expect_err("refused");
        assert!(error.contains("--input"), "{error}");
    }
}
