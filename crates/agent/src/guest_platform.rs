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
//! is what logind says is on the screen, and how its compositor is started is
//! which cgroup the process holding the seat's card turned out to be in.
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

/// Where the kernel keeps one directory per process.
const PROC: &str = "/proc";

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
    /// How the compositor that is on the screen was started, which is what
    /// decides how anything can be delivered to it.
    ///
    /// `None` where no compositor is running: a headless guest, and every
    /// guest in the moment between boot and its greeter.
    pub compositor: Option<CompositorLaunch>,
}

/// The names a GNOME session goes under, matched as substrings of a
/// lowercased session name.
///
/// Two rather than a catalogue: GNOME calls its sessions `gnome`,
/// `gnome-classic` and `gnome-xorg`, and Ubuntu renames its own to `ubuntu`,
/// `ubuntu-wayland` and `ubuntu-xorg` while shipping the same shell. Anything
/// else is a desktop this build has no special knowledge of, which is the
/// answer that leaves it alone.
const GNOME_SESSIONS: [&str; 2] = ["gnome", "ubuntu"];

/// The units GNOME's login screen is linked as, without the `.service` suffix.
///
/// Debian and Ubuntu package it as `gdm3`; everyone else, GNOME included, as
/// `gdm`.
const GNOME_DISPLAY_MANAGERS: [&str; 2] = ["gdm", "gdm3"];

impl DesktopFacts {
    /// Whether the desktop found here is GNOME.
    ///
    /// The one desktop VMLord knows to need helping: GNOME hides
    /// StatusNotifierItems until an AppIndicator extension is installed and
    /// enabled, and its compositor is a templated user unit a drop-in can
    /// reach. Both of those are things to do *to* a desktop rather than
    /// properties to read off one, so they are asked of what was found and
    /// never of what the VM was created asking for.
    ///
    /// The session on the screen settles it where there is one. Where there is
    /// none -- which is every guest at the moment its display recipe runs,
    /// root and before anybody has logged in -- the unit owning the login
    /// screen does, because that is what the next session will be started by.
    /// Nothing found at all is not GNOME: a headless guest gets nothing done
    /// to it.
    #[must_use]
    pub fn is_gnome(&self) -> bool {
        if let Some(session) = &self.session {
            let session = session.to_lowercase();

            return GNOME_SESSIONS.iter().any(|name| session.contains(name));
        }
        self.display_manager.as_deref().is_some_and(|unit| {
            let unit = unit.trim_end_matches(".service").to_lowercase();

            GNOME_DISPLAY_MANAGERS.contains(&unit.as_str())
        })
    }

    /// How the guest's synthetic Hyper-V display is kept off this desktop.
    ///
    /// The desktop that was *found*, because the mechanism is a thing done to
    /// a running compositor rather than a property of the image the VM was
    /// created from.
    ///
    /// Where mutter is what will be on the screen, the tag it already reads is
    /// the gentlest answer there is: the card keeps its driver, the Hyper-V
    /// console keeps working, and one compositor is asked to leave one device
    /// alone. Where it is not, the tag is a word nothing reads, and the only
    /// thing that hides a card from a compositor that has no such word is not
    /// having a driver bound to it.
    ///
    /// Nothing found at all takes the tag as well, and that is a choice rather
    /// than a default: the recipe runs on a guest whose desktop packages may
    /// still be installing, and of the two mechanisms the tag is the one that
    /// cannot take a display away from anybody. The unbinding is reserved for
    /// a desktop somebody actually found.
    #[must_use]
    pub fn output_selection(&self) -> OutputSelection {
        if self.is_gnome() || !self.found() {
            return OutputSelection::Ignored;
        }

        OutputSelection::Unbound
    }

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
        let session = match &self.display_manager {
            Some(unit) => format!("{session}, under {unit}"),
            None => session,
        };
        match &self.compositor {
            Some(launch) => format!("{session}, {}", launch.describe()),
            None => session,
        }
    }
}

/// How the compositor that is on the screen was started.
///
/// Not what it is called: what started it. A drop-in reaches a compositor that
/// systemd started and reaches nothing at all in a session that a login shell
/// opened, and that difference -- not the name `gnome` or `Hyprland` -- is
/// what an isolation has to be chosen by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompositorLaunch {
    /// A systemd user unit, named as it is running:
    /// `org.gnome.Shell@wayland.service`, `wayland-wm@hyprland.service`.
    Unit(String),
    /// The session's own scope, which is what a compositor started from a
    /// login shell runs in. A scope is made by whoever asked for it and
    /// carries no configuration a drop-in could add to.
    Scope(String),
}

impl CompositorLaunch {
    /// Where a drop-in for this compositor goes, relative to the user unit
    /// directory, or `None` where there is no unit to attach one to.
    ///
    /// The template rather than the instance: `org.gnome.Shell@wayland.service`
    /// is one instance among several -- the greeter runs its own, and so does
    /// every user who logs in -- and systemd reads `foo@.service.d` for every
    /// instance of `foo@`. A drop-in on the instance that happened to be
    /// running when the recipe looked would miss the next one.
    #[must_use]
    pub fn drop_in_directory(&self) -> Option<String> {
        let Self::Unit(unit) = self else {
            return None;
        };
        let name = match unit.split_once('@') {
            Some((template, _)) => format!("{template}@.service"),
            None => unit.clone(),
        };
        Some(format!("{name}.d"))
    }

    /// The launch in one phrase, for the line a recipe reports.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Unit(unit) => format!("started by {unit}"),
            Self::Scope(scope) => format!("started outside a unit, in {scope}"),
        }
    }
}

/// How the guest's own synthetic display is kept from taking half the desktop.
///
/// A Hyper-V guest has a display of its own -- `simpledrm` at first,
/// `hyperv_drm` once that is unbound -- and a compositor that finds two cards
/// lights both. The second monitor is drawn on the Hyper-V console, where the
/// viewer cannot see it, and an absolute pointer is mapped across the pair, so
/// clicks land about a third of a screen from where they were aimed. Task #121
/// measured exactly that.
///
/// Two ways of preventing it, and which one suits a guest is decided by the
/// desktop found on it rather than written down beside a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSelection {
    /// The card stays bound and is tagged for a compositor that reads the tag.
    ///
    /// `mutter-device-ignore` is mutter's own -- `61-mutter.rules` uses it for
    /// vkms -- and a udev rule sorting after that file adds to it. It means
    /// nothing to any other compositor.
    Ignored,
    /// The card is taken away from every compositor by unbinding its driver.
    ///
    /// What is left for a compositor with no ignore tag of its own. It costs
    /// the Hyper-V console, which is the screen nobody was looking at, and it
    /// is the one answer that needs no compositor to agree to anything.
    Unbound,
}

impl OutputSelection {
    /// The choice in one phrase, for the line a recipe reports.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Ignored => {
                "the Hyper-V display is tagged for a compositor that reads the tag".to_owned()
            }
            Self::Unbound => "the Hyper-V display is unbound from its driver".to_owned(),
        }
    }
}

/// What the leaf of a process's cgroup path says started it.
///
/// systemd puts every process it starts in a cgroup named after its unit, and
/// everything else a login opens in the session's scope, so the last segment
/// of the path is the answer. A leaf that is neither is a cgroup nothing here
/// can act on, and says so by being `None` rather than by being guessed at.
fn launch_of(cgroup_path: &str) -> Option<CompositorLaunch> {
    let leaf = cgroup_path.rsplit('/').find(|part| !part.is_empty())?;
    if leaf.ends_with(".service") {
        return Some(CompositorLaunch::Unit(leaf.to_owned()));
    }
    if leaf.ends_with(".scope") {
        return Some(CompositorLaunch::Scope(leaf.to_owned()));
    }

    None
}

/// The cgroup path a `/proc/<pid>/cgroup` file names.
///
/// Two formats, and both are read: the unified hierarchy writes one `0::` line
/// and a guest still on cgroup v1 writes one line per controller, of which the
/// `name=systemd` one is where the units are.
fn cgroup_path(text: &str) -> Option<&str> {
    let mut legacy = None;
    for line in text.lines() {
        let mut fields = line.splitn(3, ':');
        let Some((controllers, path)) = fields.nth(1).zip(fields.next()) else {
            continue;
        };
        if controllers.is_empty() {
            return Some(path.trim());
        }
        if controllers.split(',').any(|name| name == "name=systemd") {
            legacy = Some(path.trim());
        }
    }

    legacy
}

/// Whether this process belongs to a user's own slice.
///
/// The compositor is the session user's process; the display broker holds a
/// card too and is a system service, so the slice is what tells them apart
/// without either being named.
fn belongs_to(cgroup_path: &str, uid: u32) -> bool {
    cgroup_path.contains(&format!("/user-{uid}.slice/"))
}

/// The graphical session that is on the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Session {
    desktop: Option<String>,
    kind: String,
    /// Who the session belongs to, which is whose processes the compositor is
    /// among. The greeter's own session has one of these as much as a
    /// logged-in user's does.
    uid: Option<u32>,
}

/// One logind session file, if that session is the graphical one on screen.
///
/// The same three conditions the display broker authorises its socket with: a
/// console login has no desktop, an inactive session is not the one on screen,
/// and a session with no seat is not at the screen at all.
fn graphical_session(text: &str) -> Option<Session> {
    let mut desktop = None;
    let mut kind = None;
    let mut uid = None;
    let (mut seat, mut active) = (false, false);

    for line in text.lines() {
        match line.split_once('=') {
            Some(("SEAT", value)) => seat = value.trim() == "seat0",
            Some(("ACTIVE", value)) => active = value.trim() == "1",
            Some(("UID", value)) => uid = value.trim().parse().ok(),
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
        uid,
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

/// The processes of `uid` that have a card open, and how each was started.
///
/// A compositor is found by what it does rather than by what it is called: it
/// holds the seat's card open, and a list of the programs that might be one is
/// exactly the table of constants this module exists without. The slice keeps
/// the guest's own display broker -- a system service that holds a card too --
/// out of the answer.
fn compositor_launch(proc: &Path, uid: u32) -> Option<CompositorLaunch> {
    let Ok(entries) = std::fs::read_dir(proc) else {
        return None;
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path.join("cgroup")) else {
            continue;
        };
        let Some(cgroup) = cgroup_path(&text) else {
            continue;
        };
        if !belongs_to(cgroup, uid) || !holds_a_card(&path.join("fd")) {
            continue;
        }
        if let Some(launch) = launch_of(cgroup) {
            found.push(launch);
        }
    }

    // A unit ahead of a scope, and then by name: a session may have a second
    // process holding a card -- an Xwayland, a probe -- and the answer must
    // not depend on the order the kernel listed `/proc` in. A unit is the
    // stronger answer of the two, because it is the one something can be
    // delivered to.
    found.sort_by(|left, right| key(left).cmp(&key(right)));
    found.into_iter().next()
}

/// What `compositor_launch` orders candidates by.
fn key(launch: &CompositorLaunch) -> (u8, &str) {
    match launch {
        CompositorLaunch::Unit(name) => (0, name),
        CompositorLaunch::Scope(name) => (1, name),
    }
}

/// Whether a process has one of the seat's cards open.
fn holds_a_card(descriptors: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(descriptors) else {
        return false;
    };
    entries.flatten().any(|entry| {
        std::fs::read_link(entry.path())
            .ok()
            .and_then(|target| target.to_str().map(is_drm_card))
            .unwrap_or(false)
    })
}

/// Whether an open file is a DRM card.
///
/// The card and not a render node: everything that draws opens a render node,
/// and what a compositor alone opens is the device that owns the outputs.
fn is_drm_card(target: &str) -> bool {
    target.strip_prefix("/dev/dri/card").is_some_and(|number| {
        !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// What desktop this guest has, read out of a sessions directory, a link and
/// the processes of whoever is at the screen.
fn desktop_facts(sessions: &Path, display_manager: &Path, proc: &Path) -> DesktopFacts {
    let session = session_in(sessions);
    DesktopFacts {
        session: session.as_ref().and_then(|found| found.desktop.clone()),
        session_type: session.as_ref().map(|found| found.kind.clone()),
        display_manager: std::fs::read_link(display_manager)
            .ok()
            .as_deref()
            .and_then(display_manager_unit),
        compositor: session
            .and_then(|found| found.uid)
            .and_then(|uid| compositor_launch(proc, uid)),
    }
}

/// What desktop this guest has right now.
///
/// Read again rather than carried: a recipe that ran before the greeter did
/// asks this a second time, and the answer it wants is the one that is true
/// when it asks.
#[must_use]
pub fn desktop_now() -> DesktopFacts {
    desktop_facts(
        Path::new(SESSIONS),
        Path::new(DISPLAY_MANAGER),
        Path::new(PROC),
    )
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
        desktop: desktop_now(),
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
        CompositorLaunch, DesktopFacts, GuestFacts, LibraryLayout, OutputSelection, PackageManager,
        cgroup_path, compositor_launch, desktop_facts, display_manager_unit, graphical_session,
        is_drm_card, launch_of, library_layout, library_triplet, package_manager, parse_os_release,
        session_in,
    };

    const GNOME_WAYLAND: &str =
        "UID=1000\nSEAT=seat0\nTYPE=wayland\nACTIVE=1\nDESKTOP=gnome\nSTATE=active\n";
    const TTY: &str = "UID=1000\nSEAT=seat0\nTYPE=tty\nACTIVE=1\nSTATE=active\n";

    /// One process in a fixture `/proc`: where its cgroup says it was started
    /// and what it has open.
    fn process(proc: &Path, pid: u32, cgroup: &str, open: &[&str]) {
        let directory = proc.join(pid.to_string());
        std::fs::create_dir_all(directory.join("fd")).unwrap();
        std::fs::write(directory.join("cgroup"), format!("0::{cgroup}\n")).unwrap();
        for (index, target) in open.iter().enumerate() {
            // Dangling on purpose: what is read is where the link points, and
            // a fixture has no card to point it at.
            std::os::unix::fs::symlink(target, directory.join("fd").join((index + 3).to_string()))
                .unwrap();
        }
    }

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

        let proc = directory.join("proc");
        process(
            &proc,
            410,
            "/user.slice/user-1000.slice/user@1000.service/session.slice/\
             org.gnome.Shell@wayland.service",
            &["/dev/dri/card0"],
        );

        let facts = desktop_facts(&sessions, &link, &proc);

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(
            facts,
            DesktopFacts {
                session: Some("gnome".to_owned()),
                session_type: Some("wayland".to_owned()),
                display_manager: Some("gdm.service".to_owned()),
                compositor: Some(CompositorLaunch::Unit(
                    "org.gnome.Shell@wayland.service".to_owned()
                )),
            }
        );
        assert!(facts.found());
    }

    #[test]
    fn a_headless_guest_has_a_desktop_of_nothing_rather_than_a_failure() {
        let directory = temporary("headless");

        let facts = desktop_facts(
            &directory,
            &directory.join("display-manager.service"),
            &directory.join("proc"),
        );

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

        let facts = desktop_facts(&sessions, &link, &directory.join("proc"));

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(facts.session, None);
        assert_eq!(
            facts.compositor, None,
            "a display manager that has started nothing yet has no compositor to be asked \
             how it starts"
        );
        assert_eq!(facts.display_manager.as_deref(), Some("gdm.service"));
        assert!(facts.found());
    }

    #[test]
    fn the_session_on_the_screen_says_whether_this_is_gnome() {
        // Ubuntu renames GNOME's sessions after itself and ships the same
        // shell, so both names answer yes; a compositor that hosts its own
        // tray items answers no and is left alone.
        for name in ["gnome", "GNOME-Classic", "ubuntu-wayland"] {
            let found = DesktopFacts {
                session: Some(name.to_owned()),
                session_type: Some("wayland".to_owned()),
                display_manager: Some("gdm.service".to_owned()),
                ..DesktopFacts::default()
            };
            assert!(found.is_gnome(), "{name} is a GNOME session");
        }

        let hyprland = DesktopFacts {
            session: Some("Hyprland".to_owned()),
            session_type: Some("wayland".to_owned()),
            display_manager: Some("gdm.service".to_owned()),
            ..DesktopFacts::default()
        };
        assert!(
            !hyprland.is_gnome(),
            "the session on the screen outranks the greeter that started it: \
             a guest whose login screen is GDM is not running GNOME because \
             of it"
        );
    }

    #[test]
    fn the_unit_a_compositor_runs_in_is_what_starts_it() {
        assert_eq!(
            launch_of(
                "/user.slice/user-1000.slice/user@1000.service/session.slice/org.gnome.Shell@wayland.service"
            ),
            Some(CompositorLaunch::Unit(
                "org.gnome.Shell@wayland.service".to_owned()
            ))
        );
        assert_eq!(
            launch_of("/user.slice/user-1000.slice/session-3.scope"),
            Some(CompositorLaunch::Scope("session-3.scope".to_owned())),
            "a compositor a login shell started is in the session's own scope, \
             which is not a unit anything can be added to"
        );
        assert_eq!(
            launch_of("/user.slice/user-1000.slice"),
            None,
            "a cgroup that is neither says so rather than being read as one"
        );
    }

    #[test]
    fn a_guest_with_no_session_yet_is_read_from_its_login_screen() {
        // The shape at recipe time, when nobody has logged in: the greeter is
        // the only evidence there is, and it is what starts the session that
        // the tray extension will have to show through.
        for unit in ["gdm.service", "gdm3.service"] {
            let greeter = DesktopFacts {
                display_manager: Some(unit.to_owned()),
                ..DesktopFacts::default()
            };
            assert!(greeter.is_gnome(), "{unit} is GNOME's login screen");
        }

        let sddm = DesktopFacts {
            display_manager: Some("sddm.service".to_owned()),
            ..DesktopFacts::default()
        };
        assert!(!sddm.is_gnome());
    }

    #[test]
    fn a_guest_with_nothing_on_screen_is_not_gnome() {
        // A headless VM, and the answer that gets nothing installed into it.
        assert!(!DesktopFacts::default().is_gnome());
    }

    #[test]
    fn a_drop_in_goes_on_the_template_and_not_on_the_instance() {
        // The greeter runs one instance and every user who logs in runs
        // another; systemd reads the template's directory for all of them.
        let gnome = CompositorLaunch::Unit("org.gnome.Shell@wayland.service".to_owned());
        assert_eq!(
            gnome.drop_in_directory().as_deref(),
            Some("org.gnome.Shell@.service.d")
        );

        let uwsm = CompositorLaunch::Unit("wayland-wm@hyprland.service".to_owned());
        assert_eq!(
            uwsm.drop_in_directory().as_deref(),
            Some("wayland-wm@.service.d")
        );

        let plain = CompositorLaunch::Unit("cage.service".to_owned());
        assert_eq!(plain.drop_in_directory().as_deref(), Some("cage.service.d"));

        assert_eq!(
            CompositorLaunch::Scope("session-3.scope".to_owned()).drop_in_directory(),
            None,
            "a session with no unit gets no path, because a file written at one \
             would be read by nothing"
        );
    }

    #[test]
    fn a_desktop_that_reads_the_tag_gets_the_tag_and_no_other_one_does() {
        // The one mechanism is a word mutter reads and every other compositor
        // ignores, so a guest that is not running mutter would light the
        // synthetic card as a second monitor nobody can see -- task #121
        // measured the cost -- and the card has to lose its driver instead.
        let gnome = DesktopFacts {
            session: Some("ubuntu-wayland".to_owned()),
            session_type: Some("wayland".to_owned()),
            display_manager: Some("gdm3.service".to_owned()),
            ..DesktopFacts::default()
        };
        assert_eq!(gnome.output_selection(), OutputSelection::Ignored);

        let hyprland = DesktopFacts {
            session: Some("Hyprland".to_owned()),
            session_type: Some("wayland".to_owned()),
            display_manager: Some("greetd.service".to_owned()),
            ..DesktopFacts::default()
        };
        assert_eq!(hyprland.output_selection(), OutputSelection::Unbound);
    }

    #[test]
    fn a_guest_with_nothing_on_screen_keeps_the_mechanism_that_removes_nothing() {
        // Not the same question as "which desktop is this": with no desktop
        // found there is no compositor to hide anything from, and the recipe
        // runs on guests whose desktop packages are still installing. The tag
        // is inert wherever it is not read; unbinding a card is not.
        assert_eq!(
            DesktopFacts::default().output_selection(),
            OutputSelection::Ignored
        );
    }

    #[test]
    fn both_cgroup_formats_name_the_same_thing() {
        assert_eq!(
            cgroup_path("0::/user.slice/user-1000.slice/session-3.scope\n"),
            Some("/user.slice/user-1000.slice/session-3.scope")
        );
        assert_eq!(
            cgroup_path(
                "12:pids:/user.slice/user-1000.slice\n\
                 1:name=systemd:/user.slice/user-1000.slice/session-3.scope\n"
            ),
            Some("/user.slice/user-1000.slice/session-3.scope"),
            "a guest still on cgroup v1 keeps the units in the systemd hierarchy"
        );
        assert_eq!(cgroup_path(""), None);
        assert_eq!(
            cgroup_path("nonsense\n0::/user.slice/user-1000.slice/session-3.scope\n"),
            Some("/user.slice/user-1000.slice/session-3.scope"),
            "a line that is not a cgroup line is skipped, not read as the end of the file"
        );
    }

    #[test]
    fn the_card_is_what_a_compositor_holds_and_a_render_node_is_not() {
        assert!(is_drm_card("/dev/dri/card0"));
        assert!(is_drm_card("/dev/dri/card12"));
        assert!(
            !is_drm_card("/dev/dri/renderD128"),
            "everything that draws opens a render node; the card is the device \
             that owns the outputs"
        );
        assert!(!is_drm_card("/dev/dri/card"));
        assert!(!is_drm_card("/dev/null"));
    }

    #[test]
    fn the_compositor_is_the_sessions_own_process_that_holds_a_card() {
        let directory = temporary("compositor");
        let proc = directory.join("proc");
        // The guest's display broker holds a card too, and is a system
        // service: without the slice it would be found first and answer for
        // the compositor.
        process(
            &proc,
            200,
            "/system.slice/vmlord-display-broker.service",
            &["/dev/dri/card1"],
        );
        process(
            &proc,
            410,
            "/user.slice/user-1000.slice/user@1000.service/session.slice/\
             org.gnome.Shell@wayland.service",
            &["/dev/dri/card1"],
        );
        // An application of the same session, drawing through a render node.
        process(
            &proc,
            520,
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox.scope",
            &["/dev/dri/renderD128"],
        );

        let found = compositor_launch(&proc, 1000);

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(
            found,
            Some(CompositorLaunch::Unit(
                "org.gnome.Shell@wayland.service".to_owned()
            ))
        );
    }

    #[test]
    fn a_compositor_started_from_a_login_shell_is_found_as_one() {
        let directory = temporary("login-shell");
        let proc = directory.join("proc");
        process(
            &proc,
            300,
            "/user.slice/user-1000.slice/session-2.scope",
            &["/dev/dri/card0"],
        );

        let found = compositor_launch(&proc, 1000);

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(
            found,
            Some(CompositorLaunch::Scope("session-2.scope".to_owned())),
            "Hyprland from a login shell is in the session's scope, and the \
             recipe has to hear that rather than a unit that is not there"
        );
    }

    #[test]
    fn a_session_of_another_user_is_not_this_sessions_compositor() {
        let directory = temporary("other-user");
        let proc = directory.join("proc");
        process(
            &proc,
            410,
            "/user.slice/user-121.slice/user@121.service/session.slice/\
             org.gnome.Shell@wayland.service",
            &["/dev/dri/card0"],
        );

        let found = compositor_launch(&proc, 1000);

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(found, None);
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
                compositor: Some(CompositorLaunch::Unit(
                    "org.gnome.Shell@wayland.service".to_owned(),
                )),
            },
        };
        assert_eq!(
            facts.platform(),
            "apt-get, libraries in /usr/lib/x86_64-linux-gnu, gnome on wayland, under \
             gdm.service, started by org.gnome.Shell@wayland.service"
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
