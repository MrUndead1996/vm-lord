//! Enumerating the host's GPU partition adapters through SetupAPI.

use std::path::{Path, PathBuf};

use vmlord_core::{HostGpuAdapter, RepositoryError};
use windows::{
    Win32::{
        Devices::{
            DeviceAndDriverInstallation::{
                CM_Get_Device_IDW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
                MAX_DEVICE_ID_LEN, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
                SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
                SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW, SetupDiGetDevicePropertyW,
                SetupGetInfDriverStoreLocationW,
            },
            Properties::{
                DEVPKEY_Device_DeviceDesc, DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_Service,
                DEVPROPTYPE,
            },
        },
        Foundation::{DEVPROPKEY, ERROR_NO_MORE_ITEMS},
    },
    core::{GUID, PCWSTR},
};

use crate::error::windows_error;

/// The GPU Partition Adapter device interface class.
///
/// Not published in any SDK header: it is what Hyper-V presents partitionable
/// adapters under, and it is the same constant the AppSandbox backend used.
const GUID_GPU_PARTITION_ADAPTER: GUID = GUID::from_u128(0x064092b3_625e_43bf_9eb5_dc845897dd59);

/// Owns an `HDEVINFO` so that no error path can leak it.
struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid set returned by `SetupDiGetClassDevsW`
        // and owned solely by this wrapper, so it is destroyed exactly once.
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// Every GPU partition adapter present on this host.
///
/// An adapter whose driver package cannot be located is still returned: it is
/// a real device, and saying it does not exist would be a different and false
/// answer.
pub(crate) fn partition_adapters() -> Result<Vec<HostGpuAdapter>, RepositoryError> {
    // SAFETY: The GUID is a valid interface class; a null enumerator and a
    // null parent window request every present device of that class.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_GPU_PARTITION_ADAPTER),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    }
    .map_err(|error| windows_error("enumerate GPU partition adapters", None, error))?;
    let set = DeviceInfoSet(set);

    let mut adapters = Vec::new();
    for index in 0.. {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: u32::try_from(size_of::<SP_DEVICE_INTERFACE_DATA>()).unwrap_or_default(),
            ..Default::default()
        };

        // SAFETY: `set.0` is live, and `interface` is a correctly sized
        // structure this call fills in.
        let enumerated = unsafe {
            SetupDiEnumDeviceInterfaces(
                set.0,
                None,
                &GUID_GPU_PARTITION_ADAPTER,
                index,
                &raw mut interface,
            )
        };
        match enumerated {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_NO_MORE_ITEMS.to_hresult() => break,
            Err(error) => {
                return Err(windows_error(
                    "enumerate a GPU partition adapter interface",
                    None,
                    error,
                ));
            }
        }

        let mut device = SP_DEVINFO_DATA {
            cbSize: u32::try_from(size_of::<SP_DEVINFO_DATA>()).unwrap_or_default(),
            ..Default::default()
        };
        let Some(interface_path) = interface_detail(&set, &interface, &mut device)? else {
            continue;
        };

        adapters.push(HostGpuAdapter {
            name: device_property(&set, &device, &DEVPKEY_Device_DeviceDesc)
                .unwrap_or_else(|| "GPU partition adapter".to_owned()),
            instance_id: device_instance_id(&device)?,
            interface_path,
            driver_store: device_property(&set, &device, &DEVPKEY_Device_DriverInfPath)
                .and_then(|inf| driver_store_location(&inf))
                .as_deref()
                .and_then(driver_store_directory),
            service: device_property(&set, &device, &DEVPKEY_Device_Service),
        });
    }

    Ok(adapters)
}

/// The device path of one interface, and the device behind it.
///
/// The detail structure is variable length -- a fixed header followed by as
/// many characters as the path needs -- so the buffer is sized by the API
/// itself rather than guessed at.
fn interface_detail(
    set: &DeviceInfoSet,
    interface: &SP_DEVICE_INTERFACE_DATA,
    device: &mut SP_DEVINFO_DATA,
) -> Result<Option<String>, RepositoryError> {
    let mut required = 0_u32;
    // SAFETY: Passing no buffer asks only for the size; the call is expected
    // to fail with ERROR_INSUFFICIENT_BUFFER and to fill `required`.
    let _ = unsafe {
        SetupDiGetDeviceInterfaceDetailW(set.0, interface, None, 0, Some(&raw mut required), None)
    };
    if required == 0 {
        return Ok(None);
    }

    let mut buffer = vec![0_u8; required as usize];
    let detail = buffer
        .as_mut_ptr()
        .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    // SAFETY: `buffer` is at least `required` bytes, which is what the call
    // above asked for. `cbSize` is the size of the fixed header, not of the
    // buffer -- SetupAPI demands exactly that.
    unsafe {
        (*detail).cbSize =
            u32::try_from(size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>()).unwrap_or_default();
        SetupDiGetDeviceInterfaceDetailW(
            set.0,
            interface,
            Some(detail),
            required,
            None,
            Some(&raw mut *device),
        )
    }
    .map_err(|error| windows_error("read a GPU adapter interface path", None, error))?;

    // SAFETY: `DevicePath` is a null-terminated UTF-16 string that runs to the
    // end of the buffer the call just filled.
    let path = unsafe { PCWSTR::from_raw((&raw const (*detail).DevicePath).cast()).to_string() }
        .map_err(|error| {
            RepositoryError::new(format!(
                "a GPU adapter interface path was not UTF-16: {error}"
            ))
        })?;

    Ok(Some(path))
}

/// One string property of a device, or `None` when it is absent.
///
/// A missing property is not an error: these are diagnostics, and an adapter
/// that does not name its service is still an adapter.
fn device_property(
    set: &DeviceInfoSet,
    device: &SP_DEVINFO_DATA,
    key: &DEVPROPKEY,
) -> Option<String> {
    let mut property_type = DEVPROPTYPE::default();
    let mut required = 0_u32;
    // SAFETY: Passing no buffer asks only for the size.
    let _ = unsafe {
        SetupDiGetDevicePropertyW(
            set.0,
            device,
            key,
            &raw mut property_type,
            None,
            Some(&raw mut required),
            0,
        )
    };
    if required == 0 {
        return None;
    }

    let mut buffer = vec![0_u8; required as usize];
    // SAFETY: `buffer` is exactly the size the call above asked for.
    unsafe {
        SetupDiGetDevicePropertyW(
            set.0,
            device,
            key,
            &raw mut property_type,
            Some(&mut buffer),
            None,
            0,
        )
    }
    .ok()?;

    decode_wide_property(&buffer)
}

/// The device instance id, which is what names an adapter across reboots.
fn device_instance_id(device: &SP_DEVINFO_DATA) -> Result<String, RepositoryError> {
    let mut buffer = [0_u16; MAX_DEVICE_ID_LEN as usize + 1];
    // SAFETY: `buffer` is at least `MAX_DEVICE_ID_LEN` characters, which is
    // the documented maximum for a device instance id.
    let result = unsafe { CM_Get_Device_IDW(device.DevInst, &mut buffer, 0) };
    if result != CR_SUCCESS {
        return Err(RepositoryError::new(format!(
            "reading a GPU adapter instance id failed (CONFIGRET {})",
            result.0
        )));
    }

    let end = buffer.iter().position(|unit| *unit == 0).unwrap_or(0);
    Ok(String::from_utf16_lossy(&buffer[..end]))
}

/// Where the DriverStore keeps the INF a device was installed from.
fn driver_store_location(inf_name: &str) -> Option<String> {
    let name: Vec<u16> = inf_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buffer = [0_u16; 512];
    // SAFETY: `name` is null-terminated, and `buffer` is passed as a sized
    // slice the call fills in.
    unsafe {
        SetupGetInfDriverStoreLocationW(
            PCWSTR::from_raw(name.as_ptr()),
            None,
            PCWSTR::null(),
            &mut buffer,
            None,
        )
    }
    .ok()?;

    let end = buffer.iter().position(|unit| *unit == 0).unwrap_or(0);
    (end > 0).then(|| String::from_utf16_lossy(&buffer[..end]))
}

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
