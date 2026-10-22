//! Rebase (M5): a change merged three-way onto a ref's head and recorded as one changeset whose
//! parent is that head, moving no ref (operations spec §3). It shares a land's preparation, the
//! request checks, the merge and the resolutions, and ends with a commit where a land publishes,
//! so that there is one merge.
//!
//! Empty until the M5 plan's task W2, which writes `Repo::rebase` here.
