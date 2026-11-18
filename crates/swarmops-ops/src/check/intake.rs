//! What a check reported, taken in once it has ended (M31-4): its report file, read into
//! findings within the run's bound.
//!
//! ```text
//!   the check ended, its process group gone (passed, failed, timed_out; never cancelled)
//!     └─ <run>/checks/<check>/report.jsonl (SWARMOPS_CHECK_REPORT), if there:
//!          open_bounded: a regular file, no link followed, no FIFO waited on, at most 64 MiB
//!          ─▶ findings::read_report_file ─▶ each finding through the run's Budget
//!   Intake: the findings returned, every finding counted, a CheckReportReadOut per report
//! ```
//!
//! A report that cannot be read is a state of that report, never the run's error: the check's
//! outcome stays the answer. Nothing here writes, and no store is opened.

use camino::Utf8Path;
use swarmops_api::{
    CheckFindingOut, CheckReportFault, CheckReportFormat, CheckReportReadOut, CheckReportState,
};

use super::findings::{Budget, read_report_file};
use super::{MAX_REPORT_BYTES, Opened, open_bounded};
use crate::error::one_line;

/// What one check's reports gave.
#[derive(Debug, Default)]
pub(crate) struct Intake {
    /// The findings the run returns, in order.
    pub(crate) findings: Vec<CheckFindingOut>,
    /// Every finding read, returned or not.
    pub(crate) total: u64,
    /// Each report, as it was read: the report file first, when there is one.
    pub(crate) reports: Vec<CheckReportReadOut>,
}

impl Intake {
    /// Takes in the findings of report number `index`, each through `budget`.
    fn add(&mut self, index: usize, findings: Vec<CheckFindingOut>, budget: &mut Budget) {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        for mut finding in findings {
            self.total += 1;
            finding.report = index;
            if budget.take(&finding) {
                self.findings.push(finding);
            }
        }
    }
}

/// Takes in what a check that ended reported in its report file at `report`, through the run's
/// `budget`. A check that wrote no report file has no report.
pub(crate) fn take_in(report: &Utf8Path, budget: &mut Budget) -> Intake {
    let mut intake = Intake::default();
    let refused = |fault, message: String| {
        let mut out = CheckReportReadOut::new(
            CheckReportFormat::Findings,
            report.as_str(),
            CheckReportState::Refused,
        );
        out.fault = Some(fault);
        out.message = Some(one_line(message));
        out
    };
    match open_bounded(report, MAX_REPORT_BYTES) {
        Ok(Opened::Missing) => {}
        Ok(Opened::NotAFile) => intake.reports.push(refused(
            CheckReportFault::NotAFile,
            "not a regular file: a symbolic link, a directory, a FIFO or another kind of entry, \
             not read"
                .to_owned(),
        )),
        Err(e) => intake.reports.push(refused(
            CheckReportFault::Io,
            format!("it cannot be read: {e}"),
        )),
        Ok(Opened::File { bytes, .. }) => {
            let read = read_report_file(&bytes);
            drop(bytes);
            let mut out = CheckReportReadOut::new(
                CheckReportFormat::Findings,
                report.as_str(),
                CheckReportState::Read,
            );
            out.findings = Some(u64::try_from(read.findings.len()).unwrap_or(u64::MAX));
            out.invalid_lines = read.invalid;
            out.cut = read.cut;
            out.message = match (read.first_invalid, read.cut) {
                (first, false) => first,
                (Some(first), true) => Some(format!("{first}; {}", cut_words())),
                (None, true) => Some(cut_words()),
            };
            let index = intake.reports.len();
            intake.reports.push(out);
            intake.add(index, read.findings, budget);
        }
    }
    intake
}

/// What the message of a report file past its bound says.
fn cut_words() -> String {
    format!(
        "past {} MiB: its lines after that were not read",
        MAX_REPORT_BYTES >> 20
    )
}
