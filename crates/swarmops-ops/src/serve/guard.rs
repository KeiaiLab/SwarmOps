//! What a client of `serve` receives carries no host path (M14 design §8): [`Redactor`] masks
//! the store's root, `--root` and `serve.git` in every text sent, and [`ServerOps`] wraps
//! [`StoreOps`] so that a request's `{"file": path}` content is refused.
//!
//! ```text
//! request ─▶ ServerOps ─▶ {"file"} anywhere? ─ yes ─▶ E_INVALID_ARG file_content_refused
//!                              │ no
//!                              ▼
//!                         inner StoreOps ─▶ Ok(resp)  as is (API types hold no host path)
//!                                       └─▶ Err(e)    message, hint, details masked
//! ```

use std::path::Path;

use serde_json::Value;
use swarmops_api::{
    BlobGetReq, BlobGetResp, BlobHasReq, BlobHasResp, BlobPutReq, BlobPutResp, ChangesetDetail,
    CommitReq, CommitResp, ContentInput, DeriveReq, DeriveResp, Diff, DiffReq, ErrorCode,
    FileContent, GcReq, GcResp, Identity, InfoReq, LandReq, LandResp, LogPage, LogReq, OpError,
    OpLogPage, OpLogReq, PathHistoryPage, PathHistoryReq, ReadFileReq, ReadTreeReq, RebaseReq,
    RebaseResp, RefDeleteReq, RefListReq, RefListResp, RefSetReq, RefSetResp, RepairReq,
    RepairResp, RepoInfo, ResolutionsInput, ResolveRevReq, ResolvedRev, ShowReq, StoreOps,
    TreeListing, UndoReq, UndoResp, UpgradeReq, UpgradeResp, VerifyReport, VerifyReq,
};

use super::refused;

/// What a masked path reads as, until each path gets its own name.
const MASK: &str = "<path>";

/// The shortest path text masked: `/` alone, or `.`, would mask every separator.
const MIN_PATH_LEN: usize = 2;

/// Masks host paths in the texts a client receives.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Redactor {
    /// The paths masked, as their text, longest first.
    paths: Vec<String>,
}

impl Redactor {
    /// The redactor of `paths`, each also in its absolute and canonical forms where they
    /// differ: `/tmp/x` masks `/private/tmp/x` too where `/tmp` is a link.
    pub fn new(paths: &[&Path]) -> Redactor {
        // 1. Each path's forms, as written, absolute, and with links resolved.
        let mut texts = Vec::new();
        for path in paths {
            texts.push(text_of(path));
            if let Ok(absolute) = std::path::absolute(path) {
                texts.push(text_of(&absolute));
            }
            if let Ok(canonical) = std::fs::canonicalize(path) {
                texts.push(text_of(&canonical));
            }
        }

        // 2. Longest first, so that a path holding another is masked whole.
        texts.retain(|text| text.len() >= MIN_PATH_LEN);
        texts.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        texts.dedup();
        Redactor { paths: texts }
    }

    /// `text`, each path masked.
    pub fn redact(&self, text: &str) -> String {
        self.paths.iter().fold(text.to_owned(), |text, path| {
            text.replace(path.as_str(), MASK)
        })
    }

    /// `error`, its message, hint and every string of its details masked.
    pub(crate) fn redact_error(&self, mut error: OpError) -> OpError {
        error.message = self.redact(&error.message);
        error.hint = error.hint.map(|hint| self.redact(&hint));
        for value in error.details.values_mut() {
            self.redact_value(value);
        }
        error
    }

    /// `value` with every string inside it masked.
    fn redact_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.redact(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.redact_value(item)),
            Value::Object(map) => map.values_mut().for_each(|item| self.redact_value(item)),
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

/// `path` as text, without a trailing separator.
fn text_of(path: &Path) -> String {
    let text = path.to_string_lossy();
    let trimmed = text.trim_end_matches('/');
    trimmed.to_owned()
}

/// The store's operations as a server serves them: those of `O`, with a request's `{"file"}`
/// content refused and every error masked.
#[derive(Debug)]
#[non_exhaustive]
pub struct ServerOps<O: StoreOps> {
    /// The operations served.
    inner: O,
    /// What masks the errors sent back.
    redactor: Redactor,
}

impl<O: StoreOps> ServerOps<O> {
    /// `inner`, served, masking no path until [`ServerOps::with_redactor`] names some.
    pub fn new(inner: O) -> ServerOps<O> {
        ServerOps {
            inner,
            redactor: Redactor::new(&[]),
        }
    }

    /// The same, its errors masked by `redactor`.
    #[must_use]
    pub fn with_redactor(mut self, redactor: Redactor) -> ServerOps<O> {
        self.redactor = redactor;
        self
    }

    /// The operations served.
    pub fn inner(&self) -> &O {
        &self.inner
    }

    /// `result` as the client gets it: its error masked.
    fn pass<T>(&self, result: Result<T, OpError>) -> Result<T, OpError> {
        result.map_err(|error| self.redactor.redact_error(error))
    }
}

/// `E_INVALID_ARG` `file_content_refused`: a request names a file on the client's machine,
/// which a server never reads.
fn file_refused() -> OpError {
    refused(
        ErrorCode::InvalidArg,
        "file_content_refused",
        "a {\"file\": …} content names a path on the client's machine, which serve does not read",
    )
    .with_hint("send the bytes instead: {\"inline\": …} or {\"base64\": …}, or blob_put then {\"blob\": id}")
}

/// Whether `content` names a client file.
fn is_file(content: Option<&ContentInput>) -> bool {
    matches!(content, Some(ContentInput::File(_)))
}

/// Whether any of a land's or a rebase's resolutions names a client file.
fn resolves_from_file(resolutions: Option<&ResolutionsInput>) -> bool {
    resolutions.is_some_and(|r| r.files.values().any(|f| is_file(f.content.as_ref())))
}

impl<O: StoreOps> StoreOps for ServerOps<O> {
    fn repo_info(&self, req: InfoReq) -> Result<RepoInfo, OpError> {
        self.pass(self.inner.repo_info(req))
    }

    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError> {
        if req.changes.iter().any(|c| is_file(c.content.as_ref())) {
            return Err(file_refused());
        }
        self.pass(self.inner.commit(who, req))
    }

    fn land(&self, who: &Identity, req: LandReq) -> Result<LandResp, OpError> {
        if resolves_from_file(req.resolutions.as_ref()) {
            return Err(file_refused());
        }
        self.pass(self.inner.land(who, req))
    }

    fn rebase(&self, who: &Identity, req: RebaseReq) -> Result<RebaseResp, OpError> {
        if resolves_from_file(req.resolutions.as_ref()) {
            return Err(file_refused());
        }
        self.pass(self.inner.rebase(who, req))
    }

    fn resolve_rev(&self, req: ResolveRevReq) -> Result<ResolvedRev, OpError> {
        self.pass(self.inner.resolve_rev(req))
    }

    fn blob_put(&self, who: &Identity, req: BlobPutReq) -> Result<BlobPutResp, OpError> {
        if is_file(Some(&req.content)) {
            return Err(file_refused());
        }
        self.pass(self.inner.blob_put(who, req))
    }

    fn blob_has(&self, req: BlobHasReq) -> Result<BlobHasResp, OpError> {
        self.pass(self.inner.blob_has(req))
    }

    fn blob_get(&self, req: BlobGetReq) -> Result<BlobGetResp, OpError> {
        self.pass(self.inner.blob_get(req))
    }

    fn read_file(&self, req: ReadFileReq) -> Result<FileContent, OpError> {
        self.pass(self.inner.read_file(req))
    }

    fn read_tree(&self, req: ReadTreeReq) -> Result<TreeListing, OpError> {
        self.pass(self.inner.read_tree(req))
    }

    fn show(&self, req: ShowReq) -> Result<ChangesetDetail, OpError> {
        self.pass(self.inner.show(req))
    }

    fn log(&self, req: LogReq) -> Result<LogPage, OpError> {
        self.pass(self.inner.log(req))
    }

    fn path_history(&self, req: PathHistoryReq) -> Result<PathHistoryPage, OpError> {
        self.pass(self.inner.path_history(req))
    }

    fn diff(&self, req: DiffReq) -> Result<Diff, OpError> {
        self.pass(self.inner.diff(req))
    }

    fn ref_list(&self, req: RefListReq) -> Result<RefListResp, OpError> {
        self.pass(self.inner.ref_list(req))
    }

    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError> {
        self.pass(self.inner.ref_set(who, req))
    }

    fn ref_delete(&self, who: &Identity, req: RefDeleteReq) -> Result<RefSetResp, OpError> {
        self.pass(self.inner.ref_delete(who, req))
    }

    fn op_log(&self, req: OpLogReq) -> Result<OpLogPage, OpError> {
        self.pass(self.inner.op_log(req))
    }

    fn undo(&self, who: &Identity, req: UndoReq) -> Result<UndoResp, OpError> {
        self.pass(self.inner.undo(who, req))
    }

    fn verify(&self, req: VerifyReq) -> Result<VerifyReport, OpError> {
        self.pass(self.inner.verify(req))
    }

    fn derive(&self, req: DeriveReq) -> Result<DeriveResp, OpError> {
        self.pass(self.inner.derive(req))
    }

    fn gc(&self, req: GcReq) -> Result<GcResp, OpError> {
        self.pass(self.inner.gc(req))
    }

    fn repair(&self, req: RepairReq) -> Result<RepairResp, OpError> {
        self.pass(self.inner.repair(req))
    }

    fn upgrade(&self, req: UpgradeReq) -> Result<UpgradeResp, OpError> {
        self.pass(self.inner.upgrade(req))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::Path;

    use swarmops_api::*;

    use super::{Redactor, ServerOps};

    /// The host path every error of [`Recording`] names.
    const ROOT: &str = "/srv/so";

    /// A store that records each call by name and fails it with an error naming a host path.
    #[derive(Debug, Default)]
    struct Recording {
        calls: RefCell<Vec<&'static str>>,
    }

    impl Recording {
        /// Records `call`, and fails it naming a file below [`ROOT`].
        fn call<T>(&self, call: &'static str) -> Result<T, OpError> {
            self.calls.borrow_mut().push(call);
            let mut error = OpError::new(
                ErrorCode::Internal,
                format!("{call} failed at {ROOT}/acme/x"),
            )
            .with_hint(format!("look at {ROOT}/acme"));
            error
                .details
                .insert("path".to_owned(), format!("{ROOT}/acme/y").into());
            Err(error)
        }
    }

    impl StoreOps for Recording {
        fn repo_info(&self, _: InfoReq) -> Result<RepoInfo, OpError> {
            self.call("repo_info")
        }
        fn commit(&self, _: &Identity, _: CommitReq) -> Result<CommitResp, OpError> {
            self.call("commit")
        }
        fn land(&self, _: &Identity, _: LandReq) -> Result<LandResp, OpError> {
            self.call("land")
        }
        fn rebase(&self, _: &Identity, _: RebaseReq) -> Result<RebaseResp, OpError> {
            self.call("rebase")
        }
        fn resolve_rev(&self, _: ResolveRevReq) -> Result<ResolvedRev, OpError> {
            self.call("resolve_rev")
        }
        fn blob_put(&self, _: &Identity, _: BlobPutReq) -> Result<BlobPutResp, OpError> {
            self.call("blob_put")
        }
        fn blob_has(&self, _: BlobHasReq) -> Result<BlobHasResp, OpError> {
            self.call("blob_has")
        }
        fn blob_get(&self, _: BlobGetReq) -> Result<BlobGetResp, OpError> {
            self.call("blob_get")
        }
        fn read_file(&self, _: ReadFileReq) -> Result<FileContent, OpError> {
            self.call("read_file")
        }
        fn read_tree(&self, _: ReadTreeReq) -> Result<TreeListing, OpError> {
            self.call("read_tree")
        }
        fn show(&self, _: ShowReq) -> Result<ChangesetDetail, OpError> {
            self.call("show")
        }
        fn log(&self, _: LogReq) -> Result<LogPage, OpError> {
            self.call("log")
        }
        fn path_history(&self, _: PathHistoryReq) -> Result<PathHistoryPage, OpError> {
            self.call("path_history")
        }
        fn diff(&self, _: DiffReq) -> Result<Diff, OpError> {
            self.call("diff")
        }
        fn ref_list(&self, _: RefListReq) -> Result<RefListResp, OpError> {
            self.call("ref_list")
        }
        fn ref_set(&self, _: &Identity, _: RefSetReq) -> Result<RefSetResp, OpError> {
            self.call("ref_set")
        }
        fn ref_delete(&self, _: &Identity, _: RefDeleteReq) -> Result<RefSetResp, OpError> {
            self.call("ref_delete")
        }
        fn op_log(&self, _: OpLogReq) -> Result<OpLogPage, OpError> {
            self.call("op_log")
        }
        fn undo(&self, _: &Identity, _: UndoReq) -> Result<UndoResp, OpError> {
            self.call("undo")
        }
        fn verify(&self, _: VerifyReq) -> Result<VerifyReport, OpError> {
            self.call("verify")
        }
        fn derive(&self, _: DeriveReq) -> Result<DeriveResp, OpError> {
            self.call("derive")
        }
        fn gc(&self, _: GcReq) -> Result<GcResp, OpError> {
            self.call("gc")
        }
        fn repair(&self, _: RepairReq) -> Result<RepairResp, OpError> {
            self.call("repair")
        }
        fn upgrade(&self, _: UpgradeReq) -> Result<UpgradeResp, OpError> {
            self.call("upgrade")
        }
    }

    /// The recording store served, masking [`ROOT`].
    fn served() -> ServerOps<Recording> {
        ServerOps::new(Recording::default()).with_redactor(Redactor::new(&[Path::new(ROOT)]))
    }

    fn who() -> Identity {
        Identity::parse("person/ada").unwrap()
    }

    /// A client file's content.
    fn file() -> ContentInput {
        ContentInput::File("/home/ada/secret".to_owned())
    }

    /// The error `served` passed on for `call`: the inner store's, with no host path left.
    fn assert_passed(served: &ServerOps<Recording>, call: &str, result: Result<(), OpError>) {
        let error = result.unwrap_err();
        assert_eq!(
            error.code,
            ErrorCode::Internal.as_str(),
            "{call}: {error:?}"
        );
        let text = serde_json::to_string(&error).unwrap();
        assert!(!text.contains(ROOT), "{call}: {text}");
        assert!(text.contains("<path>/acme"), "{call}: {text}");
        assert_eq!(
            served.inner().calls.borrow().last().copied(),
            Some(call),
            "{call}"
        );
    }

    /// `error` is `file_content_refused`, and the inner store saw no call.
    fn assert_refused(served: &ServerOps<Recording>, error: &OpError) {
        assert_eq!(error.code, ErrorCode::InvalidArg.as_str(), "{error:?}");
        assert_eq!(error.details["reason"], "file_content_refused", "{error:?}");
        assert!(served.inner().calls.borrow().is_empty(), "{error:?}");
        assert!(
            !serde_json::to_string(&error).unwrap().contains("/home/ada"),
            "{error:?}"
        );
    }

    /// The root, its canonical form, and a path below it are masked; other text is kept.
    #[test]
    fn redactor_masks_the_root_its_canonical_form_and_below() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical = std::fs::canonicalize(&real).unwrap();

        let redactor = Redactor::new(&[&link]);
        let text = format!(
            "at {} and {} and {}/acme/git/serve.git, nothing else",
            link.display(),
            canonical.display(),
            canonical.display(),
        );
        let masked = redactor.redact(&text);
        assert_eq!(
            masked,
            "at <path> and <path> and <path>/acme/git/serve.git, nothing else"
        );
    }

    /// A path that holds another is masked whole, not left half shown.
    #[test]
    fn redactor_masks_the_longest_path_first() {
        let redactor = Redactor::new(&[Path::new("/srv"), Path::new("/data/srv")]);
        assert_eq!(redactor.redact("/data/srv/acme"), "<path>/acme");
    }

    /// `/` and an empty path mask nothing: they would mask every separator.
    #[test]
    fn redactor_skips_the_filesystem_root() {
        let redactor = Redactor::new(&[Path::new("/"), Path::new("")]);
        assert_eq!(redactor.redact("a/b"), "a/b");
    }

    /// A commit with a client file's content is refused before the inner store is called.
    #[test]
    fn commit_refuses_file_content() {
        let served = served();
        let mut change = ChangeInput::new("a.txt", ChangeOp::Put);
        change.content = Some(file());
        let req = CommitReq::new(vec![change], "m");
        assert_refused(&served, &served.commit(&who(), req).unwrap_err());
    }

    /// `blob_put`, and a land's or a rebase's resolution, refuse a client file too.
    #[test]
    fn other_content_inputs_refuse_file_content() {
        let served = served();
        let blob = served
            .blob_put(&who(), BlobPutReq::new(file()))
            .unwrap_err();
        assert_refused(&served, &blob);

        let mut resolution = ResolutionInput::default();
        resolution.content = Some(file());
        let files = BTreeMap::from([("a.txt".to_owned(), resolution)]);
        let resolutions = ResolutionsInput::new("0".repeat(64), files);

        let mut land = LandReq::new("x");
        land.resolutions = Some(resolutions.clone());
        assert_refused(&served, &served.land(&who(), land).unwrap_err());

        let mut rebase = RebaseReq::new("x");
        rebase.resolutions = Some(resolutions);
        assert_refused(&served, &served.rebase(&who(), rebase).unwrap_err());
    }

    /// Content the client sends as bytes reaches the inner store.
    #[test]
    fn commit_passes_inline_content() {
        let served = served();
        let mut change = ChangeInput::new("a.txt", ChangeOp::Put);
        change.content = Some(ContentInput::Inline("x".to_owned()));
        let req = CommitReq::new(vec![change], "m");
        assert_passed(&served, "commit", served.commit(&who(), req).map(drop));
    }

    /// Every read reaches the inner store, its error masked.
    #[test]
    fn reads_delegate() {
        let s = served();
        assert_passed(&s, "repo_info", s.repo_info(InfoReq::default()).map(drop));
        assert_passed(
            &s,
            "resolve_rev",
            s.resolve_rev(ResolveRevReq::new("main")).map(drop),
        );
        assert_passed(
            &s,
            "blob_has",
            s.blob_has(BlobHasReq::new(Vec::new())).map(drop),
        );
        assert_passed(
            &s,
            "blob_get",
            s.blob_get(BlobGetReq::new(Vec::new())).map(drop),
        );
        assert_passed(
            &s,
            "read_file",
            s.read_file(ReadFileReq::new("main", "a")).map(drop),
        );
        assert_passed(
            &s,
            "read_tree",
            s.read_tree(ReadTreeReq::new("main")).map(drop),
        );
        assert_passed(&s, "show", s.show(ShowReq::new("main")).map(drop));
        assert_passed(&s, "log", s.log(LogReq::default()).map(drop));
        let history = PathHistoryReq::new("a");
        assert_passed(&s, "path_history", s.path_history(history).map(drop));
        assert_passed(&s, "diff", s.diff(DiffReq::new("main", "main")).map(drop));
        assert_passed(&s, "ref_list", s.ref_list(RefListReq::default()).map(drop));
        assert_passed(&s, "op_log", s.op_log(OpLogReq::default()).map(drop));
        assert_passed(&s, "verify", s.verify(VerifyReq::default()).map(drop));
    }

    /// Every mutation reaches the inner store with the caller, its error masked.
    #[test]
    fn mutations_delegate() {
        let s = served();
        let w = who();
        assert_passed(&s, "land", s.land(&w, LandReq::new("x")).map(drop));
        assert_passed(&s, "rebase", s.rebase(&w, RebaseReq::new("x")).map(drop));
        let blob = BlobPutReq::new(ContentInput::Base64("eA==".to_owned()));
        assert_passed(&s, "blob_put", s.blob_put(&w, blob).map(drop));
        assert_passed(
            &s,
            "ref_set",
            s.ref_set(&w, RefSetReq::new("r", "x")).map(drop),
        );
        assert_passed(
            &s,
            "ref_delete",
            s.ref_delete(&w, RefDeleteReq::new("r", "x")).map(drop),
        );
        assert_passed(&s, "undo", s.undo(&w, UndoReq::new("1:00000000")).map(drop));
    }

    /// Derivation and maintenance reach the inner store, their errors masked.
    #[test]
    fn maintenance_delegates() {
        let s = served();
        assert_passed(&s, "derive", s.derive(DeriveReq::default()).map(drop));
        assert_passed(&s, "gc", s.gc(GcReq::default()).map(drop));
        assert_passed(&s, "repair", s.repair(RepairReq::default()).map(drop));
        assert_passed(&s, "upgrade", s.upgrade(UpgradeReq::default()).map(drop));
    }
}
