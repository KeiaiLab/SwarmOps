//! `docs/reference/cli.md`: the index of the commands, by group, with the global options every
//! command takes. Each command's own page is in `commands/` ([`super::command_page`]).

use std::fmt::Write as _;

use clap::Arg;

use super::command_page::{DIR, Group, file_name, group_of};
use super::{
    BIN, GENERATED, Leaf, about_short, all_leaves, cell, grammar, long_helps, options_table,
    sentence, trim_end,
};

/// Renders `cli.md`.
pub(super) fn render() -> String {
    let root = grammar();
    let leaves = all_leaves();
    let mut out = String::new();

    // 1. Title and how to read the page.
    let _ = writeln!(out, "{GENERATED}\n");
    let _ = writeln!(out, "# CLI reference\n");
    let _ = writeln!(
        out,
        "Every command of `{BIN}` and `so`, its short form: the same program by a shorter name. \
         This page and the command pages are generated from the command line's own definition, \
         so they say what `{BIN} <command> --help` says. `-h` prints the short help and \
         `--help` the long help that the pages give.\n"
    );
    let _ = writeln!(
        out,
        "Every command but `mcp` prints one JSON envelope on stdout; its `op` names the \
         operation the command runs (`cat` runs `read_file`). [Errors](errors.md) lists every \
         error code an envelope can carry, with its exit code.\n"
    );
    let _ = writeln!(
        out,
        "Each command's page gives its syntax, its arguments, an example recorded by the tests, \
         what it returns and the errors it fails with. Its `yaml` metadata block says the \
         same for programs: `group`, `mutates` (it can change what the store holds), `identity_required` \
         (it refuses to run without `--as` or `SWARMOPS_AGENT`), `mcp_tool`, `envelope_op`, \
         `exit_codes`, `errors` and `syntax`.\n"
    );

    // 2. The flags every command takes.
    let _ = writeln!(out, "## Global options\n");
    let _ = writeln!(
        out,
        "Every command takes these, before or after its subcommand.\n"
    );
    let globals: Vec<&Arg> = root
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .collect();
    options_table(&mut out, &globals);
    long_helps(&mut out, &globals);
    let _ = writeln!(
        out,
        "`-h` prints a command's short help and `--help` its long help, as plain text, not an \
         envelope. `{BIN} --version` prints the version.\n"
    );

    // 3. One section per group; a parent command's subcommands under its own heading.
    for group in Group::ALL {
        group_section(&mut out, &root, &leaves, group);
    }
    trim_end(out)
}

/// One group: its top-level commands, then each parent (`blob`, `ref`, …) with its own.
fn group_section(out: &mut String, root: &clap::Command, leaves: &[Leaf], group: Group) {
    let _ = writeln!(out, "## {}\n", group.title());
    let members: Vec<&Leaf> = leaves
        .iter()
        .filter(|leaf| group_of(&leaf.path.join(" ")) == group)
        .collect();

    // 1. The commands that stand alone.
    let alone: Vec<&Leaf> = members
        .iter()
        .copied()
        .filter(|leaf| leaf.path.len() == 1)
        .collect();
    for leaf in &alone {
        command_line(out, leaf);
    }
    if !alone.is_empty() {
        out.push('\n');
    }

    // 2. Each parent, in the order its first subcommand comes.
    let mut parents: Vec<&str> = Vec::new();
    for leaf in members.iter().filter(|leaf| leaf.path.len() > 1) {
        if !parents.contains(&leaf.path[0].as_str()) {
            parents.push(&leaf.path[0]);
        }
    }
    for parent in parents {
        let about = root
            .find_subcommand(parent)
            .map(about_short)
            .unwrap_or_default();
        let _ = writeln!(out, "### {BIN} {parent}\n");
        let _ = writeln!(out, "{}\n", cell(&sentence(&about)));
        for leaf in members.iter().filter(|leaf| leaf.path[0] == parent) {
            command_line(out, leaf);
        }
        out.push('\n');
    }
}

/// A command's line in the index: a link to its page, and its one-line help.
fn command_line(out: &mut String, leaf: &Leaf) {
    let _ = writeln!(
        out,
        "- [`{BIN} {}`]({DIR}/{}): {}",
        leaf.path.join(" "),
        file_name(&leaf.path),
        cell(&sentence(&about_short(&leaf.cmd)))
    );
}
