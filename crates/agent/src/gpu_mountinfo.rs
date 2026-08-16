//! What is mounted in this guest right now, as the kernel reports it.
//!
//! The attach is a reconcile rather than a mount, and the reconcile needs the
//! current state from somewhere. `/proc/self/mountinfo` is that somewhere: it
//! names the filesystem type and the super options of every mount, which is
//! what says whether the 9p share already at a target is the one the manifest
//! asks for. Reading the table rather than keeping a list is also what lets an
//! agent that was upgraded and restarted clean up its predecessor's mounts.

use std::path::PathBuf;

/// One 9p mount of a VMLord share, as it is mounted now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountedShare {
    /// Where it is mounted.
    pub path: PathBuf,
    /// The share it carries, from the `aname=` super option. Empty when the
    /// mount has none, which is a 9p mount that is not one of ours.
    pub share: String,
}

/// The 9p mounts of `mountinfo`, in the order the kernel lists them.
///
/// Only 9p: another filesystem at one of the agent's targets is the guest's
/// own and is left alone rather than unmounted. A line this parser cannot read
/// is skipped, because a table with one unexpected entry is not a reason to
/// treat the guest as having nothing mounted.
pub fn parse(mountinfo: &str) -> Vec<MountedShare> {
    mountinfo.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<MountedShare> {
    // `id parent dev root mountpoint options [optional...] - fstype source
    // super_options`. The optional fields are what makes the separator
    // necessary: the fields after it cannot be counted from the left.
    let (before, after) = line.split_once(" - ")?;
    let path = unescape(before.split(' ').nth(4)?);

    let mut after = after.split(' ');
    if after.next()? != "9p" {
        return None;
    }
    let super_options = after.nth(1)?;

    let share = super_options
        .split(',')
        .find_map(|option| option.strip_prefix("aname="))
        .unwrap_or_default()
        .to_owned();

    Some(MountedShare {
        path: PathBuf::from(path),
        share,
    })
}

/// Undoes the octal escaping the kernel applies to a path in this table.
///
/// Space, tab, newline and backslash travel as `\040`, `\011`, `\012` and
/// `\134`; leaving them escaped would make a mount point with a space in it
/// compare unequal to the path it actually is.
fn unescape(field: &str) -> String {
    let mut path = String::with_capacity(field.len());
    let mut rest = field;

    while let Some(index) = rest.find('\\') {
        path.push_str(&rest[..index]);
        let escape = rest.get(index + 1..index + 4);
        match escape.and_then(|digits| u8::from_str_radix(digits, 8).ok()) {
            Some(byte) => {
                path.push(char::from(byte));
                rest = &rest[index + 4..];
            }
            // Not an escape this table produces: the backslash is itself.
            None => {
                path.push('\\');
                rest = &rest[index + 1..];
            }
        }
    }
    path.push_str(rest);

    path
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{MountedShare, parse};

    const TABLE: &str = "\
23 28 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
36 25 0:32 / /usr/lib/wsl/lib ro,relatime - 9p none ro,dirsync,aname=vmlord.gpu.wsl-lib,trans=fd
37 25 0:33 / /usr/lib/wsl/drivers/nv_dispi.inf ro,relatime shared:3 - 9p none ro,aname=vmlord.gpu.drv.nv_dispi.inf
38 25 0:34 / /mnt/other rw,relatime - 9p none rw,trans=virtio
39 25 0:35 / /opt/vmlord/gpu-payload ro,relatime - ext4 /dev/sdb ro";

    #[test]
    fn a_nine_p_mount_is_reported_with_the_share_it_carries() {
        // The share is what says whether the mount already at a target is the
        // one the manifest asks for, and it is only in the super options.
        let mounts = parse(TABLE);

        assert_eq!(
            mounts,
            vec![
                MountedShare {
                    path: PathBuf::from("/usr/lib/wsl/lib"),
                    share: "vmlord.gpu.wsl-lib".to_owned(),
                },
                MountedShare {
                    path: PathBuf::from("/usr/lib/wsl/drivers/nv_dispi.inf"),
                    share: "vmlord.gpu.drv.nv_dispi.inf".to_owned(),
                },
                MountedShare {
                    path: PathBuf::from("/mnt/other"),
                    share: String::new(),
                },
            ]
        );
    }

    #[test]
    fn another_filesystem_at_a_target_is_not_one_of_ours() {
        // Unmounting it would take away something the guest mounted itself.
        assert!(
            !parse(TABLE)
                .iter()
                .any(|mount| mount.path == Path::new("/opt/vmlord/gpu-payload"))
        );
    }

    #[test]
    fn a_line_that_cannot_be_read_does_not_empty_the_table() {
        // A table this build misreads as empty would unmount nothing and
        // mount everything a second time.
        let table = format!("this is not a mountinfo line\n{TABLE}");

        assert_eq!(parse(&table).len(), 3);
    }

    #[test]
    fn an_escaped_mount_point_is_the_path_it_stands_for() {
        // The kernel escapes a space, and a path that stayed escaped would
        // never compare equal to the target it is.
        let table = "36 25 0:32 / /usr/lib/wsl\\040lib ro - 9p none ro,aname=vmlord.gpu.wsl-lib";

        assert_eq!(
            parse(table),
            vec![MountedShare {
                path: PathBuf::from("/usr/lib/wsl lib"),
                share: "vmlord.gpu.wsl-lib".to_owned(),
            }]
        );
    }
}
