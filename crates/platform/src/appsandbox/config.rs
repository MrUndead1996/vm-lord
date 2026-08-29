use std::path::{Path, PathBuf};

use vmlord_core::RepositoryError;

/// AppSandbox configuration for one VM, kept private to the platform layer.
pub(super) struct ParsedVm {
    ordinal: usize,
    name: String,
    os_type: String,
    ram_mb: u32,
    cpu_cores: u32,
    hdd_gb: u32,
    network_mode: u32,
    gpu_mode: u32,
    admin_user: String,
    ssh_enabled: u32,
    ssh_port: u16,
    ssh_deploy_key: u32,
    install_complete: u32,
    vhdx_path: PathBuf,
}

impl ParsedVm {
    #[must_use]
    pub(super) fn ordinal(&self) -> usize {
        self.ordinal
    }

    #[must_use]
    pub(super) fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub(super) fn os_type(&self) -> &str {
        &self.os_type
    }

    #[must_use]
    pub(super) fn ram_mb(&self) -> u32 {
        self.ram_mb
    }

    #[must_use]
    pub(super) fn cpu_cores(&self) -> u32 {
        self.cpu_cores
    }

    #[must_use]
    pub(super) fn hdd_gb(&self) -> u32 {
        self.hdd_gb
    }

    #[must_use]
    pub(super) fn network_mode(&self) -> u32 {
        self.network_mode
    }

    #[must_use]
    pub(super) fn gpu_mode(&self) -> u32 {
        self.gpu_mode
    }

    #[must_use]
    pub(super) fn admin_user(&self) -> &str {
        &self.admin_user
    }

    #[must_use]
    pub(super) fn ssh_enabled(&self) -> u32 {
        self.ssh_enabled
    }

    #[must_use]
    pub(super) fn ssh_port(&self) -> u16 {
        self.ssh_port
    }

    #[must_use]
    pub(super) fn ssh_deploy_key(&self) -> u32 {
        self.ssh_deploy_key
    }

    #[must_use]
    pub(super) fn install_complete(&self) -> u32 {
        self.install_complete
    }

    #[must_use]
    pub(super) fn vhdx_path(&self) -> &Path {
        &self.vhdx_path
    }
}

/// Parses the VM sections from an AppSandbox `vms.cfg` file.
pub(super) fn parse_vms_cfg(input: &str) -> Result<Vec<ParsedVm>, RepositoryError> {
    let mut parser = Parser::default();
    for (index, raw) in input.lines().enumerate() {
        parser.consume(index + 1, raw.trim_end_matches('\r'))?;
    }
    parser.finish()
}

#[derive(Default)]
struct Parser {
    current_vm: Option<VmBuilder>,
    parsed: Vec<ParsedVm>,
    vm_ordinal: usize,
    last_line: usize,
}

impl Parser {
    fn consume(&mut self, line_number: usize, raw: &str) -> Result<(), RepositoryError> {
        self.last_line = line_number;
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            return Ok(());
        }

        if line.starts_with('[') {
            self.consume_section(line_number, line)
        } else {
            self.consume_field(line_number, line)
        }
    }

    fn consume_section(&mut self, line_number: usize, line: &str) -> Result<(), RepositoryError> {
        let Some(section) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        else {
            return Err(parse_error(line_number, "invalid section header"));
        };

        self.finish_current(line_number)?;
        if section.trim() == "VM" {
            self.vm_ordinal += 1;
            self.current_vm = Some(VmBuilder::new(self.vm_ordinal));
        }
        Ok(())
    }

    fn consume_field(&mut self, line_number: usize, line: &str) -> Result<(), RepositoryError> {
        let Some(vm) = &mut self.current_vm else {
            return Ok(());
        };
        let Some((key, value)) = line.split_once('=') else {
            return Err(parse_error(
                line_number,
                "expected a key=value field in [VM]",
            ));
        };

        vm.set(line_number, key.trim(), value.trim())
    }

    fn finish_current(&mut self, line_number: usize) -> Result<(), RepositoryError> {
        let Some(vm) = self.current_vm.take() else {
            return Ok(());
        };
        self.parsed.push(vm.finish(line_number)?);
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<ParsedVm>, RepositoryError> {
        self.finish_current(self.last_line.max(1))?;
        Ok(self.parsed)
    }
}

struct VmBuilder {
    ordinal: usize,
    name: Option<String>,
    os_type: Option<String>,
    ram_mb: Option<u32>,
    cpu_cores: Option<u32>,
    hdd_gb: Option<u32>,
    network_mode: Option<u32>,
    gpu_mode: Option<u32>,
    admin_user: Option<String>,
    ssh_enabled: Option<u32>,
    ssh_port: Option<u16>,
    ssh_deploy_key: Option<u32>,
    install_complete: Option<u32>,
    vhdx_path: Option<PathBuf>,
}

impl VmBuilder {
    fn new(ordinal: usize) -> Self {
        Self {
            ordinal,
            name: None,
            os_type: None,
            ram_mb: None,
            cpu_cores: None,
            hdd_gb: None,
            network_mode: None,
            gpu_mode: None,
            admin_user: None,
            ssh_enabled: None,
            ssh_port: None,
            ssh_deploy_key: None,
            install_complete: None,
            vhdx_path: None,
        }
    }

    fn set(&mut self, line_number: usize, key: &str, value: &str) -> Result<(), RepositoryError> {
        match key {
            "Name" => set_string(&mut self.name, self.ordinal, line_number, key, value),
            "OsType" => set_string(&mut self.os_type, self.ordinal, line_number, key, value),
            "RamMB" => set_integer(&mut self.ram_mb, self.ordinal, line_number, key, value),
            "CpuCores" => set_integer(&mut self.cpu_cores, self.ordinal, line_number, key, value),
            "HddGB" => set_integer(&mut self.hdd_gb, self.ordinal, line_number, key, value),
            "NetworkMode" => set_integer(
                &mut self.network_mode,
                self.ordinal,
                line_number,
                key,
                value,
            ),
            "GpuMode" => set_integer(&mut self.gpu_mode, self.ordinal, line_number, key, value),
            "AdminUser" => set_string(&mut self.admin_user, self.ordinal, line_number, key, value),
            "SshEnabled" => {
                set_integer(&mut self.ssh_enabled, self.ordinal, line_number, key, value)
            }
            "SshPort" => set_integer(&mut self.ssh_port, self.ordinal, line_number, key, value),
            "SshDeployKey" => set_integer(
                &mut self.ssh_deploy_key,
                self.ordinal,
                line_number,
                key,
                value,
            ),
            "InstallComplete" => set_integer(
                &mut self.install_complete,
                self.ordinal,
                line_number,
                key,
                value,
            ),
            "VhdxPath" => set_path(&mut self.vhdx_path, self.ordinal, line_number, key, value),
            _ => Ok(()),
        }
    }

    fn finish(self, line_number: usize) -> Result<ParsedVm, RepositoryError> {
        Ok(ParsedVm {
            ordinal: self.ordinal,
            name: required(self.name, self.ordinal, line_number, "Name")?,
            os_type: required(self.os_type, self.ordinal, line_number, "OsType")?,
            ram_mb: required(self.ram_mb, self.ordinal, line_number, "RamMB")?,
            cpu_cores: required(self.cpu_cores, self.ordinal, line_number, "CpuCores")?,
            hdd_gb: required(self.hdd_gb, self.ordinal, line_number, "HddGB")?,
            network_mode: required(self.network_mode, self.ordinal, line_number, "NetworkMode")?,
            gpu_mode: required(self.gpu_mode, self.ordinal, line_number, "GpuMode")?,
            admin_user: required(self.admin_user, self.ordinal, line_number, "AdminUser")?,
            ssh_enabled: required(self.ssh_enabled, self.ordinal, line_number, "SshEnabled")?,
            ssh_port: required(self.ssh_port, self.ordinal, line_number, "SshPort")?,
            ssh_deploy_key: required(
                self.ssh_deploy_key,
                self.ordinal,
                line_number,
                "SshDeployKey",
            )?,
            install_complete: required(
                self.install_complete,
                self.ordinal,
                line_number,
                "InstallComplete",
            )?,
            vhdx_path: required(self.vhdx_path, self.ordinal, line_number, "VhdxPath")?,
        })
    }
}

fn set_string(
    slot: &mut Option<String>,
    ordinal: usize,
    line_number: usize,
    key: &str,
    value: &str,
) -> Result<(), RepositoryError> {
    if slot.is_some() {
        return Err(duplicate_key(line_number, ordinal, key));
    }
    if value.is_empty() {
        return Err(parse_error(
            line_number,
            format!("AppSandbox VM {ordinal} has an empty {key}"),
        ));
    }
    *slot = Some(value.to_owned());
    Ok(())
}

fn set_path(
    slot: &mut Option<PathBuf>,
    ordinal: usize,
    line_number: usize,
    key: &str,
    value: &str,
) -> Result<(), RepositoryError> {
    if slot.is_some() {
        return Err(duplicate_key(line_number, ordinal, key));
    }
    if value.is_empty() {
        return Err(parse_error(
            line_number,
            format!("AppSandbox VM {ordinal} has an empty {key}"),
        ));
    }
    *slot = Some(PathBuf::from(value));
    Ok(())
}

fn set_integer<T>(
    slot: &mut Option<T>,
    ordinal: usize,
    line_number: usize,
    key: &str,
    value: &str,
) -> Result<(), RepositoryError>
where
    T: std::str::FromStr,
{
    if slot.is_some() {
        return Err(duplicate_key(line_number, ordinal, key));
    }
    *slot = Some(value.parse().map_err(|_| {
        parse_error(
            line_number,
            format!("AppSandbox VM {ordinal} has an invalid integer for {key}"),
        )
    })?);
    Ok(())
}

fn required<T>(
    value: Option<T>,
    ordinal: usize,
    line_number: usize,
    key: &str,
) -> Result<T, RepositoryError> {
    value.ok_or_else(|| {
        parse_error(
            line_number,
            format!("AppSandbox VM {ordinal} is missing required {key}"),
        )
    })
}

fn duplicate_key(line_number: usize, ordinal: usize, key: &str) -> RepositoryError {
    parse_error(
        line_number,
        format!("AppSandbox VM {ordinal} has a duplicate required {key}"),
    )
}

fn parse_error(line_number: usize, detail: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::new(format!("AppSandbox vms.cfg line {line_number}: {detail}"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{ParsedVm, parse_vms_cfg};

    #[test]
    fn parses_two_vm_sections_without_leaking_settings_fields() {
        let parsed = parse_vms_cfg(include_str!(
            "../../tests/fixtures/appsandbox/two-linux.cfg"
        ))
        .expect("fixture is valid");

        assert_eq!(
            parsed.iter().map(ParsedVm::name).collect::<Vec<_>>(),
            ["ubuntu", "fedora"]
        );
    }

    #[test]
    fn preserves_the_fields_discovery_needs_from_a_linux_vm() {
        let parsed = parse_vms_cfg(include_str!(
            "../../tests/fixtures/appsandbox/one-linux.cfg"
        ))
        .expect("fixture is valid");
        let vm = parsed.first().expect("one VM should be parsed");

        assert_eq!(vm.ordinal(), 1);
        assert_eq!(vm.name(), "ubuntu");
        assert_eq!(vm.os_type(), "Linux");
        assert_eq!(vm.ram_mb(), 4096);
        assert_eq!(vm.cpu_cores(), 4);
        assert_eq!(vm.hdd_gb(), 64);
        assert_eq!(vm.network_mode(), 1);
        assert_eq!(vm.gpu_mode(), 1);
        assert_eq!(vm.admin_user(), "ubuntu");
        assert_eq!(vm.ssh_enabled(), 1);
        assert_eq!(vm.ssh_port(), 22);
        assert_eq!(vm.ssh_deploy_key(), 1);
        assert_eq!(vm.install_complete(), 1);
        assert_eq!(
            vm.vhdx_path(),
            Path::new(r"C:\ProgramData\AppSandbox\ubuntu\disk.vhdx")
        );
    }

    #[test]
    fn rejects_a_duplicate_required_key_at_its_line() {
        let error = parser_error("duplicate-name.cfg");

        assert!(error.to_string().contains("line 4"), "{error}");
        assert!(error.to_string().contains("Name"), "{error}");
    }

    #[test]
    fn rejects_a_vm_missing_its_vhdx_path() {
        let error = parser_error("missing-vhdx-path.cfg");

        assert!(error.to_string().contains("line 13"), "{error}");
        assert!(error.to_string().contains("VhdxPath"), "{error}");
    }

    #[test]
    fn rejects_malformed_integers_at_their_line() {
        let error = parser_error("malformed-integer.cfg");

        assert!(error.to_string().contains("line 4"), "{error}");
        assert!(error.to_string().contains("RamMB"), "{error}");
    }

    #[test]
    fn accepts_crlf_input() {
        let parsed = parse_vms_cfg(include_str!(
            "../../tests/fixtures/appsandbox/crlf-linux.cfg"
        ))
        .expect("CRLF configuration is valid");

        assert_eq!(parsed[0].name(), "crlf-ubuntu");
    }

    #[test]
    fn preserves_a_windows_vm_for_compatibility_reporting() {
        let parsed = parse_vms_cfg(include_str!("../../tests/fixtures/appsandbox/windows.cfg"))
            .expect("Windows VMs remain discoverable");

        assert_eq!(parsed[0].os_type(), "Windows");
    }

    fn parser_error(fixture: &str) -> vmlord_core::RepositoryError {
        let input = match fixture {
            "duplicate-name.cfg" => {
                include_str!("../../tests/fixtures/appsandbox/duplicate-name.cfg")
            }
            "missing-vhdx-path.cfg" => {
                include_str!("../../tests/fixtures/appsandbox/missing-vhdx-path.cfg")
            }
            "malformed-integer.cfg" => {
                include_str!("../../tests/fixtures/appsandbox/malformed-integer.cfg")
            }
            _ => panic!("unknown parser fixture: {fixture}"),
        };

        match parse_vms_cfg(input) {
            Ok(_) => panic!("{fixture} should be rejected"),
            Err(error) => error,
        }
    }
}
