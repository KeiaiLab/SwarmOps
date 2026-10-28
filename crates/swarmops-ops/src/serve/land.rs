//! A push to `refs/land/<target>` (M14 design §7): its tip imported, then landed onto `target`
//! with the push's land key, and a conflict reported on the side band.

use swarmops_git::serve::proc_receive::{Command, Report};

use super::hook::HookCtx;

/// The most conflicting paths a land's report lists, then `… and <k> more`.
pub const MAX_CONFLICT_LINES: usize = 50;

/// What the hook reports on `command`, a push to `refs/land/<target>`.
pub fn run(ctx: &HookCtx<'_>, command: &Command, target: &str) -> Report {
    let _ = (ctx, target);
    Report::ng(command.refname.clone(), "landing: not built")
}
