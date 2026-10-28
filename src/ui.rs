//! Terminal rendering: a live dashboard on a TTY, plain status lines otherwise.

use crate::agent::clip;
use crate::run::Observer;
use crate::state::{fmt_cost, fmt_dur, now, Run, RunState, Status, TaskState};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};

pub fn color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn paint(on: bool, code: &str, s: &str) -> String {
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn term_size() -> (usize, usize) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    (if ok && ws.ws_col > 20 { ws.ws_col as usize } else { 100 }, if ok && ws.ws_row > 4 { ws.ws_row as usize } else { 40 })
}

pub fn term_width() -> usize {
    term_size().0
}

/// The dashboard redraws in place with cursor-up, which breaks once it is taller than the
/// terminal. Then collapse the quiet rows: finished tasks become one line, queued ones another,
/// and everything active or needing attention stays visible.
/// Cut a line to `width` visible characters, ignoring ANSI escape sequences, so a redraw
/// never wraps (a wrapped line breaks the cursor-up arithmetic of the live dashboard).
pub fn clip_visible(line: &str, width: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let (mut seen, mut in_esc, mut cut) = (0usize, false, false);
    for ch in line.chars() {
        if in_esc {
            out.push(ch);
            if ch.is_ascii_alphabetic() {
                in_esc = false;
            }
            continue;
        }
        if ch == '\x1b' {
            in_esc = true;
            out.push(ch);
            continue;
        }
        if seen >= width {
            cut = true;
            continue;
        }
        out.push(ch);
        seen += 1;
    }
    if cut && line.contains('\x1b') {
        out.push_str("\x1b[0m");
    }
    out
}

pub fn fit_height(s: &RunState, mut lines: Vec<String>, rows: usize, c: bool) -> Vec<String> {
    if lines.len() < rows {
        return lines;
    }
    let keep = |t: &TaskState| t.status.is_active() || matches!(t.status, Status::Failed | Status::Blocked | Status::Stopped);
    let head = lines.remove(0);
    let mut out = vec![head];
    let (mut done, mut queued, mut other) = (0, 0, 0);
    for (t, line) in s.tasks.iter().zip(lines.iter()) {
        match t.status {
            _ if keep(t) => out.push(line.clone()),
            Status::Done => done += 1,
            Status::Queued => queued += 1,
            _ => other += 1,
        }
    }
    let mut folded = Vec::new();
    if done > 0 {
        folded.push(format!("{done} done"));
    }
    if queued > 0 {
        folded.push(format!("{queued} queued"));
    }
    if other > 0 {
        folded.push(format!("{other} skipped"));
    }
    if !folded.is_empty() {
        out.push(paint(c, "2", &format!(" … {} (musher status shows all)", folded.join(", "))));
    }
    out.truncate(rows.saturating_sub(1).max(2));
    out
}

const SPIN: [&str; 4] = ["◐", "◓", "◑", "◒"];

fn glyph(s: Status, frame: usize) -> (&'static str, &'static str) {
    match s {
        Status::Done => ("✓", "32"),
        Status::Failed => ("✗", "31"),
        Status::Blocked => ("!", "33;1"),
        Status::Skipped => ("–", "2"),
        Status::Stopped => ("■", "2"),
        Status::Queued => ("◌", "2"),
        Status::Setup => (SPIN[frame % 4], "36"),
        Status::Working => (SPIN[frame % 4], "33"),
        Status::Checking => (SPIN[frame % 4], "36"),
        Status::Fixing => (SPIN[frame % 4], "35"),
    }
}

fn elapsed(t: &TaskState) -> u64 {
    if t.started == 0 {
        0
    } else if t.ended >= t.started {
        t.ended - t.started
    } else {
        now().saturating_sub(t.started)
    }
}

/// Render the whole run as lines (no trailing newline per line).
pub fn render(s: &RunState, frame: usize, width: usize, c: bool) -> Vec<String> {
    let done = s.tasks.iter().filter(|t| t.status == Status::Done).count();
    let active = s.tasks.iter().filter(|t| t.status.is_active()).count();
    let bad = s.tasks.iter().filter(|t| matches!(t.status, Status::Failed | Status::Blocked)).count();
    let end = if s.ended > 0 { s.ended } else { now() };
    let mut head = format!("{} run {}  {}/{} done", paint(c, "1", "musher"), s.id, done, s.tasks.len());
    if active > 0 {
        head += &format!("  {} active", active);
    }
    if bad > 0 {
        head += &format!("  {}", paint(c, "31", &format!("{bad} need attention")));
    }
    let cost = fmt_cost(s.cost());
    head += &format!("  {}  {}", if cost.is_empty() { "$0" } else { &cost }, fmt_dur(end.saturating_sub(s.started)));
    if s.finished {
        head += "  (finished)";
    }
    // Name column: up to 24 chars, less on a narrow terminal so the fixed columns still fit.
    let idw = s.tasks.iter().map(|t| t.id.chars().count()).max().unwrap_or(4).clamp(4, 24).min(width.saturating_sub(34).max(6));
    let mut out = vec![head];
    for t in &s.tasks {
        let (g, col) = glyph(t.status, frame);
        let money = fmt_cost(t.cost_usd);
        let detail = if t.status.is_active() && !t.activity.is_empty() {
            t.activity.clone()
        } else if t.status == Status::Queued && !t.after.is_empty() {
            format!("after {}", t.after.join(", "))
        } else if t.status == Status::Done {
            let mut d = format!("{} files", t.files_changed);
            if t.check == Some(true) {
                d += "  check ✓";
            }
            if t.attempts > 1 {
                d += &format!("  {} attempts", t.attempts);
            }
            if !t.note.is_empty() {
                d += &format!("  {}", t.note);
            }
            d
        } else {
            t.note.clone()
        };
        let left = format!(" {} {:<idw$}  {:<8} {:>6} {:>6}  ", paint(c, col, g), clip(&t.id, idw), t.status.as_str(), fmt_dur(elapsed(t)), money);
        let visible_left = 1 + 1 + 1 + idw + 2 + 8 + 1 + 6 + 1 + 6 + 2;
        let room = width.saturating_sub(visible_left + 1).max(10);
        let detail = clip(&detail, room);
        let detail = if matches!(t.status, Status::Failed | Status::Blocked) { paint(c, col, &detail) } else { paint(c, "2", &detail) };
        out.push(format!("{left}{detail}"));
    }
    out
}

/// Live, in-place dashboard.
pub struct Live {
    drawn: usize,
    frame: usize,
    hint: bool,
}

impl Live {
    pub fn new() -> Live {
        Live { drawn: 0, frame: 0, hint: true }
    }

    pub fn draw(&mut self, s: &RunState) {
        let c = color();
        let (w, h) = term_size();
        let mut lines = fit_height(s, render(s, self.frame, w, c), h.saturating_sub(if self.hint && !s.finished { 1 } else { 0 }), c);
        if self.hint && !s.finished {
            lines.push(paint(c, "2", " Ctrl-C stops (resume later) · other terminal: musher watch · musher logs <task> -f"));
        }
        // Never let a line wrap: the terminal would add rows the cursor-up below does not know about.
        let lines: Vec<String> = lines.iter().map(|l| clip_visible(l, w.saturating_sub(1))).collect();
        self.frame += 1;
        let mut buf = String::new();
        if self.drawn > 0 {
            buf += &format!("\x1b[{}A", self.drawn);
        }
        for l in &lines {
            buf += "\r\x1b[2K";
            buf += l;
            buf += "\n";
        }
        // Clear leftovers if the dashboard shrank.
        for _ in lines.len()..self.drawn {
            buf += "\r\x1b[2K\n";
        }
        if self.drawn > lines.len() {
            buf += &format!("\x1b[{}A", self.drawn - lines.len());
        }
        self.drawn = lines.len();
        let mut so = std::io::stdout().lock();
        let _ = so.write_all(buf.as_bytes());
        let _ = so.flush();
    }
}

impl Observer for Live {
    fn tick(&mut self, run: &Run) {
        self.draw(&run.snapshot());
    }
    fn finish(&mut self, run: &Run) {
        self.hint = false;
        let s = run.snapshot();
        self.draw(&s);
        summary_footer(&s);
    }
}

/// One line per state change; for pipes, CI and background runs.
pub struct Plain {
    seen: HashMap<String, Status>,
}

impl Plain {
    pub fn new() -> Plain {
        Plain { seen: HashMap::new() }
    }
}

impl Observer for Plain {
    fn tick(&mut self, run: &Run) {
        let s = run.snapshot();
        for t in &s.tasks {
            if self.seen.get(&t.id) != Some(&t.status) {
                if t.status != Status::Queued || self.seen.contains_key(&t.id) {
                    let note = if t.note.is_empty() { String::new() } else { format!("  {}", t.note) };
                    println!("[{}] {:<10} {}{}", fmt_dur(now().saturating_sub(s.started)), t.id, t.status.as_str(), note);
                }
                self.seen.insert(t.id.clone(), t.status);
            }
        }
    }
    fn finish(&mut self, run: &Run) {
        self.tick(run);
        summary_footer(&run.snapshot());
    }
}

fn summary_footer(s: &RunState) {
    let c = color();
    let all_done = s.tasks.iter().all(|t| t.status == Status::Done);
    let attention: Vec<&TaskState> = s.tasks.iter().filter(|t| matches!(t.status, Status::Failed | Status::Blocked | Status::Stopped)).collect();
    println!();
    if all_done {
        println!(
            "{} all {} tasks done{}",
            paint(c, "32;1", "✓"),
            s.tasks.len(),
            if s.cost() > 0.0 { format!(" · {}", fmt_cost(s.cost())) } else { String::new() }
        );
        if s.isolation == "worktree" {
            println!("  review: musher diff <task>   ·   land it: musher merge");
        }
    } else {
        for t in &attention {
            let hint = match t.status {
                Status::Stopped => "musher resume".to_string(),
                _ => format!("musher logs {0}  ·  musher attach {0}  ·  musher ask {0} \"…\"", t.id),
            };
            println!("{} {}: {}", paint(c, "31", "✗"), t.id, if t.note.is_empty() { t.status.as_str() } else { &t.note });
            println!("    {}", paint(c, "2", &hint));
        }
        if attention.iter().any(|t| t.status != Status::Stopped) {
            println!("  after fixing: musher resume   (re-runs only unfinished tasks)");
        }
        let over_budget = s.tasks.iter().filter(|t| t.status == Status::Skipped && t.note == "budget reached").count();
        if over_budget > 0 {
            println!(
                "{} {over_budget} task{} not started: budget reached ({} spent)",
                paint(c, "33", "–"),
                if over_budget == 1 { "" } else { "s" },
                fmt_cost(s.cost())
            );
            println!("    {}", paint(c, "2", "raise budget_usd in musher.toml, then musher resume"));
        }
    }
    notify(s, all_done);
}

static NOTIFY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_notify(on: bool) {
    NOTIFY.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn notify(s: &RunState, ok: bool) {
    if !NOTIFY.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let done = s.tasks.iter().filter(|t| t.status == Status::Done).count();
    let msg =
        if ok { format!("All {} tasks done {}", s.tasks.len(), fmt_cost(s.cost())) } else { format!("{done}/{} done — some tasks need you", s.tasks.len()) };
    print!("\x07");
    let _ = std::io::stdout().flush();
    if cfg!(target_os = "macos") {
        let script = format!("display notification {:?} with title \"musher\"", msg);
        let _ = std::process::Command::new("osascript").args(["-e", &script]).output();
    } else {
        let _ = std::process::Command::new("notify-send").args(["musher", &msg]).output();
    }
}

/// `--json`: nothing on stdout but the final JSON; progress goes to stderr so pipes stay clean.
pub struct Quiet(Plain);

impl Quiet {
    pub fn new() -> Quiet {
        Quiet(Plain::new())
    }
}

impl Observer for Quiet {
    fn tick(&mut self, run: &Run) {
        let s = run.snapshot();
        for t in &s.tasks {
            if self.0.seen.get(&t.id) != Some(&t.status) {
                if t.status != Status::Queued || self.0.seen.contains_key(&t.id) {
                    eprintln!("[{}] {:<10} {}", fmt_dur(now().saturating_sub(s.started)), t.id, t.status.as_str());
                }
                self.0.seen.insert(t.id.clone(), t.status);
            }
        }
    }
    fn finish(&mut self, run: &Run) {
        let s = run.snapshot();
        notify(&s, s.tasks.iter().all(|t| t.status == Status::Done));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_keeps_escapes_and_counts_visible_chars() {
        assert_eq!(clip_visible("abcdef", 3), "abc");
        assert_eq!(clip_visible("\x1b[31mabcdef\x1b[0m", 3), "\x1b[31mabc\x1b[0m\x1b[0m");
        assert_eq!(clip_visible("한글abc", 4), "한글ab");
        assert_eq!(clip_visible("short", 10), "short");
    }

    #[test]
    fn renders_without_color() {
        let s = RunState {
            id: "r1".into(),
            root: "/x".into(),
            base_commit: "abc".into(),
            base_ref: "main".into(),
            isolation: "worktree".into(),
            started: now(),
            ended: 0,
            pid: 0,
            finished: false,
            tasks: vec![TaskState {
                id: "api".into(),
                agent: "claude".into(),
                status: Status::Working,
                after: vec![],
                branch: String::new(),
                worktree: String::new(),
                start: String::new(),
                session: String::new(),
                attempts: 1,
                started: now(),
                ended: 0,
                cost_usd: 0.5,
                tokens: 0,
                activity: "Edit src/main.rs".into(),
                note: String::new(),
                check: None,
                files_changed: 0,
                pid: 0,
            }],
        };
        let lines = render(&s, 0, 80, false);
        assert!(lines[0].contains("0/1 done"));
        assert!(lines[1].contains("working") && lines[1].contains("Edit src/main.rs") && lines[1].contains("$0.50"));
    }
}
