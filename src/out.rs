//! Small text helpers for the plain output agents read: short lines, no
//! decoration, only what the next step needs. (ui.rs dresses it for people.)

/// `1 tree`, `3 trees`.
pub fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// `512 B`, `38 KB`, `4.2 MB`, `1.3 GB`.
pub fn bytes(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.0} KB", n as f64 / 1024.0),
        1_048_576..=1_073_741_823 => format!("{:.1} MB", n as f64 / 1_048_576.0),
        _ => format!("{:.1} GB", n as f64 / 1_073_741_824.0),
    }
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// `42s`, `3m`, `5h`, `2d` since `ms` (epoch milliseconds).
pub fn ago(ms: u64) -> String {
    let now = crate::state::now_ms();
    span(now.saturating_sub(ms))
}

pub fn span(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        3600..=86399 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d", s / 86400),
    }
}

/// Columns aligned with two spaces, no trailing blanks.
pub fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (c, cell) in row.iter().enumerate() {
            line += cell;
            if c + 1 < row.len() {
                line += &" ".repeat(widths[c] - cell.chars().count() + 2);
            }
        }
        out += line.trim_end();
        out.push('\n');
    }
    out
}

/// The last `n` lines that hold something, blank lines at the end dropped.
pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let end = lines.iter().rposition(|l| !l.is_empty()).map_or(0, |i| i + 1);
    lines[end.saturating_sub(n)..end].join("\n")
}

/// At most `n` lines, saying how many were left out.
pub fn head(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= n {
        return text.trim_end().to_string();
    }
    format!("{}\n[{} more lines]", lines[..n].join("\n"), lines.len() - n)
}

/// One line for a shell: words with spaces or shell signs are quoted.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

pub fn quote(a: &str) -> String {
    let plain = !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%{}".contains(c));
    match plain {
        true => a.into(),
        false => format!("'{}'", a.replace('\'', r"'\''")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_align_and_tails_skip_blank_ends() {
        let t = table(&[vec!["a".into(), "bb".into()], vec!["ccc".into(), "d".into()]]);
        assert_eq!(t, "a    bb\nccc  d\n");
        assert_eq!(tail("1\n2\n3\n\n\n", 2), "2\n3");
        assert_eq!(head("1\n2\n3", 2), "1\n2\n[1 more lines]");
    }

    #[test]
    fn shell_words_quote_only_when_needed() {
        let argv: Vec<String> = ["npx", "vitest", "run", "--shard={shard}/{total}", "it's"].map(String::from).to_vec();
        assert_eq!(shell_join(&argv), r"npx vitest run --shard={shard}/{total} 'it'\''s'");
    }

    #[test]
    fn spans_read_naturally() {
        assert_eq!(span(42_000), "42s");
        assert_eq!(span(190_000), "3m10s");
        assert_eq!(span(7_200_000), "2h00m");
    }
}
