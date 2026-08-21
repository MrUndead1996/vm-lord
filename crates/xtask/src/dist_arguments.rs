//! What `cargo dist` was asked to include.

use std::path::PathBuf;

/// One packed payload directory, and which kind of payload it holds.
///
/// Two variants rather than a directory and a flag: the two kinds are staged
/// by different code into different subdirectories, and a release that mixed
/// them up would ship a display payload no GPU catalog can read.
pub(crate) enum DistPayload {
    Gpu(PathBuf),
    Display(PathBuf),
}

/// Reads `cargo dist`'s arguments: zero or more payload directories.
pub(crate) fn parse<I: IntoIterator<Item = String>>(
    arguments: I,
) -> Result<Vec<DistPayload>, String> {
    let mut values = arguments.into_iter();
    let mut payloads = Vec::new();
    while let Some(flag) = values.next() {
        let mut directory = || {
            values
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))
                .map(PathBuf::from)
        };
        let payload = match flag.as_str() {
            "--gpu-payload" => DistPayload::Gpu(directory()?),
            "--display-payload" => DistPayload::Display(directory()?),
            _ => return Err(format!("unknown argument `{flag}`")),
        };
        payloads.push(payload);
    }
    Ok(payloads)
}

#[cfg(test)]
mod tests {
    use super::{DistPayload, parse};

    fn arguments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn a_distribution_may_carry_both_kinds_of_payload() {
        let payloads = parse(arguments(&[
            "--gpu-payload",
            "gpu",
            "--display-payload",
            "display",
        ]))
        .expect("both flags are known");

        assert!(matches!(payloads[0], DistPayload::Gpu(_)));
        assert!(matches!(payloads[1], DistPayload::Display(_)));
    }

    #[test]
    fn a_distribution_may_carry_any_number_of_directories() {
        let payloads = parse(arguments(&["--gpu-payload", "one", "--gpu-payload", "two"]))
            .expect("two of one kind is as ordinary as one of each");

        assert_eq!(payloads.len(), 2);
    }

    #[test]
    fn a_distribution_may_carry_none() {
        assert!(parse(arguments(&[])).unwrap().is_empty());
    }

    #[test]
    fn a_flag_without_a_directory_is_refused() {
        assert!(parse(arguments(&["--display-payload"])).is_err());
        assert!(parse(arguments(&["--payload", "x"])).is_err());
        assert!(parse(arguments(&["built"])).is_err());
    }
}
