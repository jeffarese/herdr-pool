//! Flags and options by hand: a dozen subcommands don't need a parser crate.
//! `--name value` and `--name=value` both work; `--` ends options.

use crate::error::{Error, Result};

pub struct Args {
    flags: Vec<String>,
    values: Vec<(String, String)>,
    pub rest: Vec<String>,
}

/// `flags` take no value (but may carry one as `--flag=value`), `options`
/// need one. With `stop`, the first positional and everything after it is
/// `rest` untouched: the program and task of `run` keep their own flags.
pub fn parse(args: &[String], flags: &[&str], options: &[&str], stop: bool) -> Result<Args> {
    let mut out = Args { flags: Vec::new(), values: Vec::new(), rest: Vec::new() };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" {
            out.rest.extend(args[i..].iter().cloned());
            break;
        }
        let Some(long) = arg.strip_prefix("--").or_else(|| (arg == "-h").then_some("help")) else {
            out.rest.push(arg.clone());
            if stop {
                out.rest.extend(args[i..].iter().cloned());
                break;
            }
            continue;
        };
        let (name, inline) = match long.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (long, None),
        };
        if name == "help" || flags.contains(&name) {
            out.flags.push(name.into());
            if let Some(v) = inline {
                out.values.push((name.into(), v));
            }
        } else if options.contains(&name) {
            let value = match inline {
                Some(v) => v,
                None if i < args.len() => {
                    i += 1;
                    args[i - 1].clone()
                }
                None => return Err(Error::usage(format!("--{name} needs a value"))),
            };
            out.values.push((name.into(), value));
        } else {
            let mut known: Vec<String> = flags.iter().chain(options).map(|f| format!("--{f}")).collect();
            known.sort();
            return Err(Error::usage(format!("unknown option --{name} (this command takes {})", known.join(" "))));
        }
    }
    Ok(out)
}

impl Args {
    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// Every value given, commas splitting too: `--on a,b --on c`.
    pub fn list(&self, name: &str) -> Vec<String> {
        let all = self.values.iter().filter(|(n, _)| n == name);
        all.flat_map(|(_, v)| v.split(',')).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
    }

    pub fn num<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>> {
        match self.value(name) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| Error::usage(format!("--{name}: not a number: {v}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn run_stops_at_the_program() {
        let a = parse(&s(&["--on", "studio", "claude", "--model", "x", "fix it"]), &["here"], &["on"], true).unwrap();
        assert_eq!(a.value("on"), Some("studio"));
        assert_eq!(a.rest, s(&["claude", "--model", "x", "fix it"]));
    }

    #[test]
    fn options_may_follow_positionals_elsewhere() {
        let a = parse(&s(&["fix", "--apply", "--branch=b", "--on", "a,b"]), &["apply"], &["branch", "on"], false).unwrap();
        assert!(a.flag("apply"));
        assert_eq!(a.value("branch"), Some("b"));
        assert_eq!(a.list("on"), s(&["a", "b"]));
        assert_eq!(a.rest, s(&["fix"]));
    }

    #[test]
    fn flags_may_carry_a_value_and_typos_fail() {
        let a = parse(&s(&["--wait-slot=30", "--", "--not-mine"]), &["wait-slot"], &[], true).unwrap();
        assert!(a.flag("wait-slot"));
        assert_eq!(a.num::<f64>("wait-slot").unwrap(), Some(30.0));
        assert_eq!(a.rest, s(&["--not-mine"]));
        assert!(parse(&s(&["--nope"]), &[], &[], true).is_err());
        assert!(parse(&s(&["--on"]), &[], &["on"], true).is_err());
    }
}
