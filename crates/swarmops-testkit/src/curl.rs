//! A stand-in for `curl`, for the tests of `auth login`'s device flow: SwarmOps asks GitHub for
//! a code and a token by running the host's curl, and a test must neither reach GitHub nor
//! depend on what it would answer.
//!
//! ```text
//!   so auth login --gh ── SWARMOPS_CURL=<dir>/curl ── the stand-in
//!                              request n:  its arguments, stdin and variables ──► <dir>/calls/
//!                              answer n:   what the test queued ◄── <dir>/answers/
//! ```
//!
//! [`FakeCurl::path`] is the value for the product's variable ([`CURL_ENV`]). Each run of the
//! stand-in records what it was run with ([`FakeCurl::calls`]) and prints the next answer the
//! test queued ([`FakeCurl::answer`], [`FakeCurl::print`], [`FakeCurl::fail`]); a run with no
//! answer left prints nothing and exits 97. Its directory is written into the script, as the
//! product clears its children's environment.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::remote;

/// The variable that names the curl the binary runs: `swarmops-cli`'s, which this crate cannot
/// name by its constant.
pub const CURL_ENV: &str = "SWARMOPS_CURL";

/// The exit code of a run of the stand-in that had no answer queued.
pub const NO_ANSWER_EXIT: i32 = 97;

/// The stand-in: records a run, then prints its answer and exits with its code. It runs `cat`
/// and `env` by their paths, since the product may give it no `PATH`.
const SCRIPT: &str = r#"#!/bin/sh
dir=@DIR@
n=1
while [ -e "$dir/calls/$n.args" ]; do n=$((n + 1)); done
for arg in "$@"; do printf '%s\n' "$arg"; done > "$dir/calls/$n.args"
/bin/cat > "$dir/calls/$n.stdin"
/usr/bin/env > "$dir/calls/$n.env"
[ -e "$dir/answers/$n.exit" ] || exit 97
/bin/cat "$dir/answers/$n.out"
read -r code < "$dir/answers/$n.exit"
exit "$code"
"#;

/// A stand-in for curl in a directory of its own, with the answers a test queues for its runs
/// and the record of each run.
#[derive(Debug)]
pub struct FakeCurl {
    /// The stand-in (`curl`), its answers (`answers/`) and its runs (`calls/`).
    dir: TempDir,
}

/// One run of the stand-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurlCall {
    /// Its arguments.
    pub args: Vec<String>,
    /// All it read on stdin: the request, as a curl configuration.
    pub stdin: String,
    /// The names of the variables it ran with, the shell's own (`PWD`, `SHLVL`, `_`) among
    /// them.
    pub env: Vec<String>,
}

impl Default for FakeCurl {
    fn default() -> FakeCurl {
        FakeCurl::new()
    }
}

impl FakeCurl {
    /// A stand-in with no answer queued.
    pub fn new() -> FakeCurl {
        let dir = tempfile::tempdir().expect("a temp dir for the curl stand-in");
        for sub in ["answers", "calls"] {
            fs::create_dir(dir.path().join(sub)).expect("make the stand-in's directory");
        }
        let at = dir.path().to_str().expect("a UTF-8 temp dir");
        let script = SCRIPT.replace("@DIR@", &remote::sh_quote(at));
        remote::write_script(&dir.path().join("curl"), &script);
        FakeCurl { dir }
    }

    /// The stand-in's path: the value for [`CURL_ENV`].
    pub fn path(&self) -> PathBuf {
        self.dir.path().join("curl")
    }

    /// Queues an HTTP answer for the next run without one: `body`, then the status on a line of
    /// its own, as the product asks its curl to print them; exit 0.
    pub fn answer(&self, status: u16, body: &str) -> &FakeCurl {
        self.queue(&format!("{body}\n{status}"), 0)
    }

    /// Queues `printed`, as it stands, as all the next run prints; exit 0.
    pub fn print(&self, printed: &str) -> &FakeCurl {
        self.queue(printed, 0)
    }

    /// Queues a failure for the next run: nothing printed, and curl's exit code `exit`.
    pub fn fail(&self, exit: i32) -> &FakeCurl {
        self.queue("", exit)
    }

    /// Queues `printed` and `exit` for the next run without an answer.
    fn queue(&self, printed: &str, exit: i32) -> &FakeCurl {
        let answers = self.dir.path().join("answers");
        let n = numbered(&answers, "exit").len() + 1;
        fs::write(answers.join(format!("{n}.out")), printed).expect("queue an answer");
        fs::write(answers.join(format!("{n}.exit")), format!("{exit}\n")).expect("queue an exit");
        self
    }

    /// The runs so far, in order.
    pub fn calls(&self) -> Vec<CurlCall> {
        let calls = self.dir.path().join("calls");
        let read = |file: PathBuf| fs::read_to_string(file).expect("read a run's record");
        numbered(&calls, "args")
            .into_iter()
            .map(|n| CurlCall {
                args: read(calls.join(format!("{n}.args")))
                    .lines()
                    .map(str::to_owned)
                    .collect(),
                stdin: read(calls.join(format!("{n}.stdin"))),
                env: read(calls.join(format!("{n}.env")))
                    .lines()
                    .filter_map(|line| line.split_once('='))
                    .map(|(name, _)| name.to_owned())
                    .collect(),
            })
            .collect()
    }
}

/// The numbers `n` of the files `<n>.<extension>` in `dir`, in order.
fn numbered(dir: &Path, extension: &str) -> Vec<usize> {
    let mut numbers: Vec<usize> = fs::read_dir(dir)
        .expect("read the stand-in's directory")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == extension))
        .filter_map(|path| path.file_stem()?.to_str()?.parse().ok())
        .collect();
    numbers.sort_unstable();
    numbers
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::process::{Command, Output, Stdio};

    use super::*;

    /// Runs the stand-in with `args`, `stdin` and no variable but `KEPT`.
    fn run(curl: &FakeCurl, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(curl.path())
            .args(args)
            .env_clear()
            .env("KEPT", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run the stand-in");
        let mut pipe = child.stdin.take().expect("a piped stdin");
        pipe.write_all(stdin.as_bytes()).expect("write its stdin");
        drop(pipe);
        child.wait_with_output().expect("wait for the stand-in")
    }

    #[test]
    fn the_stand_in_answers_in_order_and_records_each_run() {
        let curl = FakeCurl::new();
        curl.answer(200, "{\"a\": 1}").fail(7).print("no status");

        // 1. Each run gets the next answer; one more than was queued gets none.
        let first = run(&curl, &["-q", "--config", "-"], "url = \"x\"\n");
        assert_eq!(first.status.code(), Some(0));
        assert_eq!(first.stdout, b"{\"a\": 1}\n200");
        let second = run(&curl, &[], "");
        assert_eq!((second.status.code(), second.stdout.len()), (Some(7), 0));
        let third = run(&curl, &["two words", "x"], "second request\n");
        assert_eq!(third.stdout, b"no status");
        let fourth = run(&curl, &[], "");
        assert_eq!(fourth.status.code(), Some(NO_ANSWER_EXIT));

        // 2. What each ran with, the one without an answer too.
        let calls = curl.calls();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0].args, ["-q", "--config", "-"]);
        assert_eq!(calls[0].stdin, "url = \"x\"\n");
        assert!(calls[0].env.contains(&"KEPT".to_owned()), "{:?}", calls[0]);
        assert!(!calls[0].env.contains(&"HOME".to_owned()), "{:?}", calls[0]);
        assert_eq!((calls[1].args.len(), calls[1].stdin.as_str()), (0, ""));
        assert_eq!(calls[2].args, ["two words", "x"]);
        assert_eq!(calls[2].stdin, "second request\n");
    }
}
