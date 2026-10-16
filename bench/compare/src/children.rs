//! The child processes the workloads spawn (this binary, with a `__` subcommand): W2's writer
//! processes, W4's opener, killed writer and recoverer, and the traced sync-count writer.

use std::fs::File;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use crate::data::{chain, changeset, id, parse_id};
use crate::engines::{Engine, Main, Result, by_name, other};
use crate::workloads::w4::{Opened, Recovered};
use crate::workloads::writer_loop;

pub fn main(args: &[String]) -> ExitCode {
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e}", args.first().map_or("child", String::as_str));
            ExitCode::FAILURE
        }
    }
}

fn engine(name: &str) -> Result<Box<dyn Engine>> {
    by_name(name).ok_or_else(|| other(format!("no engine {name}")))
}

fn num(s: &str) -> Result<u64> {
    s.parse().map_err(|_| other(format!("not a number: {s}")))
}

fn hex_id(s: &str) -> Result<[u8; 32]> {
    parse_id(s).ok_or_else(|| other(format!("not an id: {s}")))
}

fn us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn arg(args: &[String], i: usize) -> Result<&str> {
    args.get(i)
        .map(String::as_str)
        .ok_or_else(|| other("missing argument"))
}

fn run(args: &[String]) -> Result<()> {
    match arg(args, 0)? {
        "__w2" => w2_writer(args),
        "__open" => open(args),
        "__writer" => killed_writer(args),
        "__recover" => recover(args),
        "__sync" => traced_writer(args),
        other_command => Err(other(format!("unknown child command {other_command}"))),
    }
}

/// `__w2 <engine> <dir> <writer> <txns> <root> <barrier dir>`
fn w2_writer(args: &[String]) -> Result<()> {
    let engine = engine(arg(args, 1)?)?;
    let (w, txns) = (num(arg(args, 3)?)?, num(arg(args, 4)?)?);
    let root = hex_id(arg(args, 5)?)?;
    let barrier = PathBuf::from(arg(args, 6)?);
    let db = engine.open(arg(args, 2)?.as_ref())?;
    let mut session = db.session()?;
    File::create(barrier.join(format!("ready-{w}")))?;
    while !barrier.join("go").exists() {
        thread::sleep(Duration::from_micros(200));
    }
    let result = writer_loop(&mut *session, w, txns, root)?;
    println!("{}", serde_json::to_string(&result).map_err(other)?);
    Ok(())
}

/// `__open <engine> <dir> <id>`: opens the store, looks the changeset up, prints the times.
fn open(args: &[String]) -> Result<()> {
    let engine = engine(arg(args, 1)?)?;
    let target = hex_id(arg(args, 3)?)?;
    let started = Instant::now();
    let db = engine.open(arg(args, 2)?.as_ref())?;
    let open = started.elapsed();
    let started = Instant::now();
    let found = db.session()?.get(&target)?;
    let first = started.elapsed();
    if found.is_none() {
        return Err(other("the changeset is missing"));
    }
    let opened = Opened {
        open_us: us(open),
        first_us: us(first),
    };
    println!("{}", serde_json::to_string(&opened).map_err(other)?);
    Ok(())
}

/// `__writer <engine> <dir> <chain> <root>`: commits until killed, printing `acked <n>` after
/// each commit returns.
fn killed_writer(args: &[String]) -> Result<()> {
    let engine = engine(arg(args, 1)?)?;
    let (writer_chain, root) = (num(arg(args, 3)?)?, hex_id(arg(args, 4)?)?);
    let db = engine.open(arg(args, 2)?.as_ref())?;
    let mut session = db.session()?;
    let mut out = std::io::stdout().lock();
    let mut guess = None;
    for n in 0.. {
        let cs = changeset(writer_chain, n, Some(root));
        session.commit(&cs, Main::Current { guess }, "w4/writer")?;
        guess = Some(cs.id);
        writeln!(out, "acked {n}")?;
        out.flush()?;
    }
    Ok(())
}

/// `__recover <engine> <dir> <chain> <last acked> <recover chain>`: opens the store, commits
/// one transaction on the last acknowledged changeset, checks that it is there.
fn recover(args: &[String]) -> Result<()> {
    let engine = engine(arg(args, 1)?)?;
    let (writer_chain, last) = (num(arg(args, 3)?)?, num(arg(args, 4)?)?);
    let recover_chain = num(arg(args, 5)?)?;
    let acked = id(writer_chain, last);
    let started = Instant::now();
    let db = engine.open(arg(args, 2)?.as_ref())?;
    let open = started.elapsed();
    let started = Instant::now();
    let mut session = db.session()?;
    let cs = changeset(recover_chain, 0, Some(acked));
    session.commit(&cs, Main::Current { guess: None }, "w4/recover")?;
    let write = started.elapsed();
    let recovered = Recovered {
        open_us: us(open),
        write_us: us(write),
        acked_present: session.get(&acked)?.is_some(),
    };
    println!("{}", serde_json::to_string(&recovered).map_err(other)?);
    Ok(())
}

/// `__sync <engine> <dir> <txns>`: 32 untraced commits, `ready`, a line on stdin (the tracer
/// is attached by then), `<txns>` commits, the store closed, `done`.
fn traced_writer(args: &[String]) -> Result<()> {
    let engine = engine(arg(args, 1)?)?;
    let txns = num(arg(args, 3)?)?;
    let db = engine.create(arg(args, 2)?.as_ref())?;
    let mut session = db.session()?;
    let mut main = None;
    let mut commit = |n: u64| -> Result<()> {
        let cs = changeset(chain::SYNC, n, None);
        session.commit(&cs, Main::Expect(main), "sync")?;
        main = Some(cs.id);
        Ok(())
    };
    for n in 0..32 {
        commit(n)?;
    }
    println!("ready");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    for n in 32..32 + txns {
        commit(n)?;
    }
    drop(session);
    drop(db);
    println!("done");
    Ok(())
}
