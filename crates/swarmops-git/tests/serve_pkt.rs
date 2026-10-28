//! git's pkt-line and `proc-receive` codecs (`swarmops_git::serve::{pkt, proc_receive}`): framing
//! round trips and bounds, the `ERR` line, the v1 handshake, commands and reports, and byte
//! transcripts a git 2.43.0 `receive-pack` wrote to a `proc-receive` hook.

use std::io::{self, Cursor};

use swarmops_git::oid::GitOid;
use swarmops_git::serve::pkt::{self, Pkt};
use swarmops_git::serve::proc_receive::{self, Command, Report};

/// The tip of the probe's first commit.
const OLD: &str = "02f0202f51e66e6c98d6f7e420d304d6c9a95615";

/// The tip of the probe's second commit.
const NEW: &str = "aa98cdaea2b176d63ae407fbe244e3212a1c122b";

/// What git 2.43.0's `receive-pack` wrote to the hook for `git push R main HEAD:refs/heads/new
/// :refs/heads/old`: no capability, three commands (update, delete, create), the flush.
const PUSH_THREE: &[u8] = b"000eversion=1\n0000\
006502f0202f51e66e6c98d6f7e420d304d6c9a95615 aa98cdaea2b176d63ae407fbe244e3212a1c122b refs/heads/main\
006402f0202f51e66e6c98d6f7e420d304d6c9a95615 0000000000000000000000000000000000000000 refs/heads/old\
00640000000000000000000000000000000000000000 aa98cdaea2b176d63ae407fbe244e3212a1c122b refs/heads/new\
0000";

/// The same for `git push --atomic R HEAD:refs/heads/a1`: `atomic` after the version.
const PUSH_ATOMIC: &[u8] = b"0015version=1\x00atomic\n0000\
00630000000000000000000000000000000000000000 aa98cdaea2b176d63ae407fbe244e3212a1c122b refs/heads/a1\
0000";

/// The same for `git push -o ci.skip -o x=1 R HEAD:refs/heads/o1` with
/// `receive.advertisePushOptions=true` and a hook that asked for them: `push-options` after the
/// version, then a section of options after the commands.
const PUSH_OPTIONS: &[u8] = b"001bversion=1\x00push-options\n0000\
00630000000000000000000000000000000000000000 aa98cdaea2b176d63ae407fbe244e3212a1c122b refs/heads/o1\
0000000bci.skip0007x=10000";

/// The id `hex` writes.
fn oid(hex: &str) -> GitOid {
    hex.parse().unwrap()
}

/// Every pkt-line of `bytes`, to its end.
fn all(bytes: &[u8]) -> io::Result<Vec<Pkt>> {
    let mut input = Cursor::new(bytes);
    let mut lines = Vec::new();
    while let Some(line) = pkt::read(&mut input)? {
        lines.push(line);
    }
    Ok(lines)
}

/// The kind of error `pkt::read` gives on `bytes`.
fn read_error(bytes: &[u8]) -> io::ErrorKind {
    pkt::read(&mut Cursor::new(bytes)).unwrap_err().kind()
}

/// Data, flush and delimiter lines read back as written.
#[test]
fn round_trip() {
    // 1. Write a data line, an empty one and a flush; a delimiter has no writer, so its bytes.
    let mut out = Vec::new();
    pkt::write_data(&mut out, b"hello\n").unwrap();
    pkt::write_data(&mut out, b"").unwrap();
    out.extend_from_slice(b"0001");
    pkt::write_flush(&mut out).unwrap();
    assert_eq!(out, b"000ahello\n000400010000");

    // 2. Read them back, then the end.
    assert_eq!(
        all(&out).unwrap(),
        [
            Pkt::Data(b"hello\n".to_vec()),
            Pkt::Data(Vec::new()),
            Pkt::Delim,
            Pkt::Flush
        ]
    );

    // 3. The largest line, both ways; its length digits in upper case read too.
    let big = vec![b'x'; 65_516];
    let mut out = Vec::new();
    pkt::write_data(&mut out, &big).unwrap();
    assert_eq!(&out[..4], b"fff0");
    assert_eq!(all(&out).unwrap(), [Pkt::Data(big)]);
    assert_eq!(
        all(b"000AHELLO\n").unwrap(),
        [Pkt::Data(b"HELLO\n".to_vec())]
    );
}

/// A length under 4 (other than flush and delimiter), over 65,520, or not hex is `InvalidData`;
/// a line cut short is `UnexpectedEof`; a payload too long for a line is not written.
#[test]
fn bounds() {
    for bytes in [&b"0002"[..], b"0003", b"fff1", b"ffff", b"00g4", b"+004"] {
        assert_eq!(
            read_error(bytes),
            io::ErrorKind::InvalidData,
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
    for bytes in [&b"00"[..], b"0009abc"] {
        assert_eq!(read_error(bytes), io::ErrorKind::UnexpectedEof);
    }

    let mut out = Vec::new();
    let refused = pkt::write_data(&mut out, &vec![0; 65_517]).unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(out, b"");
}

/// `write_err` frames `ERR <message>`, the line git prints as `remote error: <message>`.
#[test]
fn err_line() {
    let mut out = Vec::new();
    pkt::write_err(&mut out, "E_AUTH: no identity").unwrap();
    assert_eq!(out, b"001bERR E_AUTH: no identity");
}

/// The v1 handshake reads receive-pack's version line and answers `version=1` with no
/// capability; another version is refused.
#[test]
fn handshake() {
    let mut out = Vec::new();
    proc_receive::handshake(&mut Cursor::new(b"000eversion=1\n0000"), &mut out).unwrap();
    assert_eq!(out, b"000eversion=1\n0000");

    // The NUL form with a capability git may add later is a version 1 line too.
    let mut out = Vec::new();
    let line = b"0015version=1\x00future\n0000";
    proc_receive::handshake(&mut Cursor::new(line), &mut out).unwrap();
    assert_eq!(out, b"000eversion=1\n0000");

    for bytes in [
        &b"000eversion=2\n0000"[..],
        b"000eversion=0\n0000",
        b"000dversion=\n0000",
        b"000ahello\n0000",
        b"0000",
    ] {
        let mut out = Vec::new();
        let refused = proc_receive::handshake(&mut Cursor::new(bytes), &mut out).unwrap_err();
        assert_eq!(
            refused.kind(),
            io::ErrorKind::InvalidData,
            "{}",
            String::from_utf8_lossy(bytes)
        );
        assert_eq!(out, b"", "nothing answered");
    }

    // receive-pack gone before its flush.
    let cut = proc_receive::handshake(&mut Cursor::new(b"000eversion=1\n"), &mut Vec::new());
    assert_eq!(cut.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
}

/// Three commands and their flush: create, update and delete, zero ids as `None`.
#[test]
fn three_commands() {
    let mut input = Vec::new();
    for line in [
        format!("{} {NEW} refs/heads/a", GitOid::NULL),
        format!("{OLD} {NEW} refs/heads/main\n"),
        format!("{OLD} {} refs/tags/v1", GitOid::NULL),
    ] {
        pkt::write_data(&mut input, line.as_bytes()).unwrap();
    }
    pkt::write_flush(&mut input).unwrap();
    input.extend_from_slice(b"0008rest");

    let mut cursor = Cursor::new(input);
    let commands = proc_receive::read_commands(&mut cursor).unwrap();
    assert_eq!(
        commands,
        [
            Command::new(None, Some(oid(NEW)), "refs/heads/a"),
            Command::new(Some(oid(OLD)), Some(oid(NEW)), "refs/heads/main"),
            Command::new(Some(oid(OLD)), None, "refs/tags/v1"),
        ]
    );
    assert_eq!(
        pkt::read(&mut cursor).unwrap(),
        Some(Pkt::Data(b"rest".to_vec())),
        "nothing read past the flush"
    );

    // A malformed line, a delimiter or an end before the flush is refused.
    for (bytes, kind) in [
        (&b"000cnot oids0000"[..], io::ErrorKind::InvalidData),
        (b"00010000", io::ErrorKind::InvalidData),
        (b"", io::ErrorKind::UnexpectedEof),
    ] {
        let refused = proc_receive::read_commands(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(refused.kind(), kind, "{}", String::from_utf8_lossy(bytes));
    }
    let empty_ref = format!("{OLD} {NEW} ");
    let mut bytes = Vec::new();
    pkt::write_data(&mut bytes, empty_ref.as_bytes()).unwrap();
    pkt::write_flush(&mut bytes).unwrap();
    let refused = proc_receive::read_commands(&mut Cursor::new(bytes)).unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::InvalidData);
}

/// Push options are not asked for (the answer names no capability, so receive-pack sends no
/// options section) and a receive-pack that offers them, or `atomic`, is refused: each ref has
/// its own CAS and no option is read.
#[test]
fn push_options() {
    // 1. Absent: the commands' flush ends what receive-pack sends.
    let mut input = Cursor::new(PUSH_THREE);
    let mut out = Vec::new();
    proc_receive::handshake(&mut input, &mut out).unwrap();
    assert!(!out.windows(12).any(|w| w == b"push-options"));
    assert_eq!(proc_receive::read_commands(&mut input).unwrap().len(), 3);
    assert_eq!(pkt::read(&mut input).unwrap(), None);

    // 2. Offered, or atomic asked for: refused at the handshake, nothing answered.
    for transcript in [PUSH_OPTIONS, PUSH_ATOMIC] {
        let mut out = Vec::new();
        let refused = proc_receive::handshake(&mut Cursor::new(transcript), &mut out).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidData);
        assert_eq!(out, b"");
    }
}

/// `ok` with its `option` lines, and `ng` with its reason, then the flush.
#[test]
fn reports() {
    let land = Report::Ok {
        refname: "refs/land/main".to_owned(),
        options: vec![
            ("refname".to_owned(), "refs/heads/main".to_owned()),
            ("old-oid".to_owned(), OLD.to_owned()),
            ("new-oid".to_owned(), NEW.to_owned()),
            ("forced-update".to_owned(), String::new()),
        ],
    };
    let reports = [
        land,
        Report::ok("refs/heads/x"),
        Report::ng(
            "refs/heads/main",
            "not a fast-forward; push to refs/land/main",
        ),
    ];
    let mut out = Vec::new();
    proc_receive::write_reports(&mut out, &reports).unwrap();

    let expected = [
        "ok refs/land/main\n".to_owned(),
        "option refname refs/heads/main\n".to_owned(),
        format!("option old-oid {OLD}\n"),
        format!("option new-oid {NEW}\n"),
        "option forced-update\n".to_owned(),
        "ok refs/heads/x\n".to_owned(),
        "ng refs/heads/main not a fast-forward; push to refs/land/main\n".to_owned(),
    ];
    let mut lines: Vec<Pkt> = expected
        .iter()
        .map(|line| Pkt::Data(line.as_bytes().to_vec()))
        .collect();
    lines.push(Pkt::Flush);
    assert_eq!(all(&out).unwrap(), lines);

    // A newline inside a field would forge a line: refused, nothing written.
    for report in [
        Report::ng("refs/heads/a", "two\nlines"),
        Report::ok("refs/heads/a\nok refs/heads/b"),
        Report::Ok {
            refname: "refs/heads/a".to_owned(),
            options: vec![("refname".to_owned(), "x\ny".to_owned())],
        },
    ] {
        let mut out = Vec::new();
        let refused = proc_receive::write_reports(&mut out, &[report]).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(out, b"");
    }
}

/// The transcripts git 2.43.0's `receive-pack` wrote decode: the handshake, then the commands
/// it sent.
#[test]
fn transcripts() {
    // 1. Three commands: update, delete, create.
    let mut input = Cursor::new(PUSH_THREE);
    proc_receive::handshake(&mut input, &mut Vec::new()).unwrap();
    assert_eq!(
        proc_receive::read_commands(&mut input).unwrap(),
        [
            Command::new(Some(oid(OLD)), Some(oid(NEW)), "refs/heads/main"),
            Command::new(Some(oid(OLD)), None, "refs/heads/old"),
            Command::new(None, Some(oid(NEW)), "refs/heads/new"),
        ]
    );

    // 2. Every line of the refused transcripts frames: version line, commands, options.
    assert_eq!(
        all(PUSH_OPTIONS).unwrap(),
        [
            Pkt::Data(b"version=1\0push-options\n".to_vec()),
            Pkt::Flush,
            Pkt::Data(format!("{} {NEW} refs/heads/o1", GitOid::NULL).into_bytes()),
            Pkt::Flush,
            Pkt::Data(b"ci.skip".to_vec()),
            Pkt::Data(b"x=1".to_vec()),
            Pkt::Flush,
        ]
    );
    assert_eq!(all(PUSH_ATOMIC).unwrap().len(), 4);
}
