//! Asking this guest whether anything renders on the GPU it was given.
//!
//! What decides is in `gpu_probe`; what is here is the part that needs a guest
//! with a device in it: opening `/dev/dxg`, looking for the libraries a
//! renderer opens, installing the two programs that can hold a GL context and
//! a Vulkan instance, and running them under the environment the recipe wrote.
//!
//! The agent cannot do the rendering itself. It is a statically linked musl
//! binary with no C toolchain behind it, so it can neither link nor `dlopen`
//! `libEGL` or `libvulkan`, and every real operation on this GPU is therefore
//! another program run with a budget.
//!
//! Nothing here fails as a whole. Every check that does not succeed is a check
//! in the report and a VM that keeps running with less GPU than it asked for.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use vmlord_agent_protocol::v1::{GpuProbeStep, ProbeGpuResponse};

use crate::{
    command::{self, Outcome},
    gpu_kernel::{device_is_usable, guest_facts},
    gpu_probe::{
        Checks, Renderer, classify, eglinfo_renderers, hardware_renderer, required_libraries,
        shell_command, verdict, vulkaninfo_devices,
    },
    gpu_recipe::{MesaPolicy, library_triplet, module_is_loaded, parse_mesa_policy},
    gpu_targets::{PAYLOAD, WSL_LIB},
    guest_files::{failure, read},
};

/// The kernel module behind the device.
const MODULE: &str = "dxgkrnl";

/// Where a bundled Mesa was staged by the recipe.
const MESA_PREFIX: &str = "/opt/vmlord/wsl-mesa";

/// Where the kernel puts the DRM nodes a guest may or may not have.
const DRM: &str = "/dev/dri";

/// The program that holds a GL context for the OpenGL check.
const OPENGL_TOOL: &str = "/usr/bin/eglinfo";

/// The program that holds a Vulkan instance for the Vulkan check.
const VULKAN_TOOL: &str = "/usr/bin/vulkaninfo";

/// The packages the two programs above come in.
///
/// Mesa's and Khronos's own, never a vendor's: that is what makes the probe
/// read the same on a host with an NVIDIA, an AMD or an Intel adapter behind
/// `/dev/dxg`.
const TOOL_PACKAGES: [&str; 2] = ["mesa-utils", "vulkan-tools"];

/// The vendor tools that are worth quoting when they happen to be there.
///
/// Diagnostics only. A guest whose vendor tool prints an adapter and whose
/// Mesa renders on llvmpipe is not ready, and a guest with no vendor tool at
/// all that draws through d3d12 is.
const VENDOR_TOOLS: [&str; 2] = ["nvidia-smi", "rocm-smi"];

const APT_BUDGET: Duration = Duration::from_secs(300);
const PROBE_BUDGET: Duration = Duration::from_secs(60);
const VENDOR_BUDGET: Duration = Duration::from_secs(15);

/// Looks at this guest's GPU and says what it found.
///
/// Called once per session, after the recipe of the same session: the probe
/// asks about a userspace the recipe has just installed.
pub fn probe(stopping: &AtomicBool) -> ProbeGpuResponse {
    let mut checks = Checks::new();
    let mut found: Vec<Renderer> = Vec::new();

    let device = device_is_usable();
    if !device {
        checks.failed(
            GpuProbeStep::Device,
            "/dev/dxg is missing, is not a character device, or will not open",
        );
        return report(
            checks,
            "/dev/dxg never opened",
            device,
            &found,
            String::new(),
        );
    }
    checks.ok(GpuProbeStep::Device, "/dev/dxg is a usable device");

    if halted(stopping) {
        return report(
            checks,
            "the guest is shutting down",
            device,
            &found,
            String::new(),
        );
    }

    let driver = module_check(&mut checks);
    libraries_check(&mut checks);

    if halted(stopping) {
        return report(checks, "the guest is shutting down", device, &found, driver);
    }

    if tools_check(&mut checks) {
        found.extend(opengl_check(&mut checks));
        found.extend(vulkan_check(&mut checks));
    } else {
        for step in [GpuProbeStep::Opengl, GpuProbeStep::Vulkan] {
            checks.skipped(step, "the probe programs are not installed");
        }
    }

    vendor_check(&mut checks);

    report(
        checks,
        "the probe did not need this check",
        device,
        &found,
        driver,
    )
}

/// Whether the guest is going down, which ends the probe where it stands.
///
/// The programs below take seconds rather than the minutes a kernel build
/// does, and systemd is still holding the guest open for this process to exit.
fn halted(stopping: &AtomicBool) -> bool {
    stopping.load(Ordering::Relaxed)
}

/// The finished report, with the verdict the checks add up to.
fn report(
    checks: Checks,
    reason: &str,
    device: bool,
    found: &[Renderer],
    driver: String,
) -> ProbeGpuResponse {
    let hardware = hardware_renderer(found);

    ProbeGpuResponse {
        verdict: i32::from(verdict(device, hardware)),
        checks: checks.finish(reason),
        renderer: hardware.unwrap_or_default().to_owned(),
        driver,
        render_node: render_node().unwrap_or_default(),
    }
}

/// Whether the module behind the device is loaded, and what it is called.
fn module_check(checks: &mut Checks) -> String {
    if module_is_loaded(&read(Path::new("/proc/modules")), MODULE) {
        checks.ok(GpuProbeStep::KernelModule, format!("{MODULE} is loaded"));
        return MODULE.to_owned();
    }

    // A device that opens without the module that creates it is a device node
    // left behind, and worth saying so rather than naming a driver anyway.
    checks.failed(
        GpuProbeStep::KernelModule,
        format!("{MODULE} is not in /proc/modules"),
    );
    String::new()
}

/// Whether the files a renderer opens are there.
///
/// Never ends the probe: the renderers are what decide, and a library check
/// that was wrong about a path must not veto a guest that draws.
fn libraries_check(checks: &mut Checks) {
    let Ok(guest) = guest_facts() else {
        checks.skipped(
            GpuProbeStep::Libraries,
            "this guest does not say what it is, so there is no library path to look in",
        );
        return;
    };
    let Some(triplet) = library_triplet(&guest.architecture) else {
        checks.skipped(
            GpuProbeStep::Libraries,
            format!(
                "vmlord-agent has no library path for architecture {}",
                guest.architecture
            ),
        );
        return;
    };

    let prefix = match parse_mesa_policy(&read(&Path::new(PAYLOAD).join("sources.json"))) {
        Ok(MesaPolicy::Bundled) => Some(MESA_PREFIX),
        // A payload that is not mounted, or one whose policy this build cannot
        // read, is not a reason to look nowhere: the distribution's own path is
        // where a guest without a staged Mesa has its driver.
        Ok(MesaPolicy::Distro) | Err(_) => None,
    };

    let required = required_libraries(triplet, prefix);
    let missing: Vec<&str> = required
        .iter()
        .filter(|path| !Path::new(path).exists())
        .map(String::as_str)
        .collect();

    if missing.is_empty() {
        checks.ok(
            GpuProbeStep::Libraries,
            format!("every library a renderer opens is there, including {WSL_LIB}/libd3d12.so"),
        );
    } else {
        checks.failed(
            GpuProbeStep::Libraries,
            format!("a renderer would not find {}", missing.join(", ")),
        );
    }
}

/// Makes sure the two programs the renderer checks run are installed.
///
/// Present-first, as every apt stage in this recipe is: the second start of a
/// VM installs nothing and needs no network.
fn tools_check(checks: &mut Checks) -> bool {
    if tools_are_present() {
        checks.skipped(
            GpuProbeStep::Tools,
            format!("{OPENGL_TOOL} and {VULKAN_TOOL} are already installed"),
        );
        return true;
    }

    let mut outcome = apt_tools();
    if !outcome.succeeded() {
        // A cloud image's package lists are as old as the image.
        let _ = command::run(
            "apt-get",
            &["update"],
            &[("DEBIAN_FRONTEND", "noninteractive")],
            APT_BUDGET,
        );
        outcome = apt_tools();
    }

    if tools_are_present() {
        checks.ok(
            GpuProbeStep::Tools,
            format!("installed {}", TOOL_PACKAGES.join(" and ")),
        );
        true
    } else {
        checks.failed(GpuProbeStep::Tools, failure("apt-get install", &outcome));
        false
    }
}

fn tools_are_present() -> bool {
    Path::new(OPENGL_TOOL).exists() && Path::new(VULKAN_TOOL).exists()
}

fn apt_tools() -> Outcome {
    let mut arguments = vec!["install", "-y"];
    arguments.extend(TOOL_PACKAGES);
    command::run(
        "apt-get",
        &arguments,
        &[("DEBIAN_FRONTEND", "noninteractive")],
        APT_BUDGET,
    )
}

/// A bounded OpenGL operation, and what rendered it.
fn opengl_check(checks: &mut Checks) -> Vec<Renderer> {
    let outcome = run_probe("eglinfo -B");
    let found: Vec<Renderer> = eglinfo_renderers(&outcome.output)
        .iter()
        .filter_map(|name| classify(name))
        .collect();

    if found.is_empty() {
        checks.failed(
            GpuProbeStep::Opengl,
            format!("eglinfo named no renderer: {}", outcome.output),
        );
        return found;
    }

    match hardware_renderer(&found) {
        Some(name) => checks.ok(GpuProbeStep::Opengl, format!("GL renders on {name}")),
        None => checks.failed(
            GpuProbeStep::Opengl,
            format!("GL renders on the CPU: {}", names(&found)),
        ),
    }
    found
}

/// A bounded Vulkan operation, and what answered it.
fn vulkan_check(checks: &mut Checks) -> Vec<Renderer> {
    let outcome = run_probe("vulkaninfo --summary");
    let devices = vulkaninfo_devices(&outcome.output);
    if devices.is_empty() {
        checks.failed(
            GpuProbeStep::Vulkan,
            format!("vulkaninfo named no device: {}", outcome.output),
        );
        return Vec::new();
    }

    // A device that says it is a CPU is software however it is named, so the
    // type decides and the name only describes.
    let found: Vec<Renderer> = devices
        .iter()
        .map(|device| {
            let name = if device.name.is_empty() {
                device.driver.clone()
            } else {
                device.name.clone()
            };
            if device.is_cpu {
                Renderer::Software(name)
            } else {
                classify(&name).unwrap_or(Renderer::Software(name))
            }
        })
        .collect();

    match hardware_renderer(&found) {
        Some(name) => checks.ok(GpuProbeStep::Vulkan, format!("Vulkan renders on {name}")),
        None => checks.failed(
            GpuProbeStep::Vulkan,
            format!("Vulkan renders on the CPU: {}", names(&found)),
        ),
    }
    found
}

/// Whatever a vendor tool has to say, when the mounted userspace carries one.
fn vendor_check(checks: &mut Checks) {
    for tool in VENDOR_TOOLS {
        let path = PathBuf::from(WSL_LIB).join(tool);
        if !path.exists() {
            continue;
        }
        let outcome = command::run(&path.to_string_lossy(), &["-L"], &[], VENDOR_BUDGET);
        let first = outcome.output.lines().next().unwrap_or_default().trim();
        checks.ok(GpuProbeStep::Vendor, format!("{tool}: {first}"));
        return;
    }

    checks.skipped(
        GpuProbeStep::Vendor,
        format!("the mounted userspace in {WSL_LIB} carries no vendor tool"),
    );
}

/// Runs one probe program under the environment the recipe wrote.
fn run_probe(program: &str) -> Outcome {
    command::run(
        "/bin/sh",
        &["-c", &shell_command(program)],
        &[],
        PROBE_BUDGET,
    )
}

/// The renderers of a list, for a message a person reads.
fn names(found: &[Renderer]) -> String {
    found
        .iter()
        .map(Renderer::name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The DRM render node this guest has, when it has one.
///
/// The d3d12 path needs none, so this is reported and never required.
fn render_node() -> Option<String> {
    let mut nodes: Vec<String> = fs::read_dir(DRM)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("renderD"))
        .collect();
    nodes.sort();
    nodes.first().map(|name| format!("{DRM}/{name}"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use vmlord_agent_protocol::v1::{GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict};

    use super::probe;
    use crate::gpu_probe::CHECKS;

    /// Every test here probes a guest that is shutting down.
    ///
    /// Not for the shutdown's sake: it is what keeps a `cargo test` from
    /// running `apt-get` on the machine doing the build. The rest of the probe
    /// needs a Hyper-V guest with a payload mounted and is proven by hand on
    /// one, as the module build in #95 was.
    fn stopping() -> AtomicBool {
        AtomicBool::new(true)
    }

    #[test]
    fn a_probe_reports_every_check_exactly_once_and_in_order() {
        // The host reads this list by position as much as by step, and a
        // check the probe forgot would be a check the host never logs.
        let report = probe(&stopping());

        assert_eq!(report.checks.len(), CHECKS.len());
        for (check, step) in report.checks.iter().zip(CHECKS) {
            assert_eq!(check.step(), step);
        }
    }

    #[test]
    fn a_guest_that_is_shutting_down_runs_no_program() {
        // systemd is holding the guest open for this process to exit, and
        // nothing after the device is worth two programs and an apt.
        let report = probe(&stopping());

        for check in &report.checks[1..] {
            assert_eq!(
                check.state(),
                GpuProbeCheckState::Skipped,
                "{:?} ran while the guest was shutting down",
                check.step()
            );
        }
        // Nothing rendered because nothing was asked to, whatever this machine
        // happens to have at /dev/dxg.
        assert_ne!(report.verdict(), GpuProbeVerdict::Renders);
        assert!(report.renderer.is_empty());
    }

    #[test]
    fn the_device_decides_whether_there_is_a_gpu_at_all() {
        // The one check that ends the probe: a verdict of `NO_DEVICE` and a
        // failed `DEVICE` are the same fact, and they must not be able to
        // disagree.
        let report = probe(&stopping());

        let device = report.checks[0].state();
        assert_eq!(report.checks[0].step(), GpuProbeStep::Device);
        assert_eq!(
            report.verdict() == GpuProbeVerdict::NoDevice,
            device == GpuProbeCheckState::Failed
        );
    }
}
