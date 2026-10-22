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

use crate::hostfs::{HostFs, HostKind};
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
/// the probe's files are removed afterwards, and ones a killed probe left first.
pub fn probe(host: &dyn HostFs, swarmws: &Utf8Path) -> io::Result<Volume> {
    // 1. Leftovers of a killed probe out of the way.
    let file = swarmws.join(PROBE);
    let link = swarmws.join(PROBE_LINK);
    remove_if_there(host, &link)?;
    remove_if_there(host, &file)?;

    // 2. Case: the file, looked up by another case.
    host.write_file(&file, b"probe")?;
    let case_insensitive = host.symlink_metadata(&swarmws.join(PROBE_UPPER))?.is_some();

    // 3. The executable bit, set and read back; Windows has none.
    let exec_bit = cfg!(unix) && {
        host.set_mode(&file, EXEC_MODE)?;
        let meta = host.symlink_metadata(&file)?;
        meta.is_some_and(|meta| meta.mode & EXEC_BITS == EXEC_BITS)
    };

    // 4. Symlinks: one made, and seen to be one; Windows without the privilege makes none.
    let symlinks = host.symlink(PROBE, &link).is_ok()
        && host
            .symlink_metadata(&link)?
            .is_some_and(|meta| meta.kind == HostKind::Symlink);

    // 5. The probe's files gone.
    remove_if_there(host, &link)?;
    remove_if_there(host, &file)?;
    Ok(Volume {
        case_insensitive,
        exec_bit,
        symlinks,
    })
}

/// Removes the file or symlink at `path` if one is there.
fn remove_if_there(host: &dyn HostFs, path: &Utf8Path) -> io::Result<()> {
    match host.remove_file(path) {
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
}
