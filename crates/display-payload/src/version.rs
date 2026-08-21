//! A display payload's own version.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use vmlord_payload::PayloadError;

/// A payload's version, independent of VMLord's.
///
/// Three numbers and nothing else: a payload that reaches a release is
/// released, so there is no pre-release to order and no build metadata to
/// ignore. `Ord` is what "the newest version wins" is decided by, and it reads
/// the numbers rather than the text -- `0.10.0` is newer than `0.9.9`, which
/// sorting strings gets wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PayloadVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl PayloadVersion {
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl FromStr for PayloadVersion {
    type Err = PayloadError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || PayloadError::InvalidCatalog(format!("invalid payload version: {value}"));
        let mut parts = value.split('.');
        let mut number = || {
            parts
                .next()
                .filter(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                .ok_or_else(invalid)?
                .parse::<u32>()
                .map_err(|_| invalid())
        };
        let version = Self {
            major: number()?,
            minor: number()?,
            patch: number()?,
        };
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(version)
    }
}

impl fmt::Display for PayloadVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Serialize for PayloadVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PayloadVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::PayloadVersion;

    #[test]
    fn versions_order_by_number_and_not_by_text() {
        assert!(
            "0.10.0".parse::<PayloadVersion>().unwrap()
                > "0.9.9".parse::<PayloadVersion>().unwrap()
        );
        assert!(
            "1.0.0".parse::<PayloadVersion>().unwrap()
                > "0.99.99".parse::<PayloadVersion>().unwrap()
        );
        assert_eq!(
            "1.2.3".parse::<PayloadVersion>().unwrap().to_string(),
            "1.2.3"
        );
    }

    #[test]
    fn a_version_that_is_not_three_numbers_is_refused() {
        for text in ["1.2", "1.2.3.4", "1.2.x", "v1.2.3", "1.2.3-rc1", "", "1..3"] {
            assert!(
                text.parse::<PayloadVersion>().is_err(),
                "accepted \"{text}\""
            );
        }
    }
}
