//! `check list` (M30): the checks a tree describes. The store form reads the tree of a changeset
//! through `dyn StoreOps`, as `swarmops mcp`'s `check_list` does; the directory form reads the
//! files below a directory of the caller's host and opens no store, so `--repo`,
//! `SWARMOPS_REPO` and `--as` are read and not used, and its envelope has no `meta.repo`.

use std::collections::BTreeMap;

use swarmops_api::{CheckListReq, Rev};

use super::Ctx;
use crate::cli::CheckCommand;
use crate::output::Printed;

/// Runs `check`'s `command`, which names operation `op`.
pub(super) fn run(ctx: &Ctx<'_>, op: &'static str, command: CheckCommand) -> Printed {
    match command {
        CheckCommand::List { dir: Some(dir), .. } => {
            let no_store = (None, BTreeMap::new(), None);
            ctx.finish(op, no_store, swarmops_ops::check::list_dir(&dir))
        }
        CheckCommand::List { rev, dir: None } => ctx.on_store(
            op,
            || {
                Ok(rev.map_or_else(CheckListReq::default, |rev| {
                    CheckListReq::new(Rev::from(rev))
                }))
            },
            |ops, req| swarmops_ops::check::list(ops, req),
        ),
    }
}
