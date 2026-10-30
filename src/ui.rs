//! What a person at a terminal sees: colour, glyphs, spinners, live boards
//! and questions. Agents read through pipes, and for them nothing here adds a
//! byte: the pretty layout needs a terminal on stdout, moving parts a terminal
//! on stderr, and questions a terminal on stdin too.
//!
//! Two looks. A command prints its result (a table, a card) on stdout and
//! its steps (`✓ Shipped to studio`) on stderr. A guided flow (`init`) draws
//! a frame down the left edge instead, a block per step, all on stderr.

use crate::error::{Error, Result};
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

struct Caps {
    pretty: bool,
    live: bool,
    ask: bool,
    color: bool,
    unicode: bool,
}

fn caps() -> &'static Caps {
    static CAPS: OnceLock<Caps> = OnceLock::new();
    CAPS.get_or_init(|| {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        // An editor's shell or a CI log says TERM=dumb: it gets plain output.
        let off = var("TERM").is_some_and(|t| t == "dumb") || var("HERDR_POOL_PLAIN").is_some_and(|v| v != "0");
        let pretty = !off && std::io::stdout().is_terminal();
        let live = !off && std::io::stderr().is_terminal();
        let unicode = match var("LC_ALL").or_else(|| var("LC_CTYPE")).or_else(|| var("LANG")) {
            Some(l) => {
                let l = l.to_ascii_lowercase();
                l.contains("utf-8") || l.contains("utf8")
            }
            None => cfg!(target_os = "macos"),
        };
        Caps {
            pretty,
            live,
            ask: live && std::io::stdin().is_terminal(),
            color: (pretty || live) && var("NO_COLOR").is_none(),
            unicode,
        }
    })
}

/// stdout is a terminal a person reads: cards and tables in colour instead
/// of the plain lines agents parse.
pub fn pretty() -> bool {
    caps().pretty
}

/// stderr is a terminal: spinners, steps and live boards go there.
pub fn live() -> bool {
    caps().live
}

/// Someone can answer questions: stdin and stderr are both terminals.
pub fn can_ask() -> bool {
    caps().ask
}

/// Columns on screen; 80 with no terminal to ask.
pub fn columns() -> usize {
    let size = crossterm::terminal::size().ok().map(|(c, _)| usize::from(c)).filter(|c| *c > 0);
    let env = || std::env::var("COLUMNS").ok()?.parse().ok();
    size.or_else(env).unwrap_or(80).clamp(20, 500)
}

fn rows() -> usize {
    crossterm::terminal::size().ok().map(|(_, r)| usize::from(r)).filter(|r| *r > 0).unwrap_or(24)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Accent,
    Good,
    Warn,
    Bad,
    Dim,
    Bold,
    /// A title on a coloured ground.
    Badge,
}

pub fn paint(tone: Tone, text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    if !caps().color || text.is_empty() {
        return text.to_string();
    }
    // Each closes only what it opened, so a colour inside dim text ends well.
    let (on, off) = match tone {
        Tone::Accent => ("36", "39"),
        Tone::Good => ("32", "39"),
        Tone::Warn => ("33", "39"),
        Tone::Bad => ("31", "39"),
        Tone::Dim => ("2", "22"),
        Tone::Bold => ("1", "22"),
        Tone::Badge => ("1;30;46", "0"),
    };
    format!("\x1b[{on}m{text}\x1b[{off}m")
}

pub fn accent(t: impl AsRef<str>) -> String {
    paint(Tone::Accent, t)
}

pub fn good(t: impl AsRef<str>) -> String {
    paint(Tone::Good, t)
}

pub fn warn(t: impl AsRef<str>) -> String {
    paint(Tone::Warn, t)
}

pub fn bad(t: impl AsRef<str>) -> String {
    paint(Tone::Bad, t)
}

pub fn dim(t: impl AsRef<str>) -> String {
    paint(Tone::Dim, t)
}

pub fn bold(t: impl AsRef<str>) -> String {
    paint(Tone::Bold, t)
}

/// How alarming a share (0 to 1) of something is: green, yellow, red.
pub fn level(share: f64) -> Tone {
    match share {
        s if s < 0.6 => Tone::Good,
        s if s < 0.85 => Tone::Warn,
        _ => Tone::Bad,
    }
}

pub struct Glyphs {
    pub ok: &'static str,
    pub bad: &'static str,
    pub warn: &'static str,
    pub dot: &'static str,
    pub ring: &'static str,
    pub arrow: &'static str,
    pub sep: &'static str,
    pub more: &'static str,
    pub pipe: &'static str,
    top: &'static str,
    bottom: &'static str,
    ask: &'static str,
    done: &'static str,
    stop: &'static str,
    on: &'static str,
    off: &'static str,
    yes: &'static str,
    no: &'static str,
    pub spin: &'static [&'static str],
    orbit: &'static [&'static str],
}

const UNICODE: Glyphs = Glyphs {
    ok: "✓",
    bad: "✗",
    warn: "▲",
    dot: "●",
    ring: "○",
    arrow: "→",
    sep: "·",
    more: "…",
    pipe: "│",
    top: "┌",
    bottom: "└",
    ask: "◆",
    done: "◇",
    stop: "■",
    on: "◼",
    off: "◻",
    yes: "●",
    no: "○",
    spin: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    orbit: &["◒", "◐", "◓", "◑"],
};

const ASCII: Glyphs = Glyphs {
    ok: "+",
    bad: "x",
    warn: "!",
    dot: "*",
    ring: "o",
    arrow: "->",
    sep: "-",
    more: "...",
    pipe: "|",
    top: "+",
    bottom: "+",
    ask: "*",
    done: "o",
    stop: "x",
    on: "[x]",
    off: "[ ]",
    yes: "(*)",
    no: "( )",
    spin: &["-", "\\", "|", "/"],
    orbit: &["-", "\\", "|", "/"],
};

pub fn g() -> &'static Glyphs {
    if caps().unicode {
        &UNICODE
    } else {
        &ASCII
    }
}

/// `s` in pieces: escape sequences (no width) and visible characters.
fn pieces(s: &str) -> impl Iterator<Item = (&str, usize)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        if i >= bytes.len() {
            return None;
        }
        let start = i;
        if bytes[i] == 0x1b {
            i += 1;
            match bytes.get(i) {
                // CSI: up to a final byte in @..~
                Some(b'[') => {
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i = (i + 1).min(bytes.len());
                }
                // OSC: up to BEL or ESC \
                Some(b']') => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                Some(_) => i += s[i..].chars().next().map_or(1, char::len_utf8),
                None => {}
            }
            return Some((&s[start..i], 0));
        }
        let c = s[i..].chars().next().expect("inside the string");
        i += c.len_utf8();
        Some((&s[start..i], c.width().unwrap_or(0)))
    })
}

/// Columns `s` takes on screen.
pub fn width(s: &str) -> usize {
    pieces(s).map(|(_, w)| w).sum()
}

/// `s` cut to `max` columns, ending in `…` when anything was cut.
pub fn fit(s: &str, max: usize) -> String {
    // One row: a line break or tab would move the cursor where widths can't see.
    let flat;
    let s = match s.contains(['\n', '\r', '\t']) {
        true => {
            flat = s.replace("\r\n", " ").replace(['\n', '\r', '\t'], " ");
            flat.as_str()
        }
        false => s,
    };
    if width(s) <= max {
        return s.to_string();
    }
    let more = g().more;
    let budget = max.saturating_sub(width(more));
    let (mut out, mut used, mut styled) = (String::new(), 0, false);
    for (piece, w) in pieces(s) {
        if piece.starts_with('\x1b') {
            styled = true;
            out += piece;
            continue;
        }
        if used + w > budget {
            break;
        }
        used += w;
        out += piece;
    }
    out += more;
    if styled {
        out += "\x1b[0m";
    }
    out
}

pub fn pad(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(width(s))))
}

pub fn pad_left(s: &str, w: usize) -> String {
    format!("{}{s}", " ".repeat(w.saturating_sub(width(s))))
}

/// Plain text broken into lines of at most `max` columns, at spaces.
pub fn wrap(text: &str, max: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split(' ') {
            if !line.is_empty() && width(&line) + 1 + width(word) > max {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line += word;
        }
        lines.push(line);
    }
    lines
}

/// Columns lined up by what shows on screen. The `flex` column gives up
/// width, its cells cut with `…`, when the table is wider than the screen.
#[derive(Default)]
pub struct Table {
    pub rows: Vec<Vec<String>>,
    pub right: Vec<usize>,
    pub flex: Option<usize>,
    pub indent: usize,
}

impl Table {
    pub fn render(&self, max: usize) -> Vec<String> {
        let cols = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths: Vec<usize> =
            (0..cols).map(|c| self.rows.iter().filter_map(|r| r.get(c)).map(|s| width(s)).max().unwrap_or(0)).collect();
        let total = self.indent + widths.iter().sum::<usize>() + 2 * cols.saturating_sub(1);
        if let Some(f) = self.flex.filter(|f| *f < cols && total > max) {
            widths[f] = widths[f].saturating_sub(total - max).max(6);
        }
        self.rows
            .iter()
            .map(|row| {
                let mut line = " ".repeat(self.indent);
                for (c, cell) in row.iter().enumerate() {
                    let cell = fit(cell, widths[c]);
                    let last = c + 1 == row.len();
                    line += &match (self.right.contains(&c), last) {
                        (true, _) => pad_left(&cell, widths[c]),
                        (false, true) => cell,
                        (false, false) => pad(&cell, widths[c]),
                    };
                    if !last {
                        line += "  ";
                    }
                }
                line.trim_end().to_string()
            })
            .collect()
    }

    pub fn print(&self) {
        for line in self.render(columns().saturating_sub(1)) {
            println!("{line}");
        }
    }
}

const EIGHTHS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// A meter `cols` wide filled to `share` (0 to 1), to an eighth of a column.
pub fn meter(share: f64, cols: usize, tone: Tone) -> String {
    let share = if share.is_finite() { share.clamp(0.0, 1.0) } else { 0.0 };
    if !caps().unicode {
        let n = (share * cols as f64).round() as usize;
        return format!("{}{}", paint(tone, "#".repeat(n)), dim("-".repeat(cols - n)));
    }
    let eighths = (share * (cols * 8) as f64).round() as usize;
    let (full, part) = (eighths / 8, eighths % 8);
    let mut fill = "█".repeat(full);
    if part > 0 {
        fill.push(EIGHTHS[part - 1]);
    }
    let rest = cols - full - usize::from(part > 0);
    format!("{}{}", paint(tone, fill), dim("░".repeat(rest)))
}

/// `0.4s`, `3.2s`, `42s`, `3m10s`.
pub fn secs(ms: u64) -> String {
    match ms {
        0..=9_999 => format!("{:.1}s", ms as f64 / 1000.0),
        _ => crate::out::span(ms),
    }
}

/// One writer on stderr at a time: a spinner's thread and the rest.
static DRAW: Mutex<()> = Mutex::new(());
/// Inside a guided flow: steps and questions draw the frame.
static FRAMED: AtomicBool = AtomicBool::new(false);

fn framed() -> bool {
    FRAMED.load(Ordering::SeqCst)
}

/// After `restore`, the spinner's and boards' threads draw nothing more.
static SILENT: AtomicBool = AtomicBool::new(false);

/// What the spinner and board threads draw: nothing once we are leaving.
fn draw_live(s: &str) {
    let _held = DRAW.lock().unwrap_or_else(|p| p.into_inner());
    if SILENT.load(Ordering::SeqCst) {
        return;
    }
    let mut e = std::io::stderr().lock();
    let _ = e.write_all(s.as_bytes());
    let _ = e.flush();
}

fn draw(s: &str) {
    let _held = DRAW.lock().unwrap_or_else(|p| p.into_inner());
    let mut e = std::io::stderr().lock();
    let _ = e.write_all(s.as_bytes());
    let _ = e.flush();
}

fn cursor(show: bool) {
    crate::signal::cursor_hidden(!show);
    draw(if show { "\x1b[?25h" } else { "\x1b[?25l" });
}

/// Before leaving early (`process::exit`): the cursor back, colours off.
/// Before leaving early (an error, `process::exit`): spinners and boards
/// stop drawing, their line is cleared and the cursor shown.
pub fn restore() {
    SILENT.store(true, Ordering::SeqCst);
    if live() {
        // Under DRAW: a frame being drawn finishes first, and none follow.
        draw("\r\x1b[2K");
        cursor(true);
    }
}

/// Text without its colours, for where there are none (a log, a pipe).
pub fn strip(s: &str) -> String {
    pieces(s).filter(|(p, _)| !p.starts_with('\x1b')).map(|(p, _)| p).collect()
}

/// The separator between facts: the glyph set's for people, and always `·`
/// in plain output, which programs read whatever the locale.
pub fn sep() -> &'static str {
    match pretty() {
        true => g().sep,
        false => "·",
    }
}

/// How a step ended, as its line: `✓ msg` alone, `◇  msg` in the frame.
fn mark(tone: Tone, msg: &str) -> String {
    let glyph = match (framed(), tone) {
        (true, Tone::Good) => g().done,
        (true, Tone::Warn) | (false, Tone::Warn) => g().warn,
        (true, Tone::Bad) => g().stop,
        (false, Tone::Good) => g().ok,
        (false, Tone::Bad) => g().bad,
        (_, _) => g().dot,
    };
    let gap = if framed() { "  " } else { " " };
    format!("{}{gap}{msg}", paint(tone, glyph))
}

/// Lines under a step: in the frame, behind its edge.
fn body(lines: &[String]) -> String {
    let lead = if framed() { format!("{}  ", dim(g().pipe)) } else { "  ".into() };
    lines.iter().map(|l| format!("{lead}{l}\n")).collect()
}

fn spacer() -> String {
    if framed() {
        format!("{}\n", dim(g().pipe))
    } else {
        String::new()
    }
}

/// A step that has happened, with lines under it. Without a terminal it is
/// plain text on stdout, so a script still learns what was done.
pub fn step(tone: Tone, msg: &str, under: &[String]) {
    if !live() {
        println!("{msg}");
        for l in under {
            println!("  {l}");
        }
        return;
    }
    draw(&format!("{}{}\n{}", spacer(), mark(tone, msg), body(under)));
}

pub fn done(msg: &str) {
    step(Tone::Good, msg, &[]);
}

/// Lines in the frame (or indented) with no step of their own.
pub fn note(lines: &[String]) {
    if live() {
        draw(&body(lines));
    } else {
        for l in lines {
            println!("{l}");
        }
    }
}

/// Opens the frame of a guided flow.
pub fn intro(title: &str, sub: &str) {
    FRAMED.store(true, Ordering::SeqCst);
    if live() {
        draw(&format!("\n{}  {}  {}\n", dim(g().top), paint(Tone::Badge, format!(" {title} ")), dim(sub)));
    }
}

/// Ends a frame left open by an error or a cancelled question.
pub fn abandon(msg: &str) {
    if live() && framed() {
        draw(&format!("{}  {msg}\n\n", dim(g().bottom)));
    }
    FRAMED.store(false, Ordering::SeqCst);
}

pub fn outro(msg: &str) {
    if live() {
        draw(&format!("{}\n{}  {msg}\n\n", dim(g().pipe), dim(g().bottom)));
    } else {
        println!("{msg}");
    }
    FRAMED.store(false, Ordering::SeqCst);
}

/// A step under way: a spinner on stderr until it ends as a line saying how
/// it went. Without a terminal there it shows nothing.
pub struct Spin {
    run: Option<(Arc<SpinState>, JoinHandle<()>)>,
    began: Instant,
    /// It drew a spacer line above itself (in the frame), to take away with it.
    spaced: bool,
}

struct SpinState {
    msg: Mutex<String>,
    stop: AtomicBool,
}

pub fn spin(msg: impl Into<String>) -> Spin {
    let began = Instant::now();
    if !live() {
        return Spin { run: None, began, spaced: false };
    }
    let state = Arc::new(SpinState { msg: Mutex::new(msg.into()), stop: AtomicBool::new(false) });
    let s = state.clone();
    let space = spacer();
    let spaced = !space.is_empty();
    draw(&space);
    cursor(false);
    let handle = std::thread::spawn(move || {
        let (frames, gap) = if framed() { (g().orbit, "  ") } else { (g().spin, " ") };
        let mut n = 0;
        while !s.stop.load(Ordering::SeqCst) {
            let msg = s.msg.lock().map(|m| m.clone()).unwrap_or_default();
            let t = began.elapsed().as_secs();
            let clock = if t >= 2 { format!("  {}", dim(format!("{t}s"))) } else { String::new() };
            let line = format!("{}{gap}{msg}{clock}", accent(frames[n % frames.len()]));
            draw_live(&format!("\r\x1b[2K{}", fit(&line, columns().saturating_sub(1))));
            n += 1;
            for _ in 0..8 {
                if s.stop.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });
    Spin { run: Some((state, handle)), began, spaced }
}

impl Spin {
    pub fn set(&self, msg: impl Into<String>) {
        if let Some((s, _)) = &self.run {
            if let Ok(mut m) = s.msg.lock() {
                *m = msg.into();
            }
        }
    }

    fn end(&mut self, line: Option<String>) {
        let Some((s, handle)) = self.run.take() else { return };
        s.stop.store(true, Ordering::SeqCst);
        let _ = handle.join();
        draw(&match (line, self.spaced) {
            (Some(l), _) => format!("\r\x1b[2K{l}\n"),
            (None, false) => "\r\x1b[2K".into(),
            // Its spacer too: what comes next brings its own.
            (None, true) => "\r\x1b[2K\x1b[1A\x1b[2K".into(),
        });
        cursor(true);
    }

    fn finish(mut self, tone: Tone, msg: &str) {
        if self.run.is_none() {
            // No terminal to draw on: what went wrong is still said, on stderr.
            if tone != Tone::Good && !live() {
                eprintln!("herdr-pool: {}", strip(msg));
            }
            return;
        }
        let ms = self.began.elapsed().as_millis() as u64;
        let took = if ms >= 1000 { format!("  {}", dim(secs(ms))) } else { String::new() };
        self.end(Some(format!("{}{took}", mark(tone, msg))));
    }

    pub fn ok(self, msg: impl AsRef<str>) {
        self.finish(Tone::Good, msg.as_ref());
    }

    pub fn warn(self, msg: impl AsRef<str>) {
        self.finish(Tone::Warn, msg.as_ref());
    }

    pub fn fail(self, msg: impl AsRef<str>) {
        self.finish(Tone::Bad, msg.as_ref());
    }

    /// Gone without a trace (what follows says enough).
    pub fn clear(mut self) {
        self.end(None);
    }
}

impl Drop for Spin {
    fn drop(&mut self) {
        self.end(None);
    }
}

/// Lines redrawn in place on stderr while work goes on (a test run's shards,
/// jobs being waited on), and gone when it ends: the result goes to stdout.
/// `render` gets the spinner's frame to put on lines still at work.
pub struct Board {
    run: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

pub fn board(render: impl Fn(&str) -> Vec<String> + Send + 'static) -> Board {
    if !live() {
        return Board { run: None };
    }
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    cursor(false);
    let handle = std::thread::spawn(move || {
        let (mut drawn, mut n) = (0, 0);
        loop {
            let last = s.load(Ordering::SeqCst);
            let mut out = up(drawn);
            drawn = 0;
            if !last {
                let mut lines = render(g().spin[n % g().spin.len()]);
                // Only what fits: the cursor can't go back up past the top.
                let room = rows().saturating_sub(2).max(3);
                if lines.len() > room {
                    let hidden = lines.len() - room + 1;
                    lines.truncate(room - 1);
                    lines.push(dim(format!("  {} {hidden} more", g().more)));
                }
                for l in &lines {
                    out += &fit(l, columns().saturating_sub(1));
                    out += "\n";
                }
                drawn = lines.len();
            }
            draw_live(&out);
            if last {
                break;
            }
            n += 1;
            for _ in 0..10 {
                if s.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });
    Board { run: Some((stop, handle)) }
}

/// Back up over `n` lines drawn before, clearing them.
fn up(n: usize) -> String {
    match n {
        0 => "\r\x1b[J".into(),
        n => format!("\x1b[{n}F\x1b[J"),
    }
}

impl Board {
    pub fn close(&mut self) {
        if let Some((stop, handle)) = self.run.take() {
            stop.store(true, Ordering::SeqCst);
            let _ = handle.join();
            cursor(true);
        }
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Enter,
    Space,
    Tab,
    Back,
    Delete,
    Esc,
    /// Ctrl-C or Ctrl-D.
    Quit,
    /// Ctrl-U: the line before the cursor.
    Clear,
    /// Ctrl-W: the word before the cursor.
    WordBack,
    Char(char),
    Redraw,
    Other,
}

fn read_key() -> Result<Key> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    loop {
        match event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                return Ok(match k.code {
                    KeyCode::Char('c' | 'd') if ctrl => Key::Quit,
                    KeyCode::Char('u') if ctrl => Key::Clear,
                    KeyCode::Char('w') if ctrl => Key::WordBack,
                    KeyCode::Char('p') if ctrl => Key::Up,
                    KeyCode::Char('n') if ctrl => Key::Down,
                    KeyCode::Char('a') if ctrl => Key::Home,
                    KeyCode::Char('e') if ctrl => Key::End,
                    KeyCode::Char(' ') => Key::Space,
                    KeyCode::Char(c) if !ctrl => Key::Char(c),
                    KeyCode::Up => Key::Up,
                    KeyCode::Down => Key::Down,
                    KeyCode::Left => Key::Left,
                    KeyCode::Right => Key::Right,
                    KeyCode::Home => Key::Home,
                    KeyCode::End => Key::End,
                    KeyCode::Enter => Key::Enter,
                    KeyCode::Tab => Key::Tab,
                    KeyCode::Backspace => Key::Back,
                    KeyCode::Delete => Key::Delete,
                    KeyCode::Esc => Key::Esc,
                    _ => Key::Other,
                });
            }
            Event::Resize(..) => return Ok(Key::Redraw),
            _ => {}
        }
    }
}

/// Raw mode while a question is open; the terminal as it was after, however
/// the question ends.
struct Raw;

impl Raw {
    fn on() -> Result<Raw> {
        crate::signal::raw(true);
        if let Err(e) = crossterm::terminal::enable_raw_mode() {
            crate::signal::raw(false);
            return Err(Error::new(format!("can't read keys: {e}")));
        }
        cursor(false);
        Ok(Raw)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        crate::signal::raw(false);
        cursor(true);
    }
}

/// Redraws a question in place. Raw mode: lines end in `\r\n`.
#[derive(Default)]
struct Canvas {
    drawn: usize,
}

impl Canvas {
    fn paint(&mut self, lines: &[String]) {
        let mut out = up(self.drawn);
        for l in lines {
            out += &fit(l, columns().saturating_sub(1));
            out += "\r\n";
        }
        self.drawn = lines.len();
        draw(&out);
    }
}

#[derive(Debug, PartialEq)]
enum Then {
    Stay,
    Done,
    Cancel,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Asking,
    Answered,
    Cancelled,
}

/// A question's first line, and the edge its other lines hang from.
fn head(phase: Phase, question: &str) -> (String, String) {
    let glyph = match phase {
        Phase::Asking => accent(g().ask),
        Phase::Answered => good(g().done),
        Phase::Cancelled => bad(g().stop),
    };
    let edge = match phase {
        Phase::Asking => accent(g().pipe),
        _ => dim(g().pipe),
    };
    (format!("{glyph}  {}", bold(question)), edge)
}

/// A question once answered (or not): in the frame, the answer under it on
/// the rail; outside, one line like a step's.
fn settled(phase: Phase, question: &str, answer: &str) -> Vec<String> {
    if framed() {
        let (top, edge) = head(phase, question);
        return vec![top, format!("{edge}  {}", dim(answer))];
    }
    let glyph = match phase {
        Phase::Cancelled => bad(g().bad),
        _ => good(g().ok),
    };
    vec![format!("{glyph} {question}  {}", dim(answer))]
}

fn ask(question: &str, mut key: impl FnMut(&Key) -> Then, mut lines: impl FnMut(Phase) -> Vec<String>) -> Result<()> {
    if !can_ask() {
        return Err(Error::usage(format!("{question}: no terminal to ask in (see --help for flags)")));
    }
    let _raw = Raw::on()?;
    let mut canvas = Canvas::default();
    if framed() {
        draw(&format!("{}\r\n", dim(g().pipe)));
    }
    loop {
        canvas.paint(&lines(Phase::Asking));
        match key(&read_key()?) {
            Then::Stay => {}
            Then::Done => {
                canvas.paint(&lines(Phase::Answered));
                return Ok(());
            }
            Then::Cancel => {
                canvas.paint(&lines(Phase::Cancelled));
                return Err(Error::cancelled());
            }
        }
    }
}

/// A yes or no; `about`, when not empty, is a line on what it means.
pub fn confirm(question: &str, about: &str, default: bool) -> Result<bool> {
    let yes = std::cell::Cell::new(default);
    let key = |k: &Key| match k {
        Key::Left | Key::Right | Key::Tab | Key::Up | Key::Down | Key::Char('h' | 'l') => {
            yes.set(!yes.get());
            Then::Stay
        }
        Key::Char('y' | 'Y') => {
            yes.set(true);
            Then::Done
        }
        Key::Char('n' | 'N') => {
            yes.set(false);
            Then::Done
        }
        Key::Enter => Then::Done,
        Key::Esc | Key::Quit => Then::Cancel,
        _ => Then::Stay,
    };
    ask(question, key, |phase| {
        let y = yes.get();
        match phase {
            Phase::Answered => return settled(phase, question, if y { "Yes" } else { "No" }),
            Phase::Cancelled => return settled(phase, question, "cancelled"),
            Phase::Asking => {}
        }
        let (top, edge) = head(phase, question);
        let mut lines = vec![top];
        if !about.is_empty() {
            lines.push(format!("{edge}  {}", dim(about)));
        }
        let opt = |on: bool, label: &str| match on {
            true => format!("{} {label}", good(g().yes)),
            false => dim(format!("{} {label}", g().no)),
        };
        lines.push(format!("{edge}  {} {} {}", opt(y, "Yes"), dim("/"), opt(!y, "No")));
        lines.push(accent(g().bottom));
        lines
    })?;
    Ok(yes.get())
}

/// One of a list, or several (`multi`): what is shown for each, and why it
/// can't be picked when it can't.
#[derive(Clone, Default)]
pub struct Item {
    pub label: String,
    pub hint: String,
    pub off: Option<String>,
}

struct Pick {
    at: usize,
    on: Vec<bool>,
    off: Vec<bool>,
    multi: bool,
    /// Enter with nothing picked asks once more: space picks, enter skips.
    warned: bool,
}

impl Pick {
    fn new(items: &[Item], on: &[bool], multi: bool) -> Pick {
        let off: Vec<bool> = items.iter().map(|i| i.off.is_some()).collect();
        let on = (0..items.len()).map(|i| on.get(i).copied().unwrap_or(false) && !off[i]).collect();
        let at = (0..items.len()).find(|i| !off[*i]).unwrap_or(0);
        Pick { at, on, off, multi, warned: false }
    }

    fn key(&mut self, k: &Key) -> Then {
        match k {
            Key::Up | Key::Char('k') => self.go(-1),
            Key::Down | Key::Char('j') | Key::Tab => self.go(1),
            Key::Space if self.multi => {
                if !self.off[self.at] {
                    self.on[self.at] = !self.on[self.at];
                }
            }
            Key::Char('a') if self.multi => {
                let all = (0..self.on.len()).filter(|i| !self.off[*i]).all(|i| self.on[i]);
                for i in (0..self.on.len()).filter(|i| !self.off[*i]) {
                    self.on[i] = !all;
                }
            }
            Key::Enter if self.multi && !self.on.contains(&true) && !self.warned => self.warned = true,
            Key::Enter if !self.multi && self.off.get(self.at) == Some(&true) => {}
            Key::Enter => return Then::Done,
            Key::Esc | Key::Quit => return Then::Cancel,
            _ => {}
        }
        if !matches!(k, Key::Enter) {
            self.warned = false;
        }
        Then::Stay
    }

    fn go(&mut self, step: isize) {
        let n = self.on.len() as isize;
        for d in 1..=n {
            let i = (self.at as isize + step * d).rem_euclid(n.max(1)) as usize;
            if !self.off[i] {
                self.at = i;
                return;
            }
        }
    }
}

fn list(question: &str, items: &[Item], on: &[bool], multi: bool) -> Result<Pick> {
    if items.is_empty() {
        return Err(Error::new(format!("{question}: nothing to choose from")));
    }
    let pick = std::cell::RefCell::new(Pick::new(items, on, multi));
    let label_w = items.iter().map(|i| width(&i.label)).max().unwrap_or(0);
    ask(question, |k| pick.borrow_mut().key(k), |phase| {
        let p = pick.borrow();
        let (top, edge) = head(phase, question);
        match phase {
            Phase::Asking => {
                let mut lines = vec![top];
                // A window around the cursor when the list is taller than the screen.
                let room = rows().saturating_sub(6).max(3);
                let first = p.at.saturating_sub(room / 2).min(items.len().saturating_sub(room));
                let last = (first + room).min(items.len());
                if first > 0 {
                    lines.push(format!("{edge}  {}", dim(format!("  ↑ {first} more"))));
                }
                for (i, item) in items.iter().enumerate().take(last).skip(first) {
                    let here = i == p.at;
                    let mark = match (multi, p.on[i], item.off.is_some()) {
                        (_, _, true) => dim(" "),
                        (true, true, _) => good(g().on),
                        (true, false, _) if here => accent(g().off),
                        (true, false, _) => dim(g().off),
                        (false, _, _) if here => accent(g().yes),
                        (false, _, _) => dim(g().no),
                    };
                    let label = pad(&item.label, label_w);
                    let label = match (here, item.off.is_some()) {
                        (_, true) => dim(label),
                        (true, _) => bold(label),
                        _ => label,
                    };
                    let hint = match &item.off {
                        Some(why) => dim(why),
                        None => dim(&item.hint),
                    };
                    lines.push(format!("{edge}  {mark} {label}  {hint}"));
                }
                if last < items.len() {
                    lines.push(format!("{edge}  {}", dim(format!("  ↓ {} more", items.len() - last))));
                }
                let help = match (multi, p.warned) {
                    (true, true) => warn("nothing picked: space picks one, enter again goes on without"),
                    (true, false) => dim("↑↓ move · space pick · a all · enter go on"),
                    (false, _) => dim("↑↓ move · enter choose"),
                };
                lines.push(format!("{}  {help}", accent(g().bottom)));
                lines
            }
            Phase::Answered => {
                let picked: Vec<&str> = (0..items.len())
                    .filter(|i| if multi { p.on[*i] } else { *i == p.at })
                    .map(|i| items[i].label.as_str())
                    .collect();
                let answer = if picked.is_empty() { "none".to_string() } else { picked.join(", ") };
                settled(phase, question, &answer)
            }
            Phase::Cancelled => settled(phase, question, "cancelled"),
        }
    })?;
    Ok(pick.into_inner())
}

/// One of `items`.
pub fn select(question: &str, items: &[Item]) -> Result<usize> {
    Ok(list(question, items, &[], false)?.at)
}

/// Any of `items`, `on` picked to start with.
pub fn pick(question: &str, items: &[Item], on: &[bool]) -> Result<Vec<usize>> {
    let p = list(question, items, on, true)?;
    Ok((0..items.len()).filter(|i| p.on[*i]).collect())
}

/// A line of text being typed.
#[derive(Default)]
struct Edit {
    text: Vec<char>,
    at: usize,
}

impl Edit {
    fn key(&mut self, k: &Key) -> Then {
        match k {
            Key::Char(c) => {
                self.text.insert(self.at, *c);
                self.at += 1;
            }
            Key::Space => {
                self.text.insert(self.at, ' ');
                self.at += 1;
            }
            Key::Back if self.at > 0 => {
                self.at -= 1;
                self.text.remove(self.at);
            }
            Key::Delete if self.at < self.text.len() => {
                self.text.remove(self.at);
            }
            Key::Left => self.at = self.at.saturating_sub(1),
            Key::Right => self.at = (self.at + 1).min(self.text.len()),
            Key::Home => self.at = 0,
            Key::End => self.at = self.text.len(),
            Key::Clear => {
                self.text.drain(..self.at);
                self.at = 0;
            }
            Key::WordBack => {
                let mut from = self.at;
                while from > 0 && self.text[from - 1] == ' ' {
                    from -= 1;
                }
                while from > 0 && self.text[from - 1] != ' ' {
                    from -= 1;
                }
                self.text.drain(from..self.at);
                self.at = from;
            }
            Key::Enter => return Then::Done,
            Key::Esc | Key::Quit => return Then::Cancel,
            _ => {}
        }
        Then::Stay
    }

    fn value(&self) -> String {
        self.text.iter().collect()
    }
}

/// A line of text. `initial` is there to keep or change; `placeholder`
/// shows (dim) while it is empty; `check` refuses an answer with a reason.
pub fn input(question: &str, initial: &str, placeholder: &str, check: impl Fn(&str) -> std::result::Result<(), String>) -> Result<String> {
    let edit = std::cell::RefCell::new(Edit { text: initial.chars().collect(), at: initial.chars().count() });
    let problem = std::cell::RefCell::new(None::<String>);
    let key = |k: &Key| {
        let then = edit.borrow_mut().key(k);
        if then == Then::Done {
            let value = edit.borrow().value();
            if let Err(why) = check(value.trim()) {
                *problem.borrow_mut() = Some(why);
                return Then::Stay;
            }
        }
        if then == Then::Stay {
            *problem.borrow_mut() = None;
        }
        then
    };
    ask(question, key, |phase| {
        let e = edit.borrow();
        let (top, edge) = head(phase, question);
        match phase {
            Phase::Asking => {
                // A block cursor of our own: the terminal's is hidden while we draw.
                let caret = |c: char| format!("\x1b[7m{c}\x1b[27m");
                let shown = match e.text.is_empty() {
                    true => match placeholder.chars().next() {
                        Some(first) => format!("{}{}", caret(first), dim(placeholder.chars().skip(1).collect::<String>())),
                        None => caret(' '),
                    },
                    false => {
                        let before: String = e.text[..e.at].iter().collect();
                        let after: String = e.text[e.at..].iter().skip(1).collect();
                        format!("{before}{}{after}", caret(e.text.get(e.at).copied().unwrap_or(' ')))
                    }
                };
                let foot = match &*problem.borrow() {
                    Some(why) => warn(why),
                    None => dim("enter to go on"),
                };
                vec![top, format!("{edge}  {shown}"), format!("{}  {foot}", accent(g().bottom))]
            }
            Phase::Answered => settled(phase, question, e.value().trim()),
            Phase::Cancelled => settled(phase, question, "cancelled"),
        }
    })?;
    let value = edit.borrow().value();
    Ok(value.trim().to_string())
}

/// An error as a person reads it: `✗` and the message, wrapped.
pub fn error(msg: &str) {
    let lines = wrap(msg, columns().saturating_sub(4).max(30));
    let mut out = format!("{} {}\n", bad(g().bad), lines.first().map(String::as_str).unwrap_or_default());
    for l in lines.iter().skip(1) {
        out += &format!("  {l}\n");
    }
    draw(&out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_text_on_one_row() {
        assert_eq!(fit("cd x\nmake all", 40), "cd x make all");
        assert_eq!(fit("a\tb\r\nc", 40), "a b c");
        assert!(!fit("one\ntwo three four five", 8).contains('\n'));
    }

    #[test]
    fn width_skips_escapes_and_counts_wide_characters() {
        assert_eq!(width("\x1b[32m✓\x1b[39m ok"), 4);
        assert_eq!(width("\x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\"), 4);
        assert_eq!(width("日本"), 4);
        assert_eq!(width(""), 0);
    }

    #[test]
    fn fit_cuts_what_shows_and_keeps_escapes_closed() {
        assert_eq!(fit("short", 10), "short");
        let cut = fit("abcdefghij", 6);
        assert_eq!(width(&cut), 6, "{cut}");
        let styled = fit("\x1b[31mabcdefghij\x1b[39m", 5);
        assert!(styled.starts_with("\x1b[31m") && styled.ends_with("\x1b[0m"), "{styled:?}");
        assert!(width(&styled) <= 5);
    }

    #[test]
    fn tables_align_by_screen_width_and_shrink_the_flex_column() {
        let t = Table {
            rows: vec![vec!["✓".into(), "a".into(), "long text here".into()], vec!["✗".into(), "bbb".into(), "x".into()]],
            flex: Some(2),
            ..Table::default()
        };
        assert_eq!(t.render(80), vec!["✓  a    long text here", "✗  bbb  x"]);
        let narrow = t.render(16);
        assert!(narrow.iter().all(|l| width(l) <= 16), "{narrow:?}");
        let right = Table { rows: vec![vec!["a".into(), "5".into()], vec!["b".into(), "10".into()]], right: vec![1], ..Table::default() };
        assert_eq!(right.render(80), vec!["a   5", "b  10"]);
    }

    #[test]
    fn wrapping_breaks_at_spaces() {
        assert_eq!(wrap("one two three four", 9), vec!["one two", "three", "four"]);
        assert_eq!(wrap("a\nb", 9), vec!["a", "b"]);
    }

    #[test]
    fn meters_fill_to_their_share() {
        let full = meter(1.0, 4, Tone::Good);
        assert_eq!(width(&full), 4);
        assert_eq!(width(&meter(0.33, 10, Tone::Good)), 10);
        assert_eq!(width(&meter(f64::NAN, 3, Tone::Good)), 3);
    }

    fn items(n: usize, off: &[usize]) -> Vec<Item> {
        (0..n).map(|i| Item { label: format!("m{i}"), off: off.contains(&i).then(|| "offline".into()), ..Item::default() }).collect()
    }

    #[test]
    fn picking_skips_what_cant_be_picked() {
        let list = items(4, &[1]);
        let mut p = Pick::new(&list, &[true, true], true);
        assert_eq!(p.on, vec![true, false, false, false], "an item that can't be picked starts unpicked");
        p.key(&Key::Down);
        assert_eq!(p.at, 2, "down jumps over it");
        p.key(&Key::Up);
        p.key(&Key::Up);
        assert_eq!(p.at, 3, "and up wraps around");
        p.key(&Key::Space);
        assert_eq!(p.on, vec![true, false, false, true]);
        p.key(&Key::Char('a'));
        assert_eq!(p.on, vec![true, false, true, true], "a picks every one that can be");
        p.key(&Key::Char('a'));
        assert_eq!(p.on, vec![false, false, false, false], "and again none");
        assert_eq!(p.key(&Key::Enter), Then::Stay, "enter with nothing picked asks once more");
        assert_eq!(p.key(&Key::Enter), Then::Done);
        assert_eq!(p.key(&Key::Esc), Then::Cancel);
        assert_eq!(p.key(&Key::Quit), Then::Cancel);
    }

    #[test]
    fn typing_edits_the_line() {
        let mut e = Edit::default();
        for c in "studo".chars() {
            e.key(&Key::Char(c));
        }
        e.key(&Key::Left);
        e.key(&Key::Char('i'));
        assert_eq!(e.value(), "studio");
        e.key(&Key::End);
        e.key(&Key::Space);
        e.key(&Key::Char('x'));
        e.key(&Key::WordBack);
        assert_eq!(e.value(), "studio ");
        e.key(&Key::Back);
        e.key(&Key::Home);
        e.key(&Key::Delete);
        assert_eq!(e.value(), "tudio");
        e.key(&Key::End);
        e.key(&Key::Clear);
        assert_eq!(e.value(), "");
        assert_eq!(e.key(&Key::Enter), Then::Done);
    }
}
