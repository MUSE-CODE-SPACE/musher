//! A run as a markdown report: one table, then each task's own summary.
//! Written to `.musher/runs/<id>/report.md` when a run ends, printed by `musher report`,
//! and used as the body of pull requests opened by `musher pr`.

use crate::state::{fmt_cost, fmt_dur, now, RunState, Status, TaskState};
use std::fs;
use std::path::Path;

/// "done" when every task is done; "stopped" when the only unfinished tasks were interrupted
/// (so a resume will simply continue); otherwise "incomplete" (something failed or was blocked).
pub fn run_outcome(s: &RunState) -> &'static str {
    if s.tasks.iter().all(|t| t.status == Status::Done) {
        "done"
    } else if s.tasks.iter().all(|t| matches!(t.status, Status::Done | Status::Stopped | Status::Queued)) {
        "stopped"
    } else {
        "incomplete"
    }
}

fn task_dur(t: &TaskState) -> u64 {
    if t.started == 0 {
        return 0;
    }
    let end = if t.ended > 0 { t.ended } else { now() };
    end.saturating_sub(t.started)
}

fn summary_of(dir: &Path, id: &str) -> String {
    fs::read_to_string(dir.join("out").join(format!("{id}.md"))).unwrap_or_default().trim().to_string()
}

/// Markdown for the whole run (or, with `only`, for those tasks).
pub fn markdown(dir: &Path, s: &RunState, only: &[&str]) -> String {
    let tasks: Vec<&TaskState> = s.tasks.iter().filter(|t| only.is_empty() || only.contains(&t.id.as_str())).collect();
    let done = tasks.iter().filter(|t| t.status == Status::Done).count();
    let cost: f64 = tasks.iter().map(|t| t.cost_usd).sum();
    let elapsed = if s.started > 0 { (if s.ended > 0 { s.ended } else { now() }).saturating_sub(s.started) } else { 0 };
    let mut md = String::new();
    if only.is_empty() {
        md.push_str(&format!("# musher run {} — {}\n\n", s.id, run_outcome(s)));
        md.push_str(&format!("{done}/{} tasks done · {} · {}", tasks.len(), fmt_dur(elapsed), if cost > 0.0 { fmt_cost(cost) } else { "no cost data".into() }));
        if !s.base_ref.is_empty() {
            md.push_str(&format!(" · from `{}` @ {}", s.base_ref, &s.base_commit[..s.base_commit.len().min(7)]));
        }
        md.push_str("\n\n");
    }
    md.push_str("| task | agent | status | attempts | time | cost | files | note |\n|---|---|---|---|---|---|---|---|\n");
    for t in &tasks {
        let check = match t.check {
            Some(true) => " ✓",
            Some(false) => " ✗",
            None => "",
        };
        md.push_str(&format!(
            "| `{}` | {} | {}{check} | {} | {} | {} | {} | {} |\n",
            t.id,
            t.agent,
            t.status.as_str(),
            t.attempts,
            fmt_dur(task_dur(t)),
            fmt_cost(t.cost_usd),
            if t.files_changed > 0 { t.files_changed.to_string() } else { String::new() },
            t.note.replace('|', "\\|")
        ));
    }
    for t in &tasks {
        let sum = summary_of(dir, &t.id);
        if sum.is_empty() {
            continue;
        }
        md.push_str(&format!("\n## {}\n\n", t.id));
        if !t.branch.is_empty() {
            md.push_str(&format!("branch `{}`\n\n", t.branch));
        }
        md.push_str(&sum);
        md.push('\n');
    }
    md
}

pub fn write(dir: &Path, s: &RunState) -> std::path::PathBuf {
    let p = dir.join("report.md");
    let _ = fs::write(&p, markdown(dir, s, &[]));
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_and_sections() {
        let dir = std::env::temp_dir().join(format!("musher-report-{}", std::process::id()));
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join("out/a.md"), "Added the endpoint.").unwrap();
        let t = |id: &str, st: Status| TaskState {
            id: id.into(),
            agent: "claude".into(),
            status: st,
            after: vec![],
            branch: format!("musher/r/{id}"),
            worktree: String::new(),
            start: String::new(),
            session: String::new(),
            attempts: 1,
            started: 100,
            ended: 190,
            cost_usd: 0.5,
            tokens: 0,
            activity: String::new(),
            note: "a | b".into(),
            check: Some(st == Status::Done),
            files_changed: 2,
        };
        let s = RunState {
            id: "r".into(),
            root: String::new(),
            base_commit: "abcdef1234".into(),
            base_ref: "main".into(),
            isolation: "worktree".into(),
            started: 100,
            ended: 200,
            pid: 0,
            finished: true,
            tasks: vec![t("a", Status::Done), t("b", Status::Failed)],
        };
        let md = markdown(&dir, &s, &[]);
        assert!(md.starts_with("# musher run r — incomplete"));
        assert!(md.contains("1/2 tasks done · 01:40 · $1.00 · from `main` @ abcdef1"));
        assert!(md.contains("| `a` | claude | done ✓ | 1 | 01:30 | $0.50 | 2 | a \\| b |"));
        assert!(md.contains("## a\n\nbranch `musher/r/a`\n\nAdded the endpoint."));
        assert!(!md.contains("## b"));
        let one = markdown(&dir, &s, &["b"]);
        assert!(!one.contains("# musher run") && one.contains("| `b` |") && !one.contains("| `a` |"));
        let _ = fs::remove_dir_all(dir);
    }
}
