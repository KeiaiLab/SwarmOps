//! The git side of `swarmops serve` (the M14 serve design's decision summary): the serving
//! repository `<store>/git/serve.git` that host git's `upload-pack`, `receive-pack` and
//! `http-backend` run on, the map between store refs and git refs, git's pkt-line and
//! `proc-receive` codecs, and the land key of a push to `refs/land/<target>`.
//!
//! ```text
//! swarmops-ops serve ──▶ repo (serve.git, its lock and hook) ──▶ child (host git, one builder)
//!                    ├─▶ refmap (store ref ↔ git ref)
//!                    ├─▶ pkt, proc_receive (the hook's protocol)
//!                    ├─▶ land_key (refs/land/<target>)
//!                    └─▶ pack_key (the pack cache's key and digest)
//! ```
//!
//! Every module is built. [`repo::ServeError::NotBuilt`] stays in the contract, but no module
//! returns it.

pub mod child;
pub mod land_key;
pub mod pack_key;
pub mod pkt;
pub mod proc_receive;
pub mod refmap;
pub mod repo;
