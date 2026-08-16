//! Mounting the GPU shares a host manifest names, and taking them away again.
//!
//! Every mount here is what a Hyper-V Plan9 share needs: a socket to the
//! host's Plan9 server handed to the kernel's 9p client as the transport. The
//! host end of that socket is not the agent's own session -- it is HCS's
//! server on a well-known vsock port, and it serves the shares the compute
//! system was configured with.
//!
//! The work is a reconcile rather than a mount. A host re-sends its manifest
//! on every session, so most attaches find the guest already in the state they
//! ask for, and the ones that do not are usually a VM that rebooted or a
//! manifest that changed. What decides is in [`reconcile`], which is a
//! function of the manifest and the mount table and is tested as one; the
//! syscalls around it cannot be tested without a Hyper-V host underneath.

use std::{
    ffi::CString,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

use vmlord_agent_protocol::v1::{GpuMount, GpuMountState, GpuShare};

use crate::{
    gpu_mountinfo::{self, MountedShare},
    gpu_targets::{self, Planned},
    vsock,
};

/// Where HCS's Plan9 server listens in the host partition.
///
/// Not the agent's own port: this socket carries 9p to the kernel's client,
/// and the compute system's `Devices/Plan9` section is what decides which
/// shares are on the other end of it.
const PLAN9_VSOCK_PORT: u32 = 50001;

/// The kernel's mount table.
const MOUNTINFO: &str = "/proc/self/mountinfo";

/// Where the dynamic linker is told about the mounted directories.
const LD_CONF: &str = "/etc/ld.so.conf.d/vmlord-gpu.conf";

/// How many times one target is mounted before it is reported as failed.
///
/// Two: the first attempt, and one more for a share whose transport died
/// between the mount and the read-back. A share the host has taken away must
/// not turn the agent into a loop of mount attempts, and no number of retries
/// makes a share that is gone come back.
const MOUNT_ATTEMPTS: usize = 2;

/// Mounts what a manifest names and unmounts what it no longer does.
///
/// Returns one report per share, in the order the shares arrived, and whether
/// the dynamic linker was told about the result. Nothing here fails as a
/// whole: a share that cannot be mounted is reported and the rest of the
/// manifest is still attached, because GPU is best effort in the guest for the
/// same reason it is on the host.
pub fn attach(shares: &[GpuShare]) -> (Vec<GpuMount>, bool) {
    let planned = gpu_targets::plan(shares);
    let mounted = read_mount_table();
    let plan = reconcile(&planned, &mounted);

    for path in &plan.stale {
        eprintln!(
            "vmlord-agent: unmounting {}, which the host no longer exports",
            path.display()
        );
        detach(path);
    }

    let mut report = Vec::with_capacity(plan.shares.len());
    let mut attached = Vec::new();
    for step in plan.shares {
        match step {
            Step::Refuse { share, message } => report.push(GpuMount {
                share,
                state: i32::from(GpuMountState::Refused),
                path: String::new(),
                message,
            }),
            Step::Attach {
                share,
                path,
                already,
            } => {
                let mount = attach_one(&share, &path, already);
                if mount.state == i32::from(GpuMountState::Mounted) {
                    attached.push(path);
                }
                report.push(mount);
            }
        }
    }

    (report, refresh_libraries(&attached))
}

/// Unmounts every GPU share this guest has, for a VM that is going down.
///
/// Best effort throughout, and driven by the mount table rather than by a list
/// this process kept: an agent that was upgraded and restarted still takes
/// away the mounts its predecessor made, and a guest that is shutting down is
/// not helped by an agent that refuses to exit because a mount was busy.
pub fn detach_all() {
    for mount in read_mount_table() {
        eprintln!("vmlord-agent: unmounting {}", mount.path.display());
        detach(&mount.path);
    }

    if let Err(error) = fs::remove_file(LD_CONF)
        && error.kind() != io::ErrorKind::NotFound
    {
        eprintln!("vmlord-agent: {LD_CONF} could not be removed: {error}");
    }
    run_ldconfig();
}

/// What has to happen to one share of a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// A share with no target in this guest.
    Refuse { share: String, message: String },
    /// A share to mount at `path`, over `already` when something is there.
    Attach {
        share: String,
        path: PathBuf,
        already: Option<String>,
    },
}

/// What an attach has to do, given a manifest and the mount table.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Plan {
    shares: Vec<Step>,
    /// Mounts of this agent's own that the manifest no longer names.
    stale: Vec<PathBuf>,
}

/// Turns a planned manifest and the current mounts into the work to do.
///
/// The stale list is what makes an attach subtractive as well as additive: a
/// VM that lost an adapter, or one whose payload generation was replaced,
/// would otherwise keep a directory backed by a share the host has stopped
/// serving, which reads to everything in the guest as a GPU userspace that is
/// there.
fn reconcile(planned: &[Planned], mounted: &[MountedShare]) -> Plan {
    let mut shares = Vec::with_capacity(planned.len());
    let mut wanted = Vec::new();

    for plan in planned {
        match plan {
            Planned::Refused { share, reason } => shares.push(Step::Refuse {
                share: share.clone(),
                message: reason.message().to_owned(),
            }),
            Planned::Mount { share, path } => {
                wanted.push(path.clone());
                shares.push(Step::Attach {
                    share: share.clone(),
                    path: path.clone(),
                    already: mounted
                        .iter()
                        .find(|mount| &mount.path == path)
                        .map(|mount| mount.share.clone()),
                });
            }
        }
    }

    let stale = mounted
        .iter()
        .filter(|mount| gpu_targets::is_managed(&mount.path) && !wanted.contains(&mount.path))
        .map(|mount| mount.path.clone())
        .collect();

    Plan { shares, stale }
}

/// Brings one target to the share the manifest asks for.
///
/// A target that already carries that share and reads back is left alone: an
/// agent reconnecting to a host that re-sent its manifest must not take a
/// working mount away and put it back, since anything in the guest holding a
/// library through it would lose the file underneath.
fn attach_one(share: &str, path: &Path, already: Option<String>) -> GpuMount {
    if already.as_deref() == Some(share) && reads_back(path) {
        return mounted(share, path, "already mounted");
    }

    if let Err(error) = fs::create_dir_all(path) {
        return failed(
            share,
            format!("{} could not be created: {error}", path.display()),
        );
    }
    if already.is_some() {
        detach(path);
    }

    let mut last = String::new();
    for _ in 0..MOUNT_ATTEMPTS {
        match mount(share, path) {
            Ok(()) => {
                if reads_back(path) {
                    return mounted(share, path, "mounted");
                }
                // Mounted and unreadable is a transport that died between the
                // two, which one more attempt fixes and a third would not.
                last = "the mount could not be read back".to_owned();
                detach(path);
            }
            Err(error) => last = error.to_string(),
        }
    }

    failed(share, last)
}

/// Mounts one share over a fresh connection to the host's Plan9 server.
///
/// The descriptor is handed to the kernel through `trans=fd` and closed here
/// afterwards: `mount(2)` takes its own reference, and the 9p client owns the
/// transport for the life of the mount.
///
/// `MS_RDONLY` states read-only a second time, independently of the flag the
/// host put on the share, and a host directory has no business handing a guest
/// device nodes or setuid binaries.
fn mount(share: &str, path: &Path) -> io::Result<()> {
    let transport = vsock::connect(vsock::VMADDR_CID_HOST, PLAN9_VSOCK_PORT)?;
    let descriptor = transport.as_raw_fd();

    let source = CString::new("none")?;
    let target = CString::new(path.as_os_str().as_encoded_bytes())?;
    let filesystem = CString::new("9p")?;
    let options = CString::new(format!(
        "trans=fd,rfdno={descriptor},wfdno={descriptor},version=9p2000.L,\
         aname={share},access=any,msize=65536,cache=loose"
    ))?;

    // SAFETY: every pointer is a `CString` that outlives this call, and the
    // flags and the option string describe the 9p mount above.
    let result = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            filesystem.as_ptr(),
            libc::MS_RDONLY | libc::MS_NODEV | libc::MS_NOSUID,
            options.as_ptr().cast(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// Takes a mount away without waiting for whatever is holding it.
///
/// `MNT_DETACH` because a process still holding a file on a share that is gone
/// must not stop the guest from getting a working one; the kernel drops its
/// references when the last one is closed.
fn detach(path: &Path) {
    let Ok(target) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return;
    };
    // SAFETY: `target` is a valid C string that outlives this call.
    let result = unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
    if result < 0 {
        let error = io::Error::last_os_error();
        eprintln!(
            "vmlord-agent: {} could not be unmounted: {error}",
            path.display()
        );
    }
}

/// Whether a mounted share can actually be read.
///
/// `read_dir` rather than a `stat` of the mount point: a 9p mount whose
/// transport died answers a `stat` from the dentry cache and fails a
/// directory read with `EIO` or `ENOTCONN`, and it is the second one that says
/// whether the guest can load anything from it.
fn reads_back(path: &Path) -> bool {
    match fs::read_dir(path) {
        // A share with nothing in it is served and empty, which is the host's
        // business rather than a broken mount.
        Ok(mut entries) => entries.all(|entry| entry.is_ok()),
        Err(_) => false,
    }
}

/// Tells the dynamic linker about the mounted directories that hold libraries.
///
/// The file is rewritten from the current set rather than appended to, which
/// is what makes an attach idempotent and what makes a share the manifest
/// dropped lose its line. A directory with no shared objects is left out: what
/// is in the payload is another task's to decide, and a cache entry for a
/// directory with nothing to load is noise in every `ldconfig` run afterwards.
fn refresh_libraries(paths: &[PathBuf]) -> bool {
    let with_libraries: Vec<&PathBuf> = paths
        .iter()
        .filter(|path| holds_shared_objects(path))
        .collect();

    let mut document = String::from("# Written by vmlord-agent. Do not edit.\n");
    for path in &with_libraries {
        document.push_str(&path.to_string_lossy());
        document.push('\n');
    }

    if let Err(error) = write_ld_conf(&document) {
        eprintln!("vmlord-agent: {LD_CONF} could not be written: {error}");
        return false;
    }

    run_ldconfig()
}

fn write_ld_conf(document: &str) -> io::Result<()> {
    if let Some(directory) = Path::new(LD_CONF).parent() {
        fs::create_dir_all(directory)?;
    }
    let mut file = fs::File::create(LD_CONF)?;
    file.write_all(document.as_bytes())
}

/// Whether a directory holds anything the dynamic linker would load.
///
/// A name of `lib.so` or `lib.so.1`: a versioned shared object is what a
/// DriverStore package and the WSL userspace are mostly made of, and a
/// directory of them with no bare `.so` still has to be on the search path.
fn holds_shared_objects(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.ends_with(".so") || name.contains(".so.")
    })
}

/// Rebuilds the linker cache.
///
/// The one external program this agent runs: there is no library form of it,
/// and writing `/etc/ld.so.cache` by hand would be a second implementation of
/// a format the distribution owns.
fn run_ldconfig() -> bool {
    match Command::new("ldconfig").status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("vmlord-agent: ldconfig exited with {status}");
            false
        }
        Err(error) => {
            eprintln!("vmlord-agent: ldconfig could not be run: {error}");
            false
        }
    }
}

/// The 9p mounts of this guest, or none when the table cannot be read.
fn read_mount_table() -> Vec<MountedShare> {
    match fs::read_to_string(MOUNTINFO) {
        Ok(table) => gpu_mountinfo::parse(&table),
        Err(error) => {
            eprintln!("vmlord-agent: {MOUNTINFO} could not be read: {error}");
            Vec::new()
        }
    }
}

fn mounted(share: &str, path: &Path, message: &str) -> GpuMount {
    GpuMount {
        share: share.to_owned(),
        state: i32::from(GpuMountState::Mounted),
        path: path.to_string_lossy().into_owned(),
        message: message.to_owned(),
    }
}

fn failed(share: &str, message: String) -> GpuMount {
    GpuMount {
        share: share.to_owned(),
        state: i32::from(GpuMountState::Failed),
        path: String::new(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Plan, Step, reconcile};
    use crate::{gpu_mountinfo::MountedShare, gpu_targets::Planned};

    fn wsl_lib() -> Planned {
        Planned::Mount {
            share: "vmlord.gpu.wsl-lib".to_owned(),
            path: PathBuf::from("/usr/lib/wsl/lib"),
        }
    }

    fn mounted(path: &str, share: &str) -> MountedShare {
        MountedShare {
            path: PathBuf::from(path),
            share: share.to_owned(),
        }
    }

    #[test]
    fn a_target_with_nothing_on_it_is_simply_mounted() {
        assert_eq!(
            reconcile(&[wsl_lib()], &[]),
            Plan {
                shares: vec![Step::Attach {
                    share: "vmlord.gpu.wsl-lib".to_owned(),
                    path: PathBuf::from("/usr/lib/wsl/lib"),
                    already: None,
                }],
                stale: Vec::new(),
            }
        );
    }

    #[test]
    fn a_target_that_already_carries_the_share_is_not_stale() {
        // A host re-sends its manifest on every session, so this is the
        // ordinary case: unmounting and mounting it again would take the
        // libraries out from under whatever is using them.
        assert_eq!(
            reconcile(
                &[wsl_lib()],
                &[mounted("/usr/lib/wsl/lib", "vmlord.gpu.wsl-lib")]
            ),
            Plan {
                shares: vec![Step::Attach {
                    share: "vmlord.gpu.wsl-lib".to_owned(),
                    path: PathBuf::from("/usr/lib/wsl/lib"),
                    already: Some("vmlord.gpu.wsl-lib".to_owned()),
                }],
                stale: Vec::new(),
            }
        );
    }

    #[test]
    fn a_target_carrying_another_share_is_replaced_rather_than_covered() {
        // A manifest that changed between boots: mounting over it would leave
        // the old share mounted underneath and unreachable.
        assert_eq!(
            reconcile(
                &[wsl_lib()],
                &[mounted("/usr/lib/wsl/lib", "vmlord.gpu.drv.old")]
            ),
            Plan {
                shares: vec![Step::Attach {
                    share: "vmlord.gpu.wsl-lib".to_owned(),
                    path: PathBuf::from("/usr/lib/wsl/lib"),
                    already: Some("vmlord.gpu.drv.old".to_owned()),
                }],
                stale: Vec::new(),
            }
        );
    }

    #[test]
    fn a_mount_the_manifest_no_longer_names_is_taken_away() {
        // A VM that lost an adapter would otherwise keep a directory backed
        // by a share the host has stopped serving.
        let plan = reconcile(
            &[wsl_lib()],
            &[
                mounted("/usr/lib/wsl/lib", "vmlord.gpu.wsl-lib"),
                mounted("/usr/lib/wsl/drivers/nv_dispi.inf", "vmlord.gpu.drv.gone"),
            ],
        );

        assert_eq!(
            plan.stale,
            vec![PathBuf::from("/usr/lib/wsl/drivers/nv_dispi.inf")]
        );
    }

    #[test]
    fn a_nine_p_mount_of_the_guests_own_is_left_alone() {
        // Only the three roots are this agent's to unmount; anything else the
        // guest mounted over 9p belongs to whoever mounted it.
        let plan = reconcile(&[wsl_lib()], &[mounted("/mnt/other", "something.else")]);

        assert_eq!(plan.stale, Vec::<PathBuf>::new());
    }

    #[test]
    fn a_refused_share_needs_no_mount_and_claims_no_target() {
        let plan = reconcile(
            &[Planned::Refused {
                share: "vmlord.gpu.drv.x".to_owned(),
                reason: crate::gpu_targets::Refusal::InvalidPackage,
            }],
            &[],
        );

        assert_eq!(
            plan,
            Plan {
                shares: vec![Step::Refuse {
                    share: "vmlord.gpu.drv.x".to_owned(),
                    message: "that driver package name cannot become a path".to_owned(),
                }],
                stale: Vec::new(),
            }
        );
    }
}
