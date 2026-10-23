//! The workspace tools of `swarmops mcp` (M5): eleven tools, one per workspace operation but
//! `ws_delete`, which removes a directory tree (an agent ends its own workspace by landing it,
//! and the hook adapter removes it). Each names its workspace by `workspace`, a registry name,
//! and runs the `swarmops_ws` function of its name on the store the server opens for the call.
//!
//! A child of `mcp`, since the tool table's machinery (`Operate`, `Refuse`, `Named`) is private
//! there. The tools have a table of their own, [`WS_TOOLS`], with their own annotations, so that
//! the store's `Tool` and its hints keep the store's operations only.
//!
//! ```text
//! tools/call ─▶ WS_TOOLS ─▶ operate_ws ─▶ Operate::workspace ─▶ (store, root, who?)
//!                                                  │
//!                                                  ▼
//!                                  swarmops_ws::<op>(&WsCtx, req)
//! ```
//!
//! The M5 plan's task W1 froze the table and the arms; task W8 writes the descriptions the
//! tests ask for. Until the `swarmops_ws` bodies land, each tool answers `E_INVALID_ARG` naming
//! the task that builds it.

use camino::Utf8Path;
use serde_json::{Value, json};
use swarmops_api::{
    Identity, StoreOps, WsCommitReq, WsCreateReq, WsDiffReq, WsDiscardReq, WsInfoReq, WsLandReq,
    WsListReq, WsMaterializeReq, WsRebaseReq, WsResolveReq, WsStatusReq,
};
use swarmops_ws::Ctx as WsCtx;

use super::{Named, Operate, Refuse};
use crate::commands::ws;

/// The workspace tools, in the order `tools/list` gives them, after the store's.
pub(super) static WS_TOOLS: [WsSpec; 11] = [
    WsSpec {
        tool: WsTool::Create,
        name: "ws_create",
        request: "WsCreateReq",
        title: "Create a workspace",
        description: "Makes a workspace: a directory holding a sparse copy of a changeset's tree \
                      (base, main by default) under the prefixes given, for building and testing \
                      a change on disk. The same call again is the same workspace (created \
                      false). A new workspace gets the files .worktreeinclude lists in `from` \
                      and runs the repository's bootstrap command (.swarmops/workspaces.json).",
    },
    WsSpec {
        tool: WsTool::Status,
        name: "ws_status",
        request: "WsStatusReq",
        title: "Show a workspace's status",
        description: "What changed in a workspace against its head, a page at a time, and how \
                      far its target has moved since its base; pass a limit of 100 or less.",
    },
    WsSpec {
        tool: WsTool::Diff,
        name: "ws_diff",
        request: "WsDiffReq",
        title: "Diff a workspace",
        description: "A workspace's edits against its head, as the diff tool gives a diff; pass \
                      max_bytes of 32768 or less and a limit of 100 or less.",
    },
    WsSpec {
        tool: WsTool::Commit,
        name: "ws_commit",
        request: "WsCommitReq",
        title: "Commit a workspace's edits",
        description: "Records a workspace's edits as one changeset on its head, uploading only \
                      the blobs the store lacks; a crashed commit replays by its key.",
    },
    WsSpec {
        tool: WsTool::Rebase,
        name: "ws_rebase",
        request: "WsRebaseReq",
        title: "Rebase a workspace",
        description: "Moves a workspace's chain onto its target's head, as the rebase tool \
                      records it, and its files with it; on a conflict the workspace holds \
                      markers: edit them, ws_resolve, then ws_rebase with action continue.",
    },
    WsSpec {
        tool: WsTool::Resolve,
        name: "ws_resolve",
        request: "WsResolveReq",
        title: "Resolve a workspace's conflicts",
        description: "Marks conflicting paths of a workspace's rebase resolved; a file that \
                      still holds a marker line is refused unless force.",
    },
    WsSpec {
        tool: WsTool::Land,
        name: "ws_land",
        request: "WsLandReq",
        title: "Land a workspace",
        description: "Lands a workspace's chain on its target. With your own shell, ws_rebase, \
                      build and test in the workspace, then ws_land with mode exact, which \
                      publishes only if the target has not moved since the rebase, so that the \
                      landed tree is the one you tested; check runs a command here instead, and \
                      blocks this server while it runs.",
    },
    WsSpec {
        tool: WsTool::Materialize,
        name: "ws_materialize",
        request: "WsMaterializeReq",
        title: "Materialise more of a workspace",
        description: "Writes out more of a workspace's tree: the paths given join its prefixes.",
    },
    WsSpec {
        tool: WsTool::Discard,
        name: "ws_discard",
        request: "WsDiscardReq",
        title: "Discard a workspace's edits",
        description: "Restores paths of a workspace from its head, every changed one when none \
                      is given; the edits are lost.",
    },
    WsSpec {
        tool: WsTool::List,
        name: "ws_list",
        request: "WsListReq",
        title: "List workspaces",
        description: "The store's workspaces, from its registry; with check, a workspace whose \
                      directory is gone is reported as an orphan.",
    },
    WsSpec {
        tool: WsTool::Info,
        name: "ws_info",
        request: "WsInfoReq",
        title: "Describe a workspace",
        description: "One workspace: its directory, base, head and prefixes.",
    },
];

/// One workspace tool: which operation it runs, and what `tools/list` says of it.
pub(super) struct WsSpec {
    pub(super) tool: WsTool,
    /// The tool's name: the operation's, as the envelope's `op` gives it.
    pub(super) name: &'static str,
    /// The operation's request type, as `schema_for_all` names it.
    pub(super) request: &'static str,
    pub(super) title: &'static str,
    /// One line.
    pub(super) description: &'static str,
}

impl Named for WsSpec {
    fn name(&self) -> &'static str {
        self.name
    }

    fn request(&self) -> &'static str {
        self.request
    }
}

/// The workspace operations, one tool each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WsTool {
    Create,
    Status,
    Diff,
    Commit,
    Rebase,
    Resolve,
    Land,
    Materialize,
    Discard,
    List,
    Info,
}

impl WsTool {
    /// The tool's annotations. None reaches beyond the store and the workspace's directory;
    /// `ws_discard` alone overwrites what is there (the edits it restores from the head).
    pub(super) fn annotations(self) -> Value {
        let (read_only, destructive, idempotent) = match self {
            WsTool::Status | WsTool::Diff | WsTool::List | WsTool::Info => (true, false, true),
            // A commit run again records another changeset, or finds nothing to commit.
            WsTool::Commit => (false, false, false),
            // Run again with the same arguments, each finds its work done: the same workspace,
            // a rebase up to date, paths resolved or materialised, the change landed.
            WsTool::Create
            | WsTool::Rebase
            | WsTool::Resolve
            | WsTool::Land
            | WsTool::Materialize => (false, false, true),
            WsTool::Discard => (false, true, true),
        };
        json!({
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": false,
        })
    }
}

/// Runs the workspace `tool`'s operation as `how` runs operations: the one place where a
/// workspace tool meets the `swarmops_ws` function it stands for. Each arm builds the
/// workspace's context from the store the server opened for this call, so no arm keeps a handle
/// and none reads the environment.
pub(super) fn operate_ws<O: Operate>(tool: WsTool, how: O) -> O::Done {
    match tool {
        WsTool::Create => how.workspace(|store, root, who, bootstrap, req| {
            ws::create_bootstrapped(&ws_ctx(store, root, who), req, bootstrap)
        }),
        WsTool::Status => how.workspace(|store, root, who, _, req| {
            swarmops_ws::status(&ws_ctx(store, root, who), req)
        }),
        WsTool::Diff => how.workspace(|store, root, who, _, req| {
            swarmops_ws::diff(&ws_ctx(store, root, who), req)
        }),
        WsTool::Commit => how.workspace(|store, root, who, _, req| {
            swarmops_ws::commit(&ws_ctx(store, root, who), req)
        }),
        WsTool::Rebase => how.workspace(|store, root, who, _, req| {
            swarmops_ws::rebase(&ws_ctx(store, root, who), req)
        }),
        WsTool::Resolve => how.workspace(|store, root, who, _, req| {
            swarmops_ws::resolve(&ws_ctx(store, root, who), req)
        }),
        WsTool::Land => how.workspace(|store, root, who, _, req| {
            swarmops_ws::land(&ws_ctx(store, root, who), req)
        }),
        WsTool::Materialize => how.workspace(|store, root, who, _, req| {
            swarmops_ws::materialize(&ws_ctx(store, root, who), req)
        }),
        WsTool::Discard => how.workspace(|store, root, who, _, req| {
            swarmops_ws::discard(&ws_ctx(store, root, who), req)
        }),
        WsTool::List => how.workspace(|store, root, who, _, req| {
            swarmops_ws::list(&ws_ctx(store, root, who), req)
        }),
        WsTool::Info => how.workspace(|store, root, who, _, req| {
            swarmops_ws::info(&ws_ctx(store, root, who), req)
        }),
    }
}

/// The workspace context of one call: the store opened for it, the root it was opened at, the
/// server's identity if it has one, and the host's two file layers.
fn ws_ctx<'a>(store: &'a dyn StoreOps, root: &Utf8Path, who: Option<&Identity>) -> WsCtx<'a> {
    ws::context(store, root, who)
}

// The workspace requests have no rules of this server's beyond their types: the workspace
// operations check their own arguments, as the CLI's do.
impl Refuse for WsCreateReq {}
impl Refuse for WsStatusReq {}
impl Refuse for WsDiffReq {}
impl Refuse for WsCommitReq {}
impl Refuse for WsRebaseReq {}
impl Refuse for WsResolveReq {}
impl Refuse for WsLandReq {}
impl Refuse for WsMaterializeReq {}
impl Refuse for WsDiscardReq {}
impl Refuse for WsListReq {}
impl Refuse for WsInfoReq {}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    use super::super::probe::Probe;
    use super::*;

    /// One tool per workspace operation but `ws_delete`, each running the function of its name
    /// on the store the server opens, with the root it opened it at.
    #[test]
    fn the_ws_tools_are_the_eleven_workspace_operations() {
        // 1. One tool per Ws*Req, but WsDeleteReq.
        let all = swarmops_api::schema_for_all();
        let requests: BTreeSet<&str> = all
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .filter(|name| name.starts_with("Ws") && name.ends_with("Req"))
            .filter(|name| *name != "WsDeleteReq")
            .collect();
        let advertised: BTreeSet<&str> = WS_TOOLS.iter().map(|spec| spec.request).collect();
        assert_eq!(advertised, requests, "one tool per workspace request type");
        assert_eq!(WS_TOOLS.len(), 11);

        // 2. Each arm runs the function of its name, which answers as it does on a store
        //    without workspaces.
        for spec in &WS_TOOLS {
            let called = RefCell::new(Vec::new());
            let outcome = RefCell::new(Vec::new());
            operate_ws(spec.tool, Probe::new(&called, &outcome));
            let outcome = outcome.into_inner();
            assert_eq!(outcome.len(), 1, "the {} tool", spec.name);
            let expected = built(spec.tool);
            assert!(
                outcome[0].contains(expected),
                "the {} tool ran {:?}",
                spec.name,
                outcome[0]
            );
        }
    }

    /// What a workspace operation answers to the probe's sample on a store without
    /// workspaces.
    fn built(tool: WsTool) -> &'static str {
        match tool {
            WsTool::List => "ok",
            WsTool::Info
            | WsTool::Status
            | WsTool::Diff
            | WsTool::Commit
            | WsTool::Discard
            | WsTool::Rebase
            | WsTool::Resolve
            | WsTool::Land => r#"the store has no workspace named "probe""#,
            // `from` is checked before anything is made: the probe's sample is not a path.
            WsTool::Create => "is relative: give an absolute one",
            WsTool::Materialize => "ws materialize needs at least one path",
        }
    }
}
