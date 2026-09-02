//! What this guest is, asked of the guest itself.
//!
//! The host declares what it has to know before a guest exists -- an image
//! URL, a default user, the packages that install a desktop. Everything else
//! is asked here, at the moment a recipe acts, because a profile records what
//! VMLord *asked for* when the VM was created and a guest months later is
//! whatever it has *become*: a kernel upgraded, a desktop replaced, packages
//! added by hand.
//!
//! So nothing below branches on the name of a distribution. Which package
//! manager is installed is whether one answers; whether libraries sit under a
//! multiarch directory is whether that directory is there; what the desktop is
//! is what logind says is on the screen.
//!
//! Every decision is a function of text or of a directory that a test can
//! point somewhere else, in the shape `display-services`' `seat` module
//! already uses: the parsing takes a string, the walk takes a path, and only
//! the thin gathering at the bottom needs a real guest.

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{command, guest_files::read};

/// How long one `--version` may take. A package manager holding a lock still
/// prints its version, so this is generous only against a guest under load.
const PROBE_BUDGET: Duration = Duration::from_secs(10);

/// Where systemd links the display manager that owns the login screen.
const DISPLAY_MANAGER: &str = "/etc/systemd/system/display-manager.service";

/// Where logind keeps one file per session.
const SESSIONS: &str = "/run/systemd/sessions";

/// What the guest says it is, and what it turned out to be.
///
/// The first four fields are identity -- who this guest is -- and the rest are
/// platform: what a recipe would otherwise have to guess from that identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestFacts {
    /// `ID` from `/etc/os-release`, lowercase by that file's own convention.
    pub distribution: String,
    /// `VERSION_ID` from `/etc/os-release`, or `BUILD_ID` where the
    /// distribution publishes no version number.
    pub release: String,
    /// The Debian architecture name, not the machine name `uname` gives.
    pub architecture: String,
    /// `uname -r`: the kernel that is running now, which is the one DKMS
    /// builds against.
    pub kernel_release: String,
    /// The package manager that answered, or `None` on a guest carrying none
    /// this build knows how to drive.
    pub package_manager: Option<PackageManager>,
    /// Where this guest keeps its shared libraries.
    pub library_layout: LibraryLayout,
    /// The desktop found here, which is not the desktop the VM was created
    /// asking for.
    pub desktop: DesktopFacts,
}

impl GuestFacts {
    /// What was detected, in one line, for the stage the host logs.
    ///
    /// Identity is what the recipes already report; this is the rest of it,
    /// and it is worth a line because detection is invisible when it works and
    /// impossible to diagnose from a host when it does not.
    #[must_use]
    pub fn platform(&self) -> String {
        let manager = match self.package_manager {
            Some(manager) => manager.program().to_owned(),
            None => "no package manager this build knows".to_owned(),
        };
        format!(
            "{manager}, libraries in {}, {}",
            self.library_layout.directory(),
            self.desktop.describe()
        )
    }
}

/// A package manager the agent knows how to drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageManager {
    /// Debian and Ubuntu.
    Apt,
    /// Arch.
    Pacman,
    /// Fedora and RHEL.
    Dnf,
    /// SUSE.
    Zypper,
}

/// The managers that are looked for, in the order they are tried.
///
/// A guest normally carries one. Where a second is installed beside it -- a
/// `dnf` on a Debian host, say -- the first in this list wins, and the order
/// therefore has to be stable rather than merely convenient.
pub const MANAGERS: [PackageManager; 4] = [
    PackageManager::Apt,
    PackageManager::Pacman,
    PackageManager::Dnf,
    PackageManager::Zypper,
];

impl PackageManager {
    /// The program that is run, and whose presence is what detects it.
    ///
    /// `apt-get` rather than `apt`: `apt` prints a warning about not having a
    /// stable interface for scripts, and this is a script.
    #[must_use]
    pub const fn program(self) -> &'static str {
        match self {
            Self::Apt => "apt-get",
            Self::Pacman => "pacman",
            Self::Dnf => "dnf",
            Self::Zypper => "zypper",
        }
    }
}

impl std::fmt::Display for PackageManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.program())
    }
}

/// The first manager `present` finds.
///
/// Split from the probing so that the choice can be tested without installing
/// anything: the predicate is `--version` in a guest and a list in a test.
pub fn package_manager(present: impl Fn(&str) -> bool) -> Option<PackageManager> {
    MANAGERS
        .into_iter()
        .find(|manager| present(manager.program()))
}

/// Whether a program is installed and can be run.
///
/// Asked by running it, the way `dependencies_are_present` asks about DKMS and
/// the compiler: a file on `PATH` that will not execute is not a manager, and
/// every one of these prints its version and exits.
fn program_answers(program: &str) -> bool {
    command::run(program, &["--version"], &[], PROBE_BUDGET).succeeded()
}

/// Where a guest keeps its shared libraries.
///
/// Two shapes, and the difference is Debian's: multiarch puts every library in
/// a per-architecture subdirectory so that two architectures can be installed
/// side by side, and everyone else puts them in `lib` itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibraryLayout {
    /// A per-architecture subdirectory, named by the triplet it carries.
    Multiarch(String),
    /// Libraries directly in `lib`.
    Flat,
}

impl LibraryLayout {
    /// The library directory under a prefix: `/usr` for the guest's own,
    /// wherever a bundled tree was staged for a payload's.
    ///
    /// The prefix is a parameter rather than a second variant because the two
    /// callers ask the same question about different trees, and a payload
    /// staged under `/opt` lays its libraries out the way the guest it was
    /// built for does.
    #[must_use]
    pub fn directory_under(&self, prefix: &str) -> String {
        match self {
            Self::Multiarch(triplet) => format!("{prefix}/lib/{triplet}"),
            Self::Flat => format!("{prefix}/lib"),
        }
    }

    /// The guest's own library directory.
    #[must_use]
    pub fn directory(&self) -> String {
        self.directory_under("/usr")
    }
}

/// The multiarch directory a Debian architecture's libraries would live under.
///
/// Derived from the guest rather than written as a constant: an agent that
/// hard-codes one architecture's library path is one that silently installs
/// nothing on the other.
///
/// Not public: a triplet is a guess until a directory confirms it, so
/// `library_layout` is what the recipes ask, and this is the half of that
/// question the architecture answers.
#[must_use]
fn library_triplet(architecture: &str) -> Option<&'static str> {
    match architecture {
        "amd64" => Some("x86_64-linux-gnu"),
        "arm64" => Some("aarch64-linux-gnu"),
        _ => None,
    }
}

/// Which layout this guest uses, decided by looking.
///
/// The directory is what settles it, not the distribution: a guest whose
/// architecture has no triplet this build knows, and a guest that simply has
/// no such directory, both keep their libraries in `lib` as far as anything
/// here can tell -- and that is the answer that makes a path exist rather than
/// the answer that makes a recipe stop.
pub fn library_layout(architecture: &str, exists: impl Fn(&Path) -> bool) -> LibraryLayout {
    library_triplet(architecture)
        .filter(|triplet| exists(&PathBuf::from(format!("/usr/lib/{triplet}"))))
        .map_or(LibraryLayout::Flat, |triplet| {
            LibraryLayout::Multiarch(triplet.to_owned())
        })
}

/// What a desktop looks like on this guest.
///
/// All three empty is a guest with no desktop -- a headless VM, or one whose
/// desktop has not started yet. That is a fact, not a failure: the recipes
/// that read this skip a stage rather than end.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopFacts {
    /// What the session on the screen calls itself: logind's `DESKTOP`, which
    /// is the session's own `XDG_SESSION_DESKTOP` -- `gnome`, `Hyprland`.
    ///
    /// The name of the session rather than the name of a process, because a
    /// list of compositor process names is exactly the table of constants this
    /// work exists to remove, and `gnome-shell` truncated to fifteen
    /// characters in `/proc/<pid>/comm` is worse evidence than a name the
    /// session chose for itself.
    pub session: Option<String>,
    /// `wayland` or `x11`, from the same file.
    pub session_type: Option<String>,
    /// The unit `display-manager.service` is linked to -- `gdm.service`,
    /// `sddm.service` -- or `None` where nothing owns the login screen.
    pub display_manager: Option<String>,
}

impl DesktopFacts {
    /// Whether anything of a desktop was found at all.
    #[must_use]
    pub fn found(&self) -> bool {
        self.session.is_some() || self.session_type.is_some() || self.display_manager.is_some()
    }

    /// The desktop in one phrase, for the line a recipe reports.
    #[must_use]
    pub fn describe(&self) -> String {
        if !self.found() {
            return "no desktop".to_owned();
        }

        let session = match (&self.session, &self.session_type) {
            (Some(session), Some(kind)) => format!("{session} on {kind}"),
            (Some(session), None) => session.clone(),
            (None, Some(kind)) => format!("an unnamed {kind} session"),
            (None, None) => "no session on the screen".to_owned(),
        };
        match &self.display_manager {
            Some(unit) => format!("{session}, under {unit}"),
            None => session,
        }
    }
}

/// The graphical session that is on the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Session {
    desktop: Option<String>,
    kind: String,
}

/// One logind session file, if that session is the graphical one on screen.
///
/// The same three conditions the display broker authorises its socket with: a
/// console login has no desktop, an inactive session is not the one on screen,
/// and a session with no seat is not at the screen at all.
fn graphical_session(text: &str) -> Option<Session> {
    let mut desktop = None;
    let mut kind = None;
    let (mut seat, mut active) = (false, false);

    for line in text.lines() {
        match line.split_once('=') {
            Some(("SEAT", value)) => seat = value.trim() == "seat0",
            Some(("ACTIVE", value)) => active = value.trim() == "1",
            Some(("TYPE", value)) => {
                kind = matches!(value.trim(), "wayland" | "x11").then(|| value.trim().to_owned());
            }
            Some(("DESKTOP", value)) => {
                let value = value.trim();
                desktop = (!value.is_empty()).then(|| value.to_owned());
            }
            _ => {}
        }
    }

    (seat && active).then_some(Session {
        desktop,
        kind: kind?,
    })
}

/// The graphical session among the files in a sessions directory.
///
/// The directory half, so that the walk can be pointed at a fixture.
fn session_in(directory: &Path) -> Option<Session> {
    for entry in std::fs::read_dir(directory).ok()?.flatten() {
        // logind writes `<id>` for a session and `<id>.ref` for a reference
        // held on it; only the first is a session's state.
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(session) = graphical_session(&text) {
            return Some(session);
        }
    }

    None
}

/// The unit name a `display-manager.service` link points at.
///
/// The name alone: the link is into `/usr/lib/systemd/system` on one guest and
/// `/lib/systemd/system` on another, and the unit is what a caller can start.
fn display_manager_unit(target: &Path) -> Option<String> {
    target
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.ends_with(".service"))
        .map(str::to_owned)
}

/// What desktop this guest has, read out of a sessions directory and a link.
fn desktop_facts(sessions: &Path, display_manager: &Path) -> DesktopFacts {
    let session = session_in(sessions);
    DesktopFacts {
        session: session.as_ref().and_then(|found| found.desktop.clone()),
        session_type: session.map(|found| found.kind),
        display_manager: std::fs::read_link(display_manager)
            .ok()
            .as_deref()
            .and_then(display_manager_unit),
    }
}

/// Reads `ID` and the release out of an `/etc/os-release`.
///
/// The release is `VERSION_ID` where the distribution numbers its releases and
/// `BUILD_ID` where it does not: Arch's file carries `ID=arch` and
/// `BUILD_ID=rolling` and no `VERSION_ID` at all, and without the fallback no
/// guest facts assemble and every recipe stops before its first stage.
/// `VERSION_ID` wins when both are present, because that is the number the
/// payload catalogs are keyed by; a `BUILD_ID` beside it stamps one image
/// rather than naming the release.
pub fn parse_os_release(text: &str) -> Option<(String, String)> {
    let mut id = None;
    let mut version = None;
    let mut build = None;
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        match name.trim() {
            "ID" => id = Some(value),
            "VERSION_ID" => version = Some(value),
            "BUILD_ID" => build = Some(value),
            _ => {}
        }
    }

    Some((id?, version.or(build)?))
}

/// What this guest is, from its own files, programs and sessions.
///
/// Identity fails the call, platform does not: a guest that will not say what
/// it is has no recipe at all, while a guest with no package manager and no
/// desktop is one whose stages skip themselves with a reason.
pub fn guest_facts() -> Result<GuestFacts, String> {
    let (distribution, release) = parse_os_release(&read(Path::new("/etc/os-release")))
        .ok_or_else(|| "/etc/os-release names no distribution".to_owned())?;
    let (kernel_release, machine) = uname()?;
    // Debian's name for the machine, because that is what a payload target
    // and an apt package name are written in.
    let architecture = match machine.as_str() {
        "x86_64" => "amd64".to_owned(),
        "aarch64" => "arm64".to_owned(),
        other => other.to_owned(),
    };

    Ok(GuestFacts {
        package_manager: package_manager(program_answers),
        library_layout: library_layout(&architecture, |path| path.is_dir()),
        desktop: desktop_facts(Path::new(SESSIONS), Path::new(DISPLAY_MANAGER)),
        distribution,
        release,
        architecture,
        kernel_release,
    })
}

/// The running kernel's release and machine.
fn uname() -> Result<(String, String), String> {
    let mut information = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: `uname` fills the `utsname` it is given and touches nothing
    // else; the pointer is to a live, correctly sized allocation.
    let result = unsafe { libc::uname(information.as_mut_ptr()) };
    if result != 0 {
        return Err(format!("uname failed: {}", io::Error::last_os_error()));
    }
    // SAFETY: `uname` returned success, so the structure is initialized.
    let information = unsafe { information.assume_init() };

    Ok((field(&information.release), field(&information.machine)))
}

/// One NUL-terminated C string out of a `utsname`.
fn field(bytes: &[libc::c_char]) -> String {
    bytes
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8 as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        DesktopFacts, GuestFacts, LibraryLayout, PackageManager, desktop_facts,
        display_manager_unit, graphical_session, library_layout, library_triplet, package_manager,
        parse_os_release, session_in,
    };

    const GNOME_WAYLAND: &str =
        "UID=1000\nSEAT=seat0\nTYPE=wayland\nACTIVE=1\nDESKTOP=gnome\nSTATE=active\n";
    const TTY: &str = "UID=1000\nSEAT=seat0\nTYPE=tty\nACTIVE=1\nSTATE=active\n";

    fn temporary(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "vmlord-guest-platform-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn the_first_manager_that_answers_is_the_one() {
        let installed = |program: &str| program == "pacman";
        assert_eq!(package_manager(installed), Some(PackageManager::Pacman));

        let debian = |program: &str| matches!(program, "apt-get" | "dnf");
        assert_eq!(
            package_manager(debian),
            Some(PackageManager::Apt),
            "a guest carrying two managers gets the first of the fixed order, \
             not whichever the filesystem answered about first"
        );
    }

    #[test]
    fn a_guest_with_no_manager_this_build_knows_has_none() {
        assert_eq!(package_manager(|_| false), None);
        assert_eq!(package_manager(|program| program == "apk"), None);
    }

    #[test]
    fn every_manager_is_detected_by_the_program_that_installs_with_it() {
        for manager in super::MANAGERS {
            assert_eq!(
                package_manager(|program| program == manager.program()),
                Some(manager)
            );
        }
        assert_eq!(PackageManager::Apt.program(), "apt-get");
        assert_eq!(PackageManager::Apt.to_string(), "apt-get");
    }

    #[test]
    fn a_multiarch_directory_that_is_there_is_the_layout() {
        let layout = library_layout("amd64", |path| {
            path == Path::new("/usr/lib/x86_64-linux-gnu")
        });

        assert_eq!(
            layout,
            LibraryLayout::Multiarch("x86_64-linux-gnu".to_owned())
        );
        assert_eq!(layout.directory(), "/usr/lib/x86_64-linux-gnu");
        assert_eq!(
            layout.directory_under("/opt/vmlord/mesa"),
            "/opt/vmlord/mesa/lib/x86_64-linux-gnu"
        );
    }

    #[test]
    fn a_guest_without_one_keeps_its_libraries_in_lib() {
        let arch = library_layout("amd64", |_| false);
        assert_eq!(arch, LibraryLayout::Flat);
        assert_eq!(arch.directory(), "/usr/lib");
        assert_eq!(
            arch.directory_under("/opt/vmlord/mesa"),
            "/opt/vmlord/mesa/lib"
        );
    }

    #[test]
    fn an_architecture_with_no_triplet_is_flat_rather_than_an_error() {
        assert_eq!(library_triplet("amd64"), Some("x86_64-linux-gnu"));
        assert_eq!(library_triplet("arm64"), Some("aarch64-linux-gnu"));
        assert_eq!(library_triplet("riscv64"), None);
        assert_eq!(library_layout("riscv64", |_| true), LibraryLayout::Flat);
    }

    #[test]
    fn the_session_on_the_screen_names_the_desktop() {
        let session = graphical_session(GNOME_WAYLAND).unwrap();
        assert_eq!(session.desktop.as_deref(), Some("gnome"));
        assert_eq!(session.kind, "wayland");

        let hyprland =
            graphical_session("UID=1000\nSEAT=seat0\nTYPE=wayland\nACTIVE=1\nDESKTOP=Hyprland\n")
                .unwrap();
        assert_eq!(
            hyprland.desktop.as_deref(),
            Some("Hyprland"),
            "the session's own spelling, because nothing here has a list of \
             desktop names to normalise it against"
        );
    }

    #[test]
    fn a_session_that_is_not_on_the_screen_names_nothing() {
        assert_eq!(
            graphical_session(TTY),
            None,
            "a console login has no desktop"
        );
        assert_eq!(
            graphical_session("SEAT=seat0\nTYPE=wayland\nACTIVE=0\nDESKTOP=gnome\n"),
            None,
            "an inactive session is not the one on screen"
        );
        assert_eq!(
            graphical_session("UID=1001\nTYPE=wayland\nACTIVE=1\nDESKTOP=gnome\n"),
            None,
            "a session with no seat is not at the screen at all"
        );
        assert_eq!(graphical_session(""), None);
    }

    #[test]
    fn a_graphical_session_that_names_no_desktop_is_still_a_desktop() {
        let session = graphical_session("SEAT=seat0\nTYPE=x11\nACTIVE=1\n").unwrap();
        assert_eq!(session.desktop, None);
        assert_eq!(session.kind, "x11");
    }

    #[test]
    fn the_walk_finds_the_graphical_session_among_the_others() {
        let directory = temporary("sessions");
        std::fs::write(directory.join("c1"), TTY).unwrap();
        std::fs::write(directory.join("c2"), GNOME_WAYLAND).unwrap();

        let session = session_in(&directory);

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(session.unwrap().desktop.as_deref(), Some("gnome"));
    }

    #[test]
    fn a_guest_without_logind_has_no_session() {
        assert_eq!(
            session_in(Path::new("/nonexistent/run/systemd/sessions")),
            None
        );
    }

    #[test]
    fn the_display_manager_is_the_unit_the_link_points_at() {
        assert_eq!(
            display_manager_unit(Path::new("/usr/lib/systemd/system/gdm.service")).as_deref(),
            Some("gdm.service")
        );
        assert_eq!(
            display_manager_unit(Path::new("/lib/systemd/system/sddm.service")).as_deref(),
            Some("sddm.service")
        );
        assert_eq!(
            display_manager_unit(Path::new("/usr/lib/systemd/system")),
            None,
            "a link that does not point at a unit names no display manager"
        );
    }

    #[test]
    fn the_desktop_of_a_guest_with_a_login_screen_and_a_session() {
        let directory = temporary("desktop");
        let sessions = directory.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("c1"), GNOME_WAYLAND).unwrap();
        let unit = directory.join("gdm.service");
        std::fs::write(&unit, "").unwrap();
        let link = directory.join("display-manager.service");
        std::os::unix::fs::symlink(&unit, &link).unwrap();

        let facts = desktop_facts(&sessions, &link);

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(
            facts,
            DesktopFacts {
                session: Some("gnome".to_owned()),
                session_type: Some("wayland".to_owned()),
                display_manager: Some("gdm.service".to_owned()),
            }
        );
        assert!(facts.found());
    }

    #[test]
    fn a_headless_guest_has_a_desktop_of_nothing_rather_than_a_failure() {
        let directory = temporary("headless");

        let facts = desktop_facts(&directory, &directory.join("display-manager.service"));

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(facts, DesktopFacts::default());
        assert!(!facts.found());
    }

    #[test]
    fn a_greeter_alone_is_a_desktop_before_anyone_has_logged_in() {
        let directory = temporary("greeter");
        let sessions = directory.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let unit = directory.join("gdm.service");
        std::fs::write(&unit, "").unwrap();
        let link = directory.join("display-manager.service");
        std::os::unix::fs::symlink(&unit, &link).unwrap();

        let facts = desktop_facts(&sessions, &link);

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(facts.session, None);
        assert_eq!(facts.display_manager.as_deref(), Some("gdm.service"));
        assert!(facts.found());
    }

    #[test]
    fn the_reported_platform_says_what_was_detected() {
        let mut facts = GuestFacts {
            distribution: "ubuntu".to_owned(),
            release: "26.04".to_owned(),
            architecture: "amd64".to_owned(),
            kernel_release: "7.0.0-14-generic".to_owned(),
            package_manager: Some(PackageManager::Apt),
            library_layout: LibraryLayout::Multiarch("x86_64-linux-gnu".to_owned()),
            desktop: DesktopFacts {
                session: Some("gnome".to_owned()),
                session_type: Some("wayland".to_owned()),
                display_manager: Some("gdm.service".to_owned()),
            },
        };
        assert_eq!(
            facts.platform(),
            "apt-get, libraries in /usr/lib/x86_64-linux-gnu, gnome on wayland, under gdm.service"
        );

        facts.package_manager = None;
        facts.library_layout = LibraryLayout::Flat;
        facts.desktop = DesktopFacts::default();
        assert_eq!(
            facts.platform(),
            "no package manager this build knows, libraries in /usr/lib, no desktop",
            "a guest the recipe can do nothing with still says why, because \
             this line is all a host has to diagnose a detection from"
        );
    }

    #[test]
    fn os_release_values_are_read_with_or_without_quotes() {
        let text = "PRETTY_NAME=\"Ubuntu 26.04 LTS\"\nID=ubuntu\nVERSION_ID=\"26.04\"\n";

        assert_eq!(
            parse_os_release(text),
            Some(("ubuntu".to_owned(), "26.04".to_owned()))
        );
    }

    #[test]
    fn an_os_release_without_an_id_names_nothing() {
        assert_eq!(parse_os_release("VERSION_ID=\"26.04\"\n"), None);
        assert_eq!(parse_os_release(""), None);
    }

    #[test]
    fn a_distribution_without_a_version_names_its_release_with_its_build_id() {
        let text = "PRETTY_NAME=\"Arch Linux\"\nID=arch\nBUILD_ID=rolling\n";

        assert_eq!(
            parse_os_release(text),
            Some(("arch".to_owned(), "rolling".to_owned()))
        );
    }

    #[test]
    fn a_build_id_beside_a_version_id_does_not_displace_the_release() {
        let text = "ID=ubuntu\nVERSION_ID=\"26.04\"\nBUILD_ID=20260901\n";

        assert_eq!(
            parse_os_release(text),
            Some(("ubuntu".to_owned(), "26.04".to_owned()))
        );
    }

    #[test]
    fn an_os_release_with_neither_a_version_nor_a_build_names_nothing() {
        assert_eq!(parse_os_release("ID=arch\n"), None);
    }
}
