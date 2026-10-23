//! Conflict markers (§5.3 of the M5 design): a conflicting text file written with diff3 markers,
//! merged again from the report's three blobs, in the report's hunk order.
//!
//! ```text
//!  <the merged lines before the hunk>
//!  <<<<<<< ours
//!  <ours' lines>
//!  ||||||| base
//!  <the base's lines>
//!  =======
//!  <theirs' lines>
//!  >>>>>>> theirs (<12 hex of theirs>)
//!  <the merged lines up to the next hunk>
//! ```
//!
//! The store's merge returns conflicts as data, never as markers, and the report's hunk texts may
//! be cut short: the blobs are read whole (`blob_get`) and merged again by the same function,
//! `merge_content`, whose hunks are the report's, in its order (file order). Binary, delete/modify,
//! mode and type conflicts are listed in the report and not written.

use std::collections::BTreeSet;

use swarmops_api::{ConflictKindOut, ConflictReport, FileConflictOut, OpError};
use swarmops_merge::conflict::{ConflictHunk, ContentMerge};
use swarmops_merge::diff3;
use swarmops_merge::line_diff;

use crate::Ctx;
use crate::materialize;
use crate::volume::{EXEC_BITS, EXEC_MODE};
use crate::workspace::Workspace;

/// The line that opens a conflict block: ours' lines follow.
pub const OURS_MARKER: &str = "<<<<<<< ours";

/// The line before the base's lines.
pub const BASE_MARKER: &str = "||||||| base";

/// The line before theirs' lines.
pub const SPLIT_MARKER: &str = "=======";

/// The start of the line that closes a block, followed by ` theirs (<12 hex>)`.
pub const THEIRS_MARKER: &str = ">>>>>>>";

/// How many hex digits of theirs' changeset the closing line names.
const SHORT_ID: usize = 12;

/// Writes each conflicting text file of `report` that workspace `ws` holds with diff3 markers,
/// over what the host holds there (the head's file: nothing else moved). The paths written.
pub(crate) fn write(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    report: &ConflictReport,
) -> Result<Vec<String>, OpError> {
    // 1. The files that get markers: text both sides edited, in the workspace's prefixes.
    let files: Vec<&FileConflictOut> = (report.files.iter())
        .filter(|file| {
            matches!(
                file.kind,
                ConflictKindOut::Content | ConflictKindOut::AddAdd
            )
        })
        .filter(|file| file.ours_blob.is_some() && file.theirs_blob.is_some())
        .filter(|file| ws.file.all || materialize::covered(&file.path, &ws.file.prefixes))
        .collect();
    if files.is_empty() {
        return Ok(Vec::new());
    }

    // 2. Their blobs, each once.
    let ids: BTreeSet<&str> = (files.iter())
        .flat_map(|file| [&file.base_blob, &file.ours_blob, &file.theirs_blob])
        .filter_map(|blob| blob.as_deref())
        .collect();
    let contents = materialize::fetch(ctx.store, ids.into_iter().collect())?;
    let blob = |id: &Option<String>| id.as_ref().map_or(&[][..], |id| contents[id].as_slice());

    // 3. Each file merged again and written with its markers, its executable bit kept.
    let label = format!(
        "{THEIRS_MARKER} theirs ({})",
        report.theirs.get(..SHORT_ID).unwrap_or(&report.theirs)
    );
    let mut written = Vec::new();
    for file in files {
        let ours = blob(&file.ours_blob);
        let theirs = blob(&file.theirs_blob);
        let Some(bytes) = render(blob(&file.base_blob), ours, theirs, &label) else {
            continue;
        };
        put(ctx, ws, &file.path, &bytes)?;
        written.push(file.path.clone());
    }
    Ok(written)
}

/// The merge of `ours` and `theirs` from `base` with each conflicting hunk as a block of
/// markers, `label` closing each; none when the contents do not merge by lines.
pub(crate) fn render(base: &[u8], ours: &[u8], theirs: &[u8], label: &str) -> Option<Vec<u8>> {
    // 1. The merge's hunks, in file order; a clean merge is its bytes.
    let hunks = match diff3::merge_content(base, ours, theirs) {
        ContentMerge::Clean(bytes) => return Some(bytes),
        ContentMerge::Binary => return None,
        ContentMerge::Conflicted(hunks) => hunks,
    };

    // 2. Between the hunks, the three sides' lines merged as they merge; each hunk a block.
    let sides = [
        line_diff::split_lines(base),
        line_diff::split_lines(ours),
        line_diff::split_lines(theirs),
    ];
    let mut out = Vec::with_capacity(ours.len().max(theirs.len()));
    let mut from = [0; 3];
    for hunk in &hunks {
        let to = starts(hunk);
        out.extend(between(&sides, from, to, label));
        block(&mut out, hunk, label);
        from = ends(hunk);
    }

    // 3. The lines after the last hunk.
    let lens = sides.each_ref().map(Vec::len);
    out.extend(between(&sides, from, lens, label));
    Some(out)
}

/// Whether `bytes` hold a line that opens, divides or closes a block of markers.
pub(crate) fn marked(bytes: &[u8]) -> bool {
    line_diff::split_lines(bytes).into_iter().any(|line| {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        line.starts_with(OURS_MARKER.as_bytes())
            || line.starts_with(BASE_MARKER.as_bytes())
            || line.starts_with(THEIRS_MARKER.as_bytes())
    })
}

/// Where `hunk` starts on each side: base, ours, theirs.
fn starts(hunk: &ConflictHunk) -> [usize; 3] {
    [hunk.base.start, hunk.ours.start, hunk.theirs.start].map(index)
}

/// Where `hunk` ends on each side: base, ours, theirs.
fn ends(hunk: &ConflictHunk) -> [usize; 3] {
    [hunk.base.end, hunk.ours.end, hunk.theirs.end].map(index)
}

/// A line number as an index.
fn index(line: u32) -> usize {
    usize::try_from(line).unwrap_or(usize::MAX)
}

/// The three sides' lines `from..to` (base, ours, theirs), which no hunk touches, merged: the
/// side that changed them, as the whole merge took it. Should they not merge cleanly alone, they
/// are a block too, so that nothing is lost.
fn between(sides: &[Vec<&[u8]>; 3], from: [usize; 3], to: [usize; 3], label: &str) -> Vec<u8> {
    let [base, ours, theirs] = [0, 1, 2].map(|side| sides[side][from[side]..to[side]].concat());
    match diff3::merge_content(&base, &ours, &theirs) {
        ContentMerge::Clean(bytes) => bytes,
        ContentMerge::Binary | ContentMerge::Conflicted(_) => {
            let mut out = Vec::new();
            push_block(&mut out, [&base, &ours, &theirs], label);
            out
        }
    }
}

/// Appends `hunk`'s block of markers to `out`.
fn block(out: &mut Vec<u8>, hunk: &ConflictHunk, label: &str) {
    let texts = [&hunk.base_text, &hunk.ours_text, &hunk.theirs_text];
    push_block(out, texts.map(Vec::as_slice), label);
}

/// Appends a block of markers holding `base`, `ours` and `theirs` to `out`, each text ended by a
/// newline before the next marker.
fn push_block(out: &mut Vec<u8>, [base, ours, theirs]: [&[u8]; 3], label: &str) {
    let parts = [
        (OURS_MARKER, ours),
        (BASE_MARKER, base),
        (SPLIT_MARKER, theirs),
    ];
    for (marker, text) in parts {
        out.extend_from_slice(marker.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(text);
        if !text.is_empty() && !text.ends_with(b"\n") {
            out.push(b'\n');
        }
    }
    out.extend_from_slice(label.as_bytes());
    out.push(b'\n');
}

/// Writes `bytes` over the workspace's file at `path`, keeping its executable bit where the
/// volume keeps it.
fn put(ctx: &Ctx<'_>, ws: &Workspace, path: &str, bytes: &[u8]) -> Result<(), OpError> {
    // 1. Whether the file there is executable, before it is replaced.
    let abs = ws.dir.join(path);
    let host = ctx.host.as_ref();
    let meta = host
        .symlink_metadata(&abs)
        .map_err(|e| materialize::host_failed(path, &e))?;
    let exec = ws.file.volume.exec_bit && meta.is_some_and(|meta| meta.mode & EXEC_BITS != 0);

    // 2. The file, its directory made if the head had none there (an add/add).
    if let Some((parent, _)) = path.rsplit_once('/') {
        (host.create_dir_all(&ws.dir.join(parent)))
            .map_err(|e| materialize::host_failed(parent, &e))?;
    }
    (host.write_file(&abs, bytes)).map_err(|e| materialize::host_failed(path, &e))?;
    if exec {
        (host.set_mode(&abs, EXEC_MODE)).map_err(|e| materialize::host_failed(path, &e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A label as a rebase onto `t…` writes it.
    const LABEL: &str = ">>>>>>> theirs (tttttttttttt)";

    /// One hunk in the middle: the merged lines around it, the block in its place.
    #[test]
    fn one_hunk_is_one_block_between_the_merged_lines() {
        let out = render(b"a\nb\nc\n", b"a\nO\nc\n", b"a\nT\nc\n", LABEL).unwrap();
        let expected =
            format!("a\n{OURS_MARKER}\nO\n{BASE_MARKER}\nb\n{SPLIT_MARKER}\nT\n{LABEL}\nc\n");
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    /// A last line without a newline still ends before the next marker.
    #[test]
    fn a_text_without_its_newline_gets_one_before_the_marker() {
        let out = render(b"a\nb", b"a\nO", b"a\nT", LABEL).unwrap();
        let expected =
            format!("a\n{OURS_MARKER}\nO\n{BASE_MARKER}\nb\n{SPLIT_MARKER}\nT\n{LABEL}\n");
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    /// Binary content gets no markers; a clean merge is its bytes.
    #[test]
    fn binary_gets_none_and_clean_is_the_merge() {
        assert_eq!(render(b"x\0a", b"x\0b", b"x\0c", LABEL), None);
        assert_eq!(
            render(b"a\n", b"b\n", b"a\n", LABEL).as_deref(),
            Some(&b"b\n"[..])
        );
    }

    /// The markers are found at the start of a line, and only there.
    #[test]
    fn marker_lines_are_found_at_a_lines_start() {
        assert!(marked(b"x\n<<<<<<< ours\ny\n"));
        assert!(marked(b">>>>>>> theirs (abc)"));
        assert!(marked(b"||||||| base\r\n"));
        assert!(!marked(b"x <<<<<<< ours\n=======\n"));
    }
}
