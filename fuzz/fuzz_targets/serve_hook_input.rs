//! What git hands `serve`'s push hooks never panics them, and no ref name they read forges a
//! report line (M14 design §2.4): the input, raw bytes, read four ways.
//!
//! ```text
//! 1 pkt-lines to the end          each payload within a pkt-line's 65,516 bytes
//! 2 proc-receive: handshake ─▶ `version=1` and a flush answered, or nothing written
//!                 commands  ─▶ each ref's reports read back, or refused for a newline or length
//! 3 pre-receive: <old> <new> <ref> lines ─▶ the same reports check
//! ```
#![no_main]

use std::io::ErrorKind;

use libfuzzer_sys::fuzz_target;
use swarmops_git::serve::pkt::{self, Pkt};
use swarmops_git::serve::proc_receive::{self, Report};
use swarmops_ops::serve::hook::fuzz_read_updates;

/// The most bytes a pkt-line's payload holds: 65,520 less its four length digits.
const MAX_PAYLOAD: usize = 65_516;

/// The handshake's whole answer: `version=1`, then a flush.
const ANSWER: &[u8] = b"000eversion=1\n0000";

/// The reason every `ng` report gives.
const REASON: &str = "E_FORBIDDEN: refused [denied]";

fuzz_target!(|data: &[u8]| {
    // 1. Every pkt-line, to the end or the first error.
    let mut input = data;
    while let Ok(Some(line)) = pkt::read(&mut input) {
        if let Pkt::Data(payload) = line {
            assert!(payload.len() <= MAX_PAYLOAD, "{}", payload.len());
        }
    }

    // 2. proc-receive: the handshake against a sink, then the commands.
    let mut input = data;
    let mut sink = Vec::new();
    match proc_receive::handshake(&mut input, &mut sink) {
        Ok(()) => assert_eq!(sink, ANSWER),
        Err(_) => assert!(sink.is_empty(), "{sink:?}"),
    }
    if let Ok(commands) = proc_receive::read_commands(&mut input) {
        for command in commands {
            reports(&command.refname);
        }
    }

    // 3. pre-receive's lines.
    if let Ok(commands) = fuzz_read_updates(&mut &data[..]) {
        for command in commands {
            assert!(!command.refname.contains('\n'), "{:?}", command.refname);
            reports(&command.refname);
        }
    }
});

/// The reports on `refname`, `ok` and `ng`, either read back as written or refused whole: for a
/// newline, which would forge a line, or for a line no pkt-line holds.
fn reports(refname: &str) {
    // 1. Written, or refused with nothing written.
    let mut output = Vec::new();
    let written = [Report::ok(refname), Report::ng(refname, REASON)];
    if let Err(e) = proc_receive::write_reports(&mut output, &written) {
        assert_eq!(e.kind(), ErrorKind::InvalidInput, "{e}");
        assert!(output.is_empty(), "{output:?}");
        let long = format!("ng {refname} {REASON}\n").len() > MAX_PAYLOAD;
        assert!(refname.contains('\n') || long, "{refname:?}: {e}");
        return;
    }

    // 2. Read back: the two lines, then the flush, then nothing.
    let mut input = output.as_slice();
    let expected = [
        format!("ok {refname}\n"),
        format!("ng {refname} {REASON}\n"),
    ];
    for line in expected {
        let read = pkt::read(&mut input).ok().flatten();
        assert_eq!(read, Some(Pkt::Data(line.into_bytes())));
    }
    assert_eq!(pkt::read(&mut input).ok().flatten(), Some(Pkt::Flush));
    assert!(input.is_empty(), "{input:?}");
}
