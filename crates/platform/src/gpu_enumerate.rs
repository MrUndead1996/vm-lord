//! Enumerating the host's GPU partition adapters through SetupAPI.

use std::path::{Path, PathBuf};

/// The driver package directory holding an INF that SetupAPI located.
///
/// `SetupGetInfDriverStoreLocationW` answers with the INF file itself; what a
/// guest is given is the directory around it, since a driver package is every
/// file in that folder and not the INF alone.
pub(crate) fn driver_store_directory(inf_location: &str) -> Option<PathBuf> {
    let parent = Path::new(inf_location).parent()?;
    (!parent.as_os_str().is_empty()).then(|| parent.to_path_buf())
}

/// Reads a `DEVPROP_TYPE_STRING` property buffer.
///
/// The buffer is UTF-16 and usually null-terminated; the terminator is not
/// guaranteed by the API contract, so text runs either to the first null or to
/// the end of the buffer. An empty string is `None`: a property that is
/// present and blank tells a reader nothing an absent one does not.
pub(crate) fn decode_wide_property(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();

    let text = String::from_utf16_lossy(&units);
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{decode_wide_property, driver_store_directory};

    fn wide(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    #[test]
    fn a_driver_store_location_becomes_its_package_directory() {
        assert_eq!(
            driver_store_directory(
                r"C:\Windows\System32\DriverStore\FileRepository\nv_dispi.inf_amd64_1234\nv_dispi.inf"
            ),
            Some(PathBuf::from(
                r"C:\Windows\System32\DriverStore\FileRepository\nv_dispi.inf_amd64_1234"
            ))
        );
    }

    #[test]
    fn a_location_without_a_directory_resolves_to_nothing() {
        assert_eq!(driver_store_directory("nv_dispi.inf"), None);
        assert_eq!(driver_store_directory(""), None);
    }

    #[test]
    fn a_wide_property_is_read_up_to_its_terminator() {
        let mut bytes = wide("Microsoft Virtual Render Driver");
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        assert_eq!(
            decode_wide_property(&bytes).as_deref(),
            Some("Microsoft Virtual Render Driver")
        );
    }

    #[test]
    fn an_unterminated_wide_property_is_still_read() {
        let bytes: Vec<u8> = "nvlddmkm"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();

        assert_eq!(decode_wide_property(&bytes).as_deref(), Some("nvlddmkm"));
    }

    #[test]
    fn an_empty_or_odd_length_property_is_nothing() {
        assert_eq!(decode_wide_property(&[]), None);
        assert_eq!(decode_wide_property(&wide("")), None);
        assert_eq!(decode_wide_property(&[0x41]), None);
    }
}
