//! herdr-pool: your machines as one pool for coding agents and test runs.
//!
//! herdr-core (`hc`) runs the terminals, carries them between machines and
//! routes agents' reports to whoever started them. This adds the decisions hc
//! leaves to you: which machine has room, getting the code there, and
//! bringing the work back.

#[macro_use]
mod error;
mod args;
mod config;
mod discover;
mod fleet;
mod git;
mod hc;
mod jobs;
mod onboard;
mod out;
mod pool;
mod shards;
mod signal;
mod state;
mod sync;
mod trust;
mod ui;

/// A command: its name, its arguments, what it does.
type Command = (&'static str, &'static str, &'static str);

/// The commands, grouped the way work goes.
const GROUPS: &[(&str, &[Command])] = &[
    (
        "Start work",
        &[
            ("run", "<prog> [task]", "start an agent or a command where there is room"),
            ("test", "[-- cmd]", "one test suite split across machines"),
            ("each", "-- <cmd>", "a command on every machine"),
        ],
    ),
    (
        "Follow it",
        &[
            ("ls", "", "jobs on every machine"),
            ("wait", "[job...]", "until jobs stop working"),
            ("diff", "<job>", "its changes so far"),
            ("cat", "<job> <path>", "a file from its work tree"),
        ],
    ),
    (
        "Bring it back",
        &[
            ("pull", "<job>", "its work as branch pool/<job> (--apply: into your tree)"),
            ("rm", "<job...>", "end it and give its tree back"),
            ("gc", "", "clean up finished work"),
        ],
    ),
    (
        "Machines",
        &[
            ("init", "", "set the pool up: find machines, install hc, check them"),
            ("status", "", "every machine: room, CPU, memory"),
            ("add", "[ssh-target]", "add a machine (no target: pick from those found)"),
            ("doctor", "[--fix]", "check every machine, and put right what can be"),
            ("refresh", "", "ask machines' login PATH again, copy hc templates"),
            ("skill", "[install]", "the agent skill: print it, or install it for Claude Code"),
        ],
    ),
];

fn usage() -> String {
    let pretty = ui::pretty();
    let width = GROUPS.iter().flat_map(|(_, cmds)| cmds.iter()).map(|(c, a, _)| c.len() + 1 + a.len()).max().unwrap_or(0);
    let mut out = match pretty {
        true => format!(
            "\n{}  {}\n",
            ui::paint(ui::Tone::Badge, " herdr-pool "),
            ui::dim("your machines as one pool for coding agents and test runs, on herdr-core")
        ),
        false => "herdr-pool: your machines as one pool for coding agents and test runs (on herdr-core)\n".into(),
    };
    for (group, cmds) in GROUPS {
        out += &format!("\n{}\n", if pretty { ui::bold(group) } else { group.to_string() });
        for (cmd, args, what) in *cmds {
            let call = format!("{cmd} {args}");
            let shown = match pretty {
                true => format!("{} {}", ui::accent(cmd), ui::dim(args)),
                false => call.clone(),
            };
            out += &format!("  {shown}{}  {what}\n", " ".repeat(width.saturating_sub(call.len())));
        }
    }
    let foot = "<command> --help: its options · help config: the config file · --skill: what agents read";
    out += &format!("\n{}\n", if pretty { ui::dim(foot) } else { foot.to_string() });
    if !config::path().exists() {
        let new = "New here? herdr-pool init sets the pool up.";
        out += &match pretty {
            true => format!("{} {}\n", ui::accent(ui::g().arrow), ui::bold(new)),
            false => format!("{new}\n"),
        };
    }
    out
}

fn main() {
    signal::guard();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = match dispatch(&argv) {
        Ok(code) => code,
        Err(e) => {
            ui::restore();
            match (e.msg.is_empty(), ui::live()) {
                (true, _) => ui::abandon(&ui::dim("Stopped here")),
                (false, true) => {
                    ui::abandon(&ui::bad("Stopped"));
                    ui::error(&e.msg);
                }
                (false, false) => eprintln!("herdr-pool: {e}"),
            }
            e.code
        }
    };
    std::process::exit(code);
}

fn dispatch(argv: &[String]) -> error::Result<i32> {
    let (cmd, rest) = match argv.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            print!("{}", usage());
            return Ok(0);
        }
    };
    match cmd {
        "init" => onboard::init(rest),
        "status" | "st" => fleet::status(rest),
        "run" => jobs::run(rest),
        "ls" | "list" => jobs::ls(rest),
        "wait" => jobs::wait(rest),
        "diff" => jobs::diff(rest),
        "cat" => jobs::cat(rest),
        "pull" => jobs::pull(rest),
        "rm" => jobs::rm(rest),
        "gc" => jobs::gc(rest),
        "test" => shards::test(rest),
        "each" => fleet::each(rest),
        "add" => onboard::add(rest),
        "refresh" | "setup" => fleet::refresh(rest),
        "doctor" => fleet::doctor(rest),
        "skill" => fleet::skill(rest),
        "--skill" => {
            println!("{}", fleet::skill_body());
            Ok(0)
        }
        "help" if rest.first().map(String::as_str) == Some("config") => {
            println!("{}", config::HELP);
            Ok(0)
        }
        "help" | "--help" | "-h" => {
            print!("{}", usage());
            Ok(0)
        }
        "--version" | "-V" => {
            println!("herdr-pool {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        other => Err(error::Error::usage(format!("unknown command {other:?}; see herdr-pool help"))),
    }
}
