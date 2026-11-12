//! The volume probe (§3.2 of the M5 design): whether the workspace's directory folds case,
//! holds the executable bit and makes symlinks, probed once at creation in its `.swarmws/` and
//! kept in `workspace.json`.
//!
//! | Probe (in `.swarmws/`)                 | Kept as            |
//! |----------------------------------------|--------------------|
//! | write `probe-a`, stat `PROBE-A`        | `case_insensitive` |
//! | set mode 0o755 on it, read it back     | `exec_bit`         |
//! | make a symlink to it                   | `symlinks`         |
//!
//! Per-directory case sensitivity (NTFS) follows the directory the workspace is made in, which
//! the probe sees.

use std::io;

use camino::Utf8Path;

use crate::hostfs::{self, HostFs, HostKind};
use crate::state::Volume;

/// The probe's file.
const PROBE: &str = "probe-a";

/// The probe's file, its case changed.
const PROBE_UPPER: &str = "PROBE-A";

/// The probe's symlink.
const PROBE_LINK: &str = "probe-l";

/// The mode the probe sets: an executable's.
pub(crate) const EXEC_MODE: u32 = 0o755;

/// The bits that make a file executable.
pub(crate) const EXEC_BITS: u32 = 0o111;

/// What the volume of `swarmws` (a workspace's `.swarmws/`, made) holds, probed through `host`;
/// the probe's files are removed afterwards, and ones a killed probe left first. Every write
/// names the workspace's directory as its root and `.swarmws/<file>` below it, so a `.swarmws`
/// that is a link is refused as swapped and nothing is written or removed through it.
pub fn probe(host: &dyn HostFs, swarmws: &Utf8Path) -> io::Result<Volume> {
    // 1. The workspace's directory is the root: `.swarmws` itself is walked without following.
    let (root, name) = hostfs::split_root(swarmws)?;
    let file_rel = format!("{name}/{PROBE}");
    let link_rel = format!("{name}/{PROBE_LINK}");

    // 2. Leftovers of a killed probe out of the way.
    let file = swarmws.join(PROBE);
    let link = swarmws.join(PROBE_LINK);
    remove_if_there(host, root, &link_rel)?;
    remove_if_there(host, root, &file_rel)?;

    // 3. Case: the file, looked up by another case.
    host.write_file(root, &file_rel, b"probe")?;
    let case_insensitive = host.symlink_metadata(&swarmws.join(PROBE_UPPER))?.is_some();

    // 4. The executable bit, set and read back; Windows has none.
    let exec_bit = cfg!(unix) && {
        host.set_mode(root, &file_rel, EXEC_MODE)?;
        let meta = host.symlink_metadata(&file)?;
        meta.is_some_and(|meta| meta.mode & EXEC_BITS == EXEC_BITS)
    };

    // 5. Symlinks: one made, and seen to be one; Windows without the privilege makes none.
    let symlinks = host.symlink(root, &link_rel, PROBE).is_ok()
        && host
            .symlink_metadata(&link)?
            .is_some_and(|meta| meta.kind == HostKind::Symlink);

    // 6. The probe's files gone.
    remove_if_there(host, root, &link_rel)?;
    remove_if_there(host, root, &file_rel)?;
    Ok(Volume {
        case_insensitive,
        exec_bit,
        symlinks,
    })
}

/// Removes the file or symlink `rel` below `root` if one is there.
fn remove_if_there(host: &dyn HostFs, root: &Utf8Path, rel: &str) -> io::Result<()> {
    match host.remove_file(root, rel) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hostfs::RealHost;

    /// This host's volume (Linux, ext4 or tmpfs) is case-sensitive and holds the bit and
    /// symlinks; the probe leaves nothing behind.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_linux_volume_holds_case_the_bit_and_links() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let volume = probe(&RealHost, dir).unwrap();
        assert_eq!(
            volume,
            Volume {
                case_insensitive: false,
                exec_bit: true,
                symlinks: true,
            }
        );
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
    }

    /// A `.swarmws` that is a link out of the workspace is refused as swapped before the probe
    /// writes or removes anything through it: a probe file a killed probe seems to have left
    /// at the link's target stays.
    #[test]
    #[cfg(unix)]
    fn probe_never_follows_a_swarmws_link() {
        // 1. A workspace directory whose `.swarmws` links to a directory outside it.
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let outside = root.join("outside");
        let swarmws = root.join("ws").join(".swarmws");
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir(root.join("ws")).unwrap();
        std::os::unix::fs::symlink(&outside, &swarmws).unwrap();

        // 2. Outside, a file by the probe's name that is not the probe's.
        let kept = outside.join(PROBE);
        std::fs::write(&kept, b"keep").unwrap();

        // 3. The probe refuses the link by its name below the workspace.
        let refused = probe(&RealHost, &swarmws).unwrap_err();
        assert_eq!(hostfs::swapped_path(&refused), Some(".swarmws"));

        // 4. Nothing outside was removed, written or added.
        assert_eq!(std::fs::read(&kept).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    }
}
