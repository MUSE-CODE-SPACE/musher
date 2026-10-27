//! Durable run state (`state.json`) and the append-only event log (`events.jsonl`).
//!
//! Everything lives in files under `.musher/`, so there is no server to keep alive:
//! any terminal can `musher watch` a run, and a crashed or stopped run can be resumed.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Setup,
    Working,
    Checking,
    Fixing,
    Done,
    Failed,
    Blocked,
    Skipped,
    Stopped,
}

impl Status {
    pub fn is_final(self) -> bool {
        matches!(self, Status::Done | Status::Failed | Status::Blocked | Status::Skipped | Status::Stopped)
    }
    pub fn is_active(self) -> bool {
        matches!(self, Status::Setup | Status::Working | Status::Checking | Status::Fixing)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Setup => "setup",
            Status::Working => "working",
            Status::Checking => "checking",
            Status::Fixing => "fixing",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Blocked => "blocked",
            Status::Skipped => "skipped",
            Status::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub id: String,
    pub agent: String,
    pub status: Status,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub worktree: String,
    /// Commit the task started from (base + merged dependencies); its own diff is start..branch.
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub started: u64,
    #[serde(default)]
    pub ended: u64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub activity: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub check: Option<bool>,
    #[serde(default)]
    pub files_changed: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub id: String,
    pub root: String,
    pub base_commit: String,
    pub base_ref: String,
    pub isolation: String,
    pub started: u64,
    #[serde(default)]
    pub ended: u64,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub finished: bool,
    pub tasks: Vec<TaskState>,
}

impl RunState {
    pub fn task(&self, id: &str) -> Option<&TaskState> {
        self.tasks.iter().find(|t| t.id == id)
    }
    pub fn task_mut(&mut self, id: &str) -> Option<&mut TaskState> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }
    pub fn cost(&self) -> f64 {
        self.tasks.iter().map(|t| t.cost_usd).sum()
    }
}

#[derive(Serialize)]
struct Event<'a> {
    ts: u64,
    run: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    task: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    msg: &'a str,
}

/// A run on disk plus the lock that serialises writes to it.
pub struct Run {
    pub dir: PathBuf,
    state: Mutex<(RunState, Instant)>,
    events: Mutex<File>,
}

impl Run {
    pub fn create(dir: PathBuf, state: RunState) -> std::io::Result<Run> {
        fs::create_dir_all(dir.join("logs"))?;
        fs::create_dir_all(dir.join("out"))?;
        Run::open_with(dir, state)
    }

    pub fn open(dir: PathBuf) -> Result<Run, String> {
        let state = load_state(&dir)?;
        Run::open_with(dir, state).map_err(|e| e.to_string())
    }

    fn open_with(dir: PathBuf, state: RunState) -> std::io::Result<Run> {
        let events = OpenOptions::new().create(true).append(true).open(dir.join("events.jsonl"))?;
        let run = Run { dir, state: Mutex::new((state, Instant::now())), events: Mutex::new(events) };
        run.save();
        Ok(run)
    }

    pub fn snapshot(&self) -> RunState {
        self.state.lock().unwrap().0.clone()
    }

    pub fn id(&self) -> String {
        self.state.lock().unwrap().0.id.clone()
    }

    /// Mutate state and persist immediately.
    pub fn update<R>(&self, f: impl FnOnce(&mut RunState) -> R) -> R {
        let mut g = self.state.lock().unwrap();
        let r = f(&mut g.0);
        write_state(&self.dir, &g.0);
        g.1 = Instant::now();
        r
    }

    /// Mutate state, persisting at most ~4x per second (for chatty activity updates).
    pub fn update_lazy(&self, f: impl FnOnce(&mut RunState)) {
        let mut g = self.state.lock().unwrap();
        f(&mut g.0);
        if g.1.elapsed().as_millis() >= 250 {
            write_state(&self.dir, &g.0);
            g.1 = Instant::now();
        }
    }

    pub fn task<R>(&self, id: &str, f: impl FnOnce(&mut TaskState) -> R) -> Option<R> {
        self.update(|s| s.task_mut(id).map(f))
    }

    pub fn set_status(&self, id: &str, status: Status, note: &str) {
        self.task(id, |t| {
            t.status = status;
            if !note.is_empty() || status.is_final() {
                t.note = note.to_string();
            }
            if status.is_final() {
                t.ended = now();
                t.activity.clear();
            }
        });
        self.event(id, status.as_str(), note);
    }

    pub fn save(&self) {
        let g = self.state.lock().unwrap();
        write_state(&self.dir, &g.0);
    }

    pub fn event(&self, task: &str, kind: &str, msg: &str) {
        let run = self.id();
        let ev = Event { ts: now(), run: &run, task, kind, msg };
        if let Ok(line) = serde_json::to_string(&ev) {
            let mut f = self.events.lock().unwrap();
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn log_path(&self, task: &str) -> PathBuf {
        self.dir.join("logs").join(format!("{task}.log"))
    }

    pub fn out_path(&self, task: &str) -> PathBuf {
        self.dir.join("out").join(format!("{task}.md"))
    }
}

fn write_state(dir: &Path, s: &RunState) {
    let tmp = dir.join("state.json.tmp");
    if let Ok(json) = serde_json::to_vec_pretty(s) {
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, dir.join("state.json"));
        }
    }
}

pub fn load_state(dir: &Path) -> Result<RunState, String> {
    let p = dir.join("state.json");
    let text = fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("corrupt {}: {e}", p.display()))
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Run ids sort by time: `YYMMDD-HHMMSS` in local time.
pub fn new_run_id() -> String {
    let out = std::process::Command::new("date").arg("+%y%m%d-%H%M%S").output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => format!("r{}", now()),
    }
}

pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    } else {
        let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        b = (n ^ (std::process::id() as u128) << 64).to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// `.musher/runs/*`, newest first.
pub fn list_runs(home: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> =
        fs::read_dir(home.join("runs")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.join("state.json").exists()).collect();
    v.sort();
    v.reverse();
    v
}

pub fn fmt_cost(usd: f64) -> String {
    if usd <= 0.0 {
        String::new()
    } else if usd < 0.1 {
        format!("${usd:.3}")
    } else {
        format!("${usd:.2}")
    }
}

pub fn fmt_dur(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, secs % 3600 / 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_shape() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert_ne!(u, uuid_v4());
    }
}
