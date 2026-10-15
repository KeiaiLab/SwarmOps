//! SwarmOps API: the operation contract (operations spec §0, §7).
//!
//! Every operation takes a request type and returns a response type or an [`OpError`]. All of
//! them are serde and JSON Schema types, so the local store, a future remote client and the CLI
//! exchange exactly the same JSON. [`StoreOps`] is the store boundary and [`Identity`] the caller
//! of a mutation. The CLI prints one [`Envelope`] per command and exits with [`OpError::exit`];
//! `swarmops schema` prints [`schema_for_all`], whose snapshot is where contract changes are
//! reviewed.

pub mod envelope;
pub mod error;
pub mod identity;
pub mod ops;
pub mod rev;

pub use envelope::{API_VERSION, Envelope, Meta};
pub use error::{ErrorCode, OpError};
pub use identity::Identity;
pub use ops::{
    Attribution, CatReq, ChangeInput, ChangeOp, ChangeOut, ChangesetDetail, ChangesetSummary,
    CommitReq, CommitResp, ContentEncoding, ContentInput, DEFAULT_LOG_LIMIT, DEFAULT_LS_LIMIT,
    DEFAULT_MAX_BYTES, DEFAULT_MAX_CHANGES, EntryKindOut, FileContent, FileKind, IdentityInfo,
    InfoReq, InitReq, LogPage, LogReq, LsReq, ModeInput, RefDeleteReq, RefListReq, RefListResp,
    RefOut, RefSetReq, RefSetResp, RepoInfo, ShowReq, SignatureOut, Signatures, StoreOps,
    TreeEntryOut, TreeListing, VerifyReport, VerifyReq,
};
pub use rev::Rev;

/// The JSON Schema (2020-12) of every request, response and shared type, keyed by type name.
///
/// [`Envelope`] is listed as `Envelope<serde_json::Value>`: its `result` is an operation's
/// response, whose type has its own entry.
pub fn schema_for_all() -> serde_json::Value {
    macro_rules! schemas {
        ($($ty:ident $(<$arg:ty>)?),+ $(,)?) => {
            serde_json::Value::Object(serde_json::Map::from_iter([
                $((
                    stringify!($ty).to_owned(),
                    schemars::schema_for!($ty $(<$arg>)?).to_value(),
                ),)+
            ]))
        };
    }
    schemas![
        // Shared by every operation.
        Envelope<serde_json::Value>,
        Rev,
        Identity,
        ErrorCode,
        OpError,
        Meta,
        // init, info, whoami.
        InitReq,
        InfoReq,
        RepoInfo,
        IdentityInfo,
        // commit.
        CommitReq,
        ChangeInput,
        ChangeOp,
        ContentInput,
        ModeInput,
        CommitResp,
        // cat, ls.
        CatReq,
        FileContent,
        FileKind,
        ContentEncoding,
        EntryKindOut,
        LsReq,
        TreeListing,
        TreeEntryOut,
        // show, log.
        ShowReq,
        ChangesetDetail,
        Attribution,
        Signatures,
        SignatureOut,
        ChangeOut,
        LogReq,
        LogPage,
        ChangesetSummary,
        // ref list, ref set, ref delete, verify.
        RefListReq,
        RefListResp,
        RefOut,
        RefSetReq,
        RefSetResp,
        RefDeleteReq,
        VerifyReq,
        VerifyReport,
    ]
}
