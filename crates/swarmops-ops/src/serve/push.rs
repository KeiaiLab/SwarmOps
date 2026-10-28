//! One command of a push that moves a ref (M14 design §6): mapped, its access checked, its
//! objects imported, then the store ref moved by compare-and-swap.

use swarmops_git::serve::proc_receive::{Command, Report};

use super::hook::HookCtx;

/// What the hook reports on `command`.
pub fn decide(ctx: &HookCtx<'_>, command: &Command) -> Report {
    let _ = ctx;
    Report::ng(command.refname.clone(), "pushing a ref: not built")
}
