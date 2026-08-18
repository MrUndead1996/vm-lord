//! What a probe's output means, decided without a GPU in the room.
//!
//! Everything here is a function of text: the name a renderer gives itself,
//! what `eglinfo` and `vulkaninfo` printed, which files a renderer needs to be
//! able to open. That is what makes the judgement of a probe testable on a
//! machine that is neither Ubuntu nor a Hyper-V guest, while `gpu_render`
//! keeps the parts that need one.

use vmlord_agent_protocol::v1::{GpuProbeCheck, GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict};

use crate::gpu_targets::WSL_LIB;

/// Every check, in the order it is attempted.
///
/// The order is the report's order, and the report is what the host logs, so
/// it is written once here rather than implied by the sequence of calls in
/// `gpu_render`.
pub const CHECKS: [GpuProbeStep; 7] = [
    GpuProbeStep::Device,
    GpuProbeStep::KernelModule,
    GpuProbeStep::Libraries,
    GpuProbeStep::Tools,
    GpuProbeStep::Opengl,
    GpuProbeStep::Vulkan,
    GpuProbeStep::Vendor,
];

/// The names of the renderers that are a CPU pretending to be a GPU.
///
/// A deny list and not an allow list of the drivers this build knows: an allow
/// list would report "no hardware renderer" on the first guest whose stack
/// renders through something nobody here wrote code against, and the failure
/// mode of a deny list is the milder one -- a new software rasteriser counts
/// as hardware once, until its name is added here.
const SOFTWARE: [&str; 5] = ["llvmpipe", "softpipe", "swrast", "lavapipe", "swiftshader"];

/// Where the environment the recipe wrote lives.
const PROFILE: &str = "/etc/profile.d/vmlord-gpu.sh";

/// What a probe found out so far.
///
/// Collected rather than sent as it goes, for the reason a recipe's report is:
/// a check list is one answer to one request.
#[derive(Default)]
pub struct Checks {
    recorded: Vec<GpuProbeCheck>,
}

impl Checks {
    pub fn new() -> Self {
        Self {
            recorded: Vec::with_capacity(CHECKS.len()),
        }
    }

    pub fn ok(&mut self, step: GpuProbeStep, message: impl Into<String>) {
        self.record(step, GpuProbeCheckState::Ok, message.into());
    }

    pub fn skipped(&mut self, step: GpuProbeStep, message: impl Into<String>) {
        self.record(step, GpuProbeCheckState::Skipped, message.into());
    }

    pub fn failed(&mut self, step: GpuProbeStep, message: impl Into<String>) {
        self.record(step, GpuProbeCheckState::Failed, message.into());
    }

    /// Keeps the first answer a check was given.
    ///
    /// Nothing should record a check twice; if something does, the report must
    /// not grow a second copy of a check the host reads once.
    fn record(&mut self, step: GpuProbeStep, state: GpuProbeCheckState, message: String) {
        if self.recorded.iter().any(|check| check.step() == step) {
            return;
        }
        self.recorded.push(GpuProbeCheck {
            step: i32::from(step),
            state: i32::from(state),
            message,
        });
    }

    /// The whole report: what was looked at, and `reason` for what was not.
    pub fn finish(mut self, reason: &str) -> Vec<GpuProbeCheck> {
        for step in CHECKS {
            self.skipped(step, reason);
        }
        self.recorded
            .sort_by_key(|check| CHECKS.iter().position(|step| *step == check.step()));
        self.recorded
    }
}

/// A renderer that answered, and what it turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Renderer {
    Hardware(String),
    Software(String),
}

impl Renderer {
    pub fn name(&self) -> &str {
        match self {
            Self::Hardware(name) | Self::Software(name) => name,
        }
    }
}

/// What a renderer's own name says it is, or nothing for no name at all.
pub fn classify(name: &str) -> Option<Renderer> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let lowercase = name.to_lowercase();
    if SOFTWARE.iter().any(|software| lowercase.contains(software)) {
        return Some(Renderer::Software(name.to_owned()));
    }
    Some(Renderer::Hardware(name.to_owned()))
}

/// Every renderer `eglinfo` named, in order and without repeats.
///
/// It walks several platforms and prints the same renderer for most of them,
/// and a check's message is read by a person.
///
/// The label is matched rather than one exact spelling of it: Mesa 26 writes
/// `OpenGL core profile renderer:` where older releases wrote `OpenGL renderer
/// string:`, and a guest whose renderer is only spelled the newer way reported
/// none at all. Both end in `renderer`, and what precedes it -- the profile --
/// is not something this has to know the list of.
pub fn eglinfo_renderers(output: &str) -> Vec<String> {
    let mut renderers: Vec<String> = Vec::new();
    for line in output.lines() {
        let Some((label, name)) = line.split_once(':') else {
            continue;
        };
        // `EGL vendor string:` also ends in `string`, which is why the word
        // before it is what decides rather than the word itself.
        let label = label.trim();
        if !label.ends_with("renderer") && !label.ends_with("renderer string") {
            continue;
        }
        let name = name.trim().to_owned();
        if !name.is_empty() && !renderers.contains(&name) {
            renderers.push(name);
        }
    }
    renderers
}

/// One physical device out of a `vulkaninfo --summary`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VulkanDevice {
    pub name: String,
    pub driver: String,
    /// Whether the driver says the device is a CPU, which is software however
    /// it is named.
    pub is_cpu: bool,
}

/// Every device `vulkaninfo --summary` listed.
///
/// Read as text and never trusted to have a fixed shape: output the parser
/// does not recognise is an empty list, which the caller reports as a check
/// that failed with the program's own output attached.
pub fn vulkaninfo_devices(output: &str) -> Vec<VulkanDevice> {
    let mut devices: Vec<VulkanDevice> = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.starts_with("GPU") && line.ends_with(':') {
            devices.push(VulkanDevice {
                name: String::new(),
                driver: String::new(),
                is_cpu: false,
            });
            continue;
        }
        let Some(device) = devices.last_mut() else {
            continue;
        };
        let Some((field, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match field.trim() {
            "deviceName" => device.name = value.to_owned(),
            "driverName" => device.driver = value.to_owned(),
            "deviceType" => device.is_cpu = value.ends_with("_CPU"),
            _ => {}
        }
    }
    devices
}

/// The first hardware renderer that answered, when one did.
pub fn hardware_renderer(found: &[Renderer]) -> Option<&str> {
    found.iter().find_map(|renderer| match renderer {
        Renderer::Hardware(name) => Some(name.as_str()),
        Renderer::Software(_) => None,
    })
}

/// What the guest makes of what it saw.
///
/// One hardware renderer is enough and has to be: Ubuntu does not build Mesa
/// with `microsoft-experimental`, so under the `distro` policy Vulkan is
/// lavapipe and GL is the only hardware path such a guest has.
pub fn verdict(device: bool, hardware: Option<&str>) -> GpuProbeVerdict {
    if !device {
        return GpuProbeVerdict::NoDevice;
    }
    if hardware.is_some() {
        return GpuProbeVerdict::Renders;
    }
    GpuProbeVerdict::DeviceOnly
}

/// The files a renderer has to be able to open, in the order they are named.
///
/// `mesa_prefix` is where a bundled Mesa was staged, and nothing under the
/// `distro` policy -- the two policies put the same driver in different
/// places, and a probe that looked in one of them would report a missing
/// library on a guest that renders.
pub fn required_libraries(triplet: &str, mesa_prefix: Option<&str>) -> Vec<String> {
    let mesa = match mesa_prefix {
        Some(prefix) => format!("{prefix}/lib/{triplet}"),
        None => format!("/usr/lib/{triplet}"),
    };

    vec![
        format!("{mesa}/dri/d3d12_dri.so"),
        format!("/usr/lib/{triplet}/libvulkan.so.1"),
        // `d3d12_dri.so` opens these itself, out of the host's mounted WSL
        // userspace: without them the GL path loads and then falls back.
        format!("{WSL_LIB}/libd3d12.so"),
        format!("{WSL_LIB}/libdxcore.so"),
    ]
}

/// The shell that runs one probe program under the recipe's environment.
///
/// Sourced rather than set here: the file is what a person gets over SSH, and
/// a second copy of those variables in the agent could disagree with it. The
/// guard is what keeps a guest whose recipe never wrote the file running the
/// program anyway -- sourcing a missing file must not be the thing that fails.
pub fn shell_command(program: &str) -> String {
    format!("[ -r {PROFILE} ] && . {PROFILE}; exec {program}")
}

#[cfg(test)]
mod tests {
    use vmlord_agent_protocol::v1::{GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict};

    use super::{
        CHECKS, Checks, Renderer, classify, eglinfo_renderers, hardware_renderer,
        required_libraries, shell_command, verdict, vulkaninfo_devices,
    };

    #[test]
    fn a_software_rasteriser_is_never_hardware_whatever_it_is_called() {
        for software in [
            "llvmpipe (LLVM 17.0.6, 256 bits)",
            "softpipe",
            "Mesa Intel(R) swrast",
            "lavapipe (LLVM 17.0.6, 256 bits)",
            "SwiftShader Device (LLVM 10.0.0)",
        ] {
            assert!(
                matches!(classify(software), Some(Renderer::Software(_))),
                "{software} is a CPU rasteriser"
            );
        }
    }

    #[test]
    fn a_renderer_this_build_has_never_heard_of_is_hardware() {
        // A deny list rather than an allow list: an allow list would report
        // "no hardware renderer" on the first stack this build was not
        // written against, and the milder failure is the right one here.
        for hardware in [
            "D3D12 (NVIDIA GeForce RTX 4070)",
            "D3D12 (AMD Radeon RX 7900 XT)",
            "Something Nobody Has Shipped Yet",
        ] {
            assert!(
                matches!(classify(hardware), Some(Renderer::Hardware(_))),
                "{hardware} has to count as hardware"
            );
        }
    }

    #[test]
    fn a_renderer_with_no_name_is_no_renderer() {
        assert!(classify("").is_none());
        assert!(classify("   ").is_none());
    }

    #[test]
    fn every_renderer_eglinfo_printed_is_read_once_and_in_order() {
        let output = "\
EGL API version: 1.5
Device platform:
OpenGL renderer string: D3D12 (NVIDIA GeForce RTX 4070)
Surfaceless platform:
OpenGL renderer string: D3D12 (NVIDIA GeForce RTX 4070)
OpenGL ES profile renderer string: llvmpipe (LLVM 17.0.6, 256 bits)
";

        assert_eq!(
            eglinfo_renderers(output),
            vec![
                "D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
                "llvmpipe (LLVM 17.0.6, 256 bits)".to_owned(),
            ]
        );
        assert!(eglinfo_renderers("eglinfo: command not found").is_empty());
    }

    #[test]
    fn the_renderer_is_read_when_eglinfo_omits_the_word_string() {
        // Mesa 26's eglinfo labels the line "OpenGL core profile renderer:",
        // where older ones wrote "OpenGL renderer string:". A guest whose
        // renderer is only spelled the newer way reported no renderer at all
        // and was held at DeviceOnly with an RTX 5070 Ti answering for it.
        let output = "\
EGL vendor string: Mesa Project
OpenGL core profile vendor: Microsoft Corporation
OpenGL core profile renderer: D3D12 (NVIDIA GeForce RTX 5070 Ti)
OpenGL core profile version: 4.6 (Core Profile) Mesa 26.0.3-1ubuntu1
OpenGL compatibility profile renderer: D3D12 (NVIDIA GeForce RTX 5070 Ti)
OpenGL ES profile renderer: D3D12 (NVIDIA GeForce RTX 5070 Ti)
";

        assert_eq!(
            eglinfo_renderers(output),
            vec!["D3D12 (NVIDIA GeForce RTX 5070 Ti)".to_owned()],
            "the vendor and version lines are not renderers, and one renderer \
             printed under three profiles is one renderer"
        );
    }

    #[test]
    fn vulkaninfo_names_every_device_it_summarised() {
        let output = "\
Devices:
========
GPU0:
\tapiVersion         = 1.3.255
\tdriverName         = llvmpipe
\tdeviceName         = llvmpipe (LLVM 17.0.6, 256 bits)
\tdeviceType         = PHYSICAL_DEVICE_TYPE_CPU
GPU1:
\tdriverName         = Microsoft Direct3D12
\tdeviceName         = Microsoft Direct3D12 (NVIDIA GeForce RTX 4070)
\tdeviceType         = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
";

        let devices = vulkaninfo_devices(output);

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].driver, "llvmpipe");
        assert!(devices[0].is_cpu);
        assert_eq!(
            devices[1].name,
            "Microsoft Direct3D12 (NVIDIA GeForce RTX 4070)"
        );
        assert!(!devices[1].is_cpu);
        assert!(vulkaninfo_devices("ERROR: no Vulkan loader").is_empty());
    }

    #[test]
    fn a_device_that_says_it_is_a_cpu_is_software_whatever_it_is_called() {
        // A driver nobody recognises that reports CPU is still not a GPU.
        let output = "\
GPU0:
\tdriverName         = something-new
\tdeviceName         = Something New
\tdeviceType         = PHYSICAL_DEVICE_TYPE_CPU
";

        let devices = vulkaninfo_devices(output);

        assert!(devices[0].is_cpu);
    }

    #[test]
    fn the_first_hardware_renderer_is_the_one_that_is_reported() {
        let found = vec![
            Renderer::Software("llvmpipe".to_owned()),
            Renderer::Hardware("D3D12 (NVIDIA GeForce RTX 4070)".to_owned()),
            Renderer::Hardware("Microsoft Direct3D12".to_owned()),
        ];

        assert_eq!(
            hardware_renderer(&found),
            Some("D3D12 (NVIDIA GeForce RTX 4070)")
        );
        assert_eq!(
            hardware_renderer(&[Renderer::Software("lavapipe".to_owned())]),
            None
        );
    }

    #[test]
    fn one_hardware_renderer_is_what_makes_a_guest_render() {
        // One is enough and has to be: under the distro Mesa policy Vulkan is
        // lavapipe, and GL is the only hardware path such a guest has.
        assert_eq!(verdict(true, Some("D3D12")), GpuProbeVerdict::Renders);
        assert_eq!(verdict(true, None), GpuProbeVerdict::DeviceOnly);
        assert_eq!(verdict(false, None), GpuProbeVerdict::NoDevice);
        assert_eq!(
            verdict(false, Some("D3D12")),
            GpuProbeVerdict::NoDevice,
            "a renderer without a device is a renderer this guest did not have"
        );
    }

    #[test]
    fn a_bundled_userspace_is_looked_for_where_it_was_staged() {
        let required = required_libraries("x86_64-linux-gnu", Some("/opt/vmlord/wsl-mesa"));

        assert!(
            required
                .contains(&"/opt/vmlord/wsl-mesa/lib/x86_64-linux-gnu/dri/d3d12_dri.so".to_owned()),
            "{required:?}"
        );
        // The WSL userspace d3d12_dri.so itself opens, whatever the policy.
        assert!(
            required.contains(&"/usr/lib/wsl/lib/libd3d12.so".to_owned()),
            "{required:?}"
        );
        assert!(
            required.contains(&"/usr/lib/wsl/lib/libdxcore.so".to_owned()),
            "{required:?}"
        );
    }

    #[test]
    fn a_distribution_userspace_is_looked_for_in_the_distributions_own_path() {
        let required = required_libraries("x86_64-linux-gnu", None);

        assert!(
            required.contains(&"/usr/lib/x86_64-linux-gnu/dri/d3d12_dri.so".to_owned()),
            "{required:?}"
        );
        assert!(
            required.contains(&"/usr/lib/x86_64-linux-gnu/libvulkan.so.1".to_owned()),
            "{required:?}"
        );
        assert!(
            !required.iter().any(|path| path.contains("wsl-mesa")),
            "nothing is staged under this policy: {required:?}"
        );
    }

    #[test]
    fn a_probe_program_runs_under_the_environment_the_recipe_wrote() {
        // Through the file rather than through variables set here: the file is
        // what a person gets over SSH, and a copy of that decision in the
        // agent could disagree with it.
        let command = shell_command("eglinfo -B");

        assert!(command.contains("/etc/profile.d/vmlord-gpu.sh"), "{command}");
        // A guest whose recipe never wrote the file still runs the program:
        // sourcing a missing file must not be what fails the check.
        assert!(command.contains("[ -r "), "{command}");
        assert!(command.contains("exec eglinfo -B"), "{command}");
    }

    #[test]
    fn a_finished_report_has_every_check_exactly_once_and_in_order() {
        let mut checks = Checks::new();
        checks.ok(GpuProbeStep::Device, "/dev/dxg is a usable device");

        let reported = checks.finish("the probe stopped before this check");

        assert_eq!(reported.len(), CHECKS.len());
        for (check, step) in reported.iter().zip(CHECKS) {
            assert_eq!(check.step(), step);
        }
        assert_eq!(reported[0].state(), GpuProbeCheckState::Ok);
        assert_eq!(reported[1].state(), GpuProbeCheckState::Skipped);
        assert_eq!(reported[1].message, "the probe stopped before this check");
    }

    #[test]
    fn the_checks_a_missing_device_never_reached_carry_its_reason() {
        let mut checks = Checks::new();
        checks.failed(GpuProbeStep::Device, "/dev/dxg is missing");

        let reported = checks.finish("/dev/dxg never opened");

        assert_eq!(reported[0].state(), GpuProbeCheckState::Failed);
        for check in &reported[1..] {
            assert_eq!(check.state(), GpuProbeCheckState::Skipped);
            assert_eq!(check.message, "/dev/dxg never opened");
        }
    }

    #[test]
    fn a_check_recorded_twice_keeps_the_first_answer() {
        let mut checks = Checks::new();
        checks.ok(GpuProbeStep::Opengl, "D3D12 (NVIDIA GeForce RTX 4070)");
        checks.failed(GpuProbeStep::Opengl, "gone");

        let reported = checks.finish("unreached");

        let opengl: Vec<_> = reported
            .iter()
            .filter(|check| check.step() == GpuProbeStep::Opengl)
            .collect();
        assert_eq!(opengl.len(), 1);
        assert_eq!(opengl[0].state(), GpuProbeCheckState::Ok);
    }
}
