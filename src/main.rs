mod agent;
mod git;
mod plan;
mod report;
mod run;
mod state;
mod ui;

use plan::{Plan, Task};
use run::{Ctx, Observer};
use state::{list_runs, load_state, now, Run, RunState, Status, TaskState};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, IsTerminal, Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "musher — drive a team of AI coding agents from one small binary.

USAGE
  musher init                       write musher.toml and set up .musher/
  musher run [-j N] [--only a,b]    run the task graph in musher.toml
  musher do \"<prompt>\" [opts]       one-off task: --agent, --check, --id, --retries
  musher resume [run]               continue a stopped or failed run (done tasks are kept)
  musher stop [run]                 stop a background run (agents are terminated, work is kept)
  musher plan \"<goal>\" [--out f]     let an agent split a goal into a musher.toml task graph (--model: for planning and the tasks)

  musher watch [run]                live dashboard from any terminal
  musher status [run] [--json]      snapshot of a run
  musher wait [run]                 block until a run finishes (exit 0 = all done)
  musher ls [--json]                list runs
  musher report [run] [tasks...]    markdown report: status, time, cost, files, each task's summary
  musher logs <task> [-f]           raw agent + check output
  musher events [-f] [--json]       durable event log

  musher diff <task> [--stat]       what a task changed (--all: including stacked deps)
  musher ask <task> \"<message>\"     follow up in the task's agent session, then re-check
  musher check <task>               after editing the worktree yourself: commit and re-run the check, no agent
  musher attach <task>              open the task's session interactively in its worktree
  musher merge [task...]            merge finished task branches into your current branch
  musher merge --resolve            let an agent integrate all finished tasks (conflicts too), verify, then merge
  musher pr <task...|--all> [--draft] push task branches and open pull requests (needs gh)
  musher mem [note...]              show or append shared team notes (injected into prompts)
  musher clean [--purge]            remove worktrees of finished runs (--purge: branches + history)
  musher doctor                     check git and installed agents
  musher completions <zsh|bash|fish> shell completion script

COMMON FLAGS
  -f <file>      plan file (default: musher.toml at the repo root)
  --run <id>     target a specific run (default: latest)
  --bg           run in the background; follow with `musher watch`
  --no-ui        plain line output instead of the live dashboard
  --no-notify    no desktop notification at the end
";

struct Args {
    pos: Vec<String>,
    flags: HashMap<String, String>,
}

const VALUE_FLAGS: &[&str] = &[
    "-f",
    "-j",
    "--only",
    "--run",
    "--agent",
    "--check",
    "--id",
    "--retries",
    "--run-id",
    "--timeout",
    "--model",
    "--permission",
    "--out",
    "--base",
    "--remote",
    "--title",
];

impl Args {
    /// `follow`: the command uses `-f` as "follow" (logs, events), not as the plan-file flag.
    fn parse(raw: &[String], follow: bool) -> Args {
        let (mut pos, mut flags) = (Vec::new(), HashMap::new());
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            if let Some((k, v)) = a.split_once('=').filter(|(k, _)| k.starts_with("--")) {
                flags.insert(k.to_string(), v.to_string());
            } else if VALUE_FLAGS.contains(&a.as_str()) && !(follow && a == "-f") {
                let v = raw.get(i + 1).cloned().unwrap_or_else(|| die(&format!("{a} needs a value")));
                flags.insert(a.clone(), v);
                i += 1;
            } else if a.starts_with('-') && a.len() > 1 && a != "-" {
                flags.insert(a.clone(), String::new());
            } else {
                pos.push(a.clone());
            }
            i += 1;
        }
        Args { pos, flags }
    }
    fn has(&self, k: &str) -> bool {
        self.flags.contains_key(k)
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).map(String::as_str)
    }
}

/// `println!` that survives a closed stdout: commands that change git state (merge, pr)
/// must finish their work even when piped into `head` or `grep -q`.
macro_rules! say {
    ($($arg:tt)*) => {{
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

fn die(msg: &str) -> ! {
    eprintln!("musher: {msg}");
    exit(2)
}

struct Env {
    root: PathBuf,
    home: PathBuf,
    is_git: bool,
}

fn env() -> Env {
    let cwd = std::env::current_dir().unwrap_or_else(|e| die(&e.to_string()));
    match git::root(&cwd) {
        Some(r) => {
            let root = PathBuf::from(r);
            Env { home: root.join(".musher"), root, is_git: true }
        }
        None => Env { home: cwd.join(".musher"), root: cwd, is_git: false },
    }
}

fn main() {
    // `musher status | head` must end quietly like any Unix tool, not panic on a closed pipe.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = raw.first().cloned() else {
        print!("{HELP}");
        return;
    };
    let args = Args::parse(&raw[1..], matches!(cmd.as_str(), "logs" | "log" | "events"));
    if args.has("--no-notify") {
        ui::set_notify(false);
    }
    let code = match cmd.as_str() {
        "init" => cmd_init(),
        "run" => cmd_run(&args, &raw),
        "do" => cmd_do(&args, &raw),
        "resume" => cmd_resume(&args, &raw),
        "stop" | "kill" => cmd_stop(&args),
        "plan" => cmd_plan(&args),
        "report" => cmd_report(&args),
        "pr" => cmd_pr(&args),
        "completions" => cmd_completions(&args),
        "__complete" => cmd_complete(&args),
        "watch" => cmd_watch(&args),
        "status" | "st" => cmd_status(&args),
        "wait" => cmd_wait(&args),
        "ls" | "runs" => cmd_ls(&args),
        "logs" | "log" => cmd_logs(&args),
        "events" => cmd_events(&args),
        "diff" => cmd_diff(&args),
        "ask" => cmd_ask(&args),
        "check" | "verify" => cmd_check(&args),
        "attach" => cmd_attach(&args),
        "merge" => cmd_merge(&args),
        "mem" | "memory" => cmd_mem(&args),
        "clean" => cmd_clean(&args),
        "doctor" => cmd_doctor(),
        "-V" | "--version" | "version" => {
            println!("musher {VERSION}");
            0
        }
        "-h" | "--help" | "help" => {
            print!("{HELP}");
            0
        }
        other => die(&format!("unknown command {other:?} — try `musher help`")),
    };
    exit(code);
}

// ───────────────────────── setup ─────────────────────────

fn cmd_init() -> i32 {
    let e = env();
    let path = e.root.join("musher.toml");
    if path.exists() {
        println!("musher.toml already exists — leaving it alone");
    } else {
        fs::write(&path, plan::TEMPLATE).unwrap_or_else(|err| die(&err.to_string()));
        println!("wrote {}", path.display());
    }
    fs::create_dir_all(&e.home).ok();
    let mem = e.home.join("memory.md");
    if !mem.exists() {
        fs::write(&mem, "").ok();
    }
    if e.is_git {
        git::exclude_musher_dir(&e.root);
        println!(".musher/ is excluded from git (via .git/info/exclude)");
        if git::musher_dir_tracked(&e.root) {
            println!("warning: .musher/ is already tracked by git, so the exclude has no effect — run `git rm -r --cached .musher` and commit");
        }
    } else {
        println!("note: not a git repository — run `git init` for worktree isolation, or set isolation = \"none\"");
    }
    println!("next: edit musher.toml, then `musher run`");
    0
}

fn load_plan(e: &Env, args: &Args) -> Plan {
    let path = args.get("-f").map(PathBuf::from).unwrap_or_else(|| e.root.join("musher.toml"));
    if !path.exists() && args.get("-f").is_none() {
        return Plan::parse("").unwrap();
    }
    let mut p = Plan::load(&path).unwrap_or_else(|err| die(&err));
    apply_overrides(&mut p, args);
    p
}

fn apply_overrides(p: &mut Plan, args: &Args) {
    if let Some(j) = args.get("-j") {
        p.settings.parallel = j.parse().ok().filter(|n| *n > 0).unwrap_or_else(|| die("-j needs a positive number"));
    }
    if let Some(t) = args.get("--timeout") {
        plan::parse_duration(t).unwrap_or_else(|err| die(&err));
        p.settings.timeout = t.to_string();
    }
    if args.has("--no-worktree") {
        p.settings.isolation = "none".into();
    }
    if args.has("--no-notify") {
        p.settings.notify = false;
    }
}

/// Keep only the selected tasks plus everything they depend on.
fn select_tasks(p: &mut Plan, only: &str) {
    let mut keep: Vec<String> = only.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    for k in &keep {
        if p.task(k).is_none() {
            die(&format!("--only: no task {k:?}"));
        }
    }
    let mut i = 0;
    while i < keep.len() {
        let deps = p.task(&keep[i]).map(|t| t.after.clone()).unwrap_or_default();
        for d in deps {
            if !keep.contains(&d) {
                keep.push(d);
            }
        }
        i += 1;
    }
    p.tasks.retain(|t| keep.contains(&t.id));
}

fn new_run(e: &Env, plan: &Plan, args: &Args) -> Arc<Run> {
    if plan.tasks.is_empty() {
        die("no tasks — add [[task]] entries to musher.toml (see `musher init`)");
    }
    // Claude's "plan" mode is read-only: a task that must pass a check cannot change anything.
    let read_only: Vec<&str> = plan
        .tasks
        .iter()
        .filter(|t| {
            t.agent.as_deref().unwrap_or(&plan.settings.agent) == "claude"
                && t.permission.as_deref().unwrap_or(&plan.settings.permission) == "plan"
                && t.check.is_some()
        })
        .map(|t| t.id.as_str())
        .collect();
    if !read_only.is_empty() {
        eprintln!(
            "musher: note — permission \"plan\" is read-only, so {} cannot edit files to satisfy its check; use acceptEdits for tasks that change code",
            read_only.join(", ")
        );
    }
    let (base_commit, base_ref) = if plan.settings.isolation == "worktree" {
        if !e.is_git {
            die("worktree isolation needs a git repository (git init && git commit), or pass --no-worktree");
        }
        let head = git::head(&e.root).unwrap_or_else(|err| die(&err));
        if git::is_dirty(&e.root) {
            eprintln!("musher: note — uncommitted changes are not visible to agents (worktrees start from HEAD)");
        }
        git::exclude_musher_dir(&e.root);
        if git::musher_dir_tracked(&e.root) {
            eprintln!("musher: warning — .musher/ is tracked by git (committed before it was excluded); `git rm -r --cached .musher` keeps run state out of your commits");
        }
        (head, git::current_ref(&e.root))
    } else {
        (String::new(), String::new())
    };
    let id = args.get("--run-id").map(String::from).unwrap_or_else(|| fresh_run_id(e));
    let tasks = plan
        .tasks
        .iter()
        .map(|t| TaskState {
            id: t.id.clone(),
            agent: t.agent.clone().unwrap_or_else(|| plan.settings.agent.clone()),
            status: Status::Queued,
            after: t.after.clone(),
            branch: String::new(),
            worktree: String::new(),
            start: String::new(),
            session: String::new(),
            attempts: 0,
            started: 0,
            ended: 0,
            cost_usd: 0.0,
            tokens: 0,
            activity: String::new(),
            note: String::new(),
            check: None,
            files_changed: 0,
            pid: 0,
        })
        .collect();
    let st = RunState {
        id: id.clone(),
        root: e.root.to_string_lossy().to_string(),
        base_commit,
        base_ref,
        isolation: plan.settings.isolation.clone(),
        started: now(),
        ended: 0,
        pid: std::process::id(),
        finished: false,
        tasks,
    };
    let dir = e.home.join("runs").join(&id);
    if dir.join("state.json").exists() {
        die(&format!("run {id} already exists"));
    }
    let run = Run::create(dir.clone(), st).unwrap_or_else(|err| die(&format!("cannot create run dir: {err}")));
    fs::write(dir.join("plan.json"), serde_json::to_vec_pretty(plan).unwrap()).ok();
    run.event("", "started", &format!("{} tasks, parallel {}", plan.tasks.len(), plan.settings.parallel));
    Arc::new(run)
}

/// A run id that is new to this `.musher/` and to the repo's branches: two runs started in the
/// same second (or from two worktrees of one repo) must not share `musher/<id>/*` branches.
fn fresh_run_id(e: &Env) -> String {
    let base = state::new_run_id();
    let taken = |id: &str| {
        e.home.join("runs").join(id).exists()
            || (e.is_git && git::git(&e.root, &["branch", "--list", &format!("musher/{id}/*")]).map(|o| !o.trim().is_empty()).unwrap_or(false))
    };
    if !taken(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).unwrap()
}

/// Re-launch ourselves detached, writing output to the run directory.
fn go_background(e: &Env, raw: &[String], run_id: &str) -> i32 {
    let dir = e.home.join("runs").join(run_id);
    fs::create_dir_all(&dir).unwrap_or_else(|err| die(&err.to_string()));
    let out = fs::File::create(dir.join("musher.out")).unwrap_or_else(|err| die(&err.to_string()));
    let mut argv: Vec<String> = raw.iter().filter(|a| *a != "--bg").cloned().collect();
    argv.push("--no-ui".into());
    if !raw.iter().any(|a| a == "--run-id") && raw[0] != "resume" {
        argv.push("--run-id".into());
        argv.push(run_id.into());
    }
    let exe = std::env::current_exe().unwrap_or_else(|err| die(&err.to_string()));
    let mut child = Command::new(exe)
        .args(&argv)
        .current_dir(&e.root)
        .stdin(Stdio::null())
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .process_group(0)
        .spawn()
        .unwrap_or_else(|err| die(&format!("cannot start background run: {err}")));
    // Return only once the run is visible, so `musher wait/watch` right after cannot miss it.
    for _ in 0..100 {
        if dir.join("state.json").exists() {
            break;
        }
        if let Ok(Some(st)) = child.try_wait() {
            let out = fs::read_to_string(dir.join("musher.out")).unwrap_or_default();
            let _ = fs::remove_dir_all(&dir);
            die(&format!("background run exited ({st}): {}", out.trim()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("run {run_id} started in the background (pid {})", child.id());
    println!("  musher watch       live dashboard");
    println!("  musher wait        block until it finishes");
    0
}

fn drive(e: &Env, plan: Plan, run: Arc<Run>, args: &Args) -> i32 {
    ui::set_notify(plan.settings.notify && !args.has("--no-notify"));
    run.update(|s| s.pid = std::process::id());
    run::install_signal_handlers();
    let ctx = Arc::new(Ctx { root: e.root.clone(), home: e.home.clone(), plan, run });
    let mut obs: Box<dyn Observer> = if args.has("--json") {
        Box::new(ui::Quiet::new())
    } else if args.has("--no-ui") || !std::io::stdout().is_terminal() {
        Box::new(ui::Plain::new())
    } else {
        Box::new(ui::Live::new())
    };
    let ok = run::execute(ctx.clone(), obs.as_mut());
    if args.has("--json") {
        println!("{}", serde_json::to_string_pretty(&ctx.run.snapshot()).unwrap());
    }
    if ok {
        0
    } else {
        1
    }
}

// ───────────────────────── running ─────────────────────────

fn cmd_run(args: &Args, raw: &[String]) -> i32 {
    let e = env();
    let mut plan = load_plan(&e, args);
    if let Some(only) = args.get("--only") {
        select_tasks(&mut plan, only);
    }
    if args.has("--dry-run") {
        return dry_run(&plan);
    }
    if args.has("--bg") {
        new_run_preflight(&e, &plan);
        return go_background(&e, raw, &fresh_run_id(&e));
    }
    let run = new_run(&e, &plan, args);
    drive(&e, plan, run, args)
}

fn new_run_preflight(e: &Env, plan: &Plan) {
    if plan.tasks.is_empty() {
        die("no tasks — add [[task]] entries to musher.toml");
    }
    if plan.settings.isolation == "worktree" {
        if !e.is_git {
            die("worktree isolation needs a git repository, or pass --no-worktree");
        }
        git::head(&e.root).unwrap_or_else(|err| die(&err));
    }
}

fn dry_run(plan: &Plan) -> i32 {
    // Print tasks in waves: everything in a wave can run in parallel.
    let mut done: Vec<&str> = Vec::new();
    let mut wave = 1;
    while done.len() < plan.tasks.len() {
        let ready: Vec<&Task> = plan.tasks.iter().filter(|t| !done.contains(&t.id.as_str()) && t.after.iter().all(|d| done.contains(&d.as_str()))).collect();
        println!("wave {wave}:");
        for t in &ready {
            let agent = t.agent.as_deref().unwrap_or(&plan.settings.agent);
            let check = t.check.as_deref().map(|c| format!("  check: {c}")).unwrap_or_default();
            println!("  {:<12} [{agent}]{check}", t.id);
            println!("      {}", agent::clip(&t.prompt, 100));
        }
        done.extend(ready.iter().map(|t| t.id.as_str()));
        wave += 1;
    }
    println!(
        "\nparallel {} · isolation {} · timeout {} · retries {}",
        plan.settings.parallel, plan.settings.isolation, plan.settings.timeout, plan.settings.retries
    );
    0
}

fn cmd_do(args: &Args, raw: &[String]) -> i32 {
    let e = env();
    let prompt = args.pos.join(" ");
    if prompt.trim().is_empty() {
        die("usage: musher do \"<prompt>\" [--agent claude] [--check \"npm test\"] [--id name]");
    }
    let mut plan = load_plan(&e, args);
    let id = args.get("--id").map(String::from).unwrap_or_else(|| slug(&prompt));
    plan.tasks = vec![Task {
        id,
        prompt,
        agent: args.get("--agent").map(String::from),
        after: vec![],
        check: args.get("--check").map(String::from),
        retries: args.get("--retries").map(|r| r.parse().unwrap_or_else(|_| die("--retries needs a number"))),
        timeout: None,
        permission: args.get("--permission").map(String::from),
        model: args.get("--model").map(String::from),
        allow: vec![],
    }];
    plan.validate().unwrap_or_else(|err| die(&err));
    if args.has("--bg") {
        new_run_preflight(&e, &plan);
        return go_background(&e, raw, &fresh_run_id(&e));
    }
    let run = new_run(&e, &plan, args);
    drive(&e, plan, run, args)
}

fn slug(s: &str) -> String {
    let words: Vec<String> = s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).take(4).map(|w| w.to_ascii_lowercase()).collect();
    if words.is_empty() {
        "task".into()
    } else {
        words.join("-").chars().take(32).collect()
    }
}

/// The run directory for an id, or a clear error.
fn run_dir(e: &Env, id: &str) -> PathBuf {
    let d = e.home.join("runs").join(id);
    if !d.join("state.json").exists() {
        die(&format!("no run {id:?} — see `musher ls`"));
    }
    d
}

/// `--run <id>`, else the newest run that is still running, else the latest run.
/// For watch/stop: a finished run is rarely the one you mean while another is active.
fn find_active_run(e: &Env, args: &Args) -> PathBuf {
    if args.get("--run").is_none() {
        if let Some(d) = list_runs(&e.home).into_iter().find(|d| load_state(d).map(|st| !st.finished && pid_alive(st.pid)).unwrap_or(false)) {
            return d;
        }
    }
    find_run(e, args)
}

/// `--run <id>`, else the latest run.
fn find_run(e: &Env, args: &Args) -> PathBuf {
    if let Some(id) = args.get("--run") {
        return run_dir(e, id);
    }
    list_runs(&e.home).into_iter().next().unwrap_or_else(|| die("no runs yet — start one with `musher run` or `musher do`"))
}

fn load_run_plan(dir: &Path) -> Plan {
    let text = fs::read_to_string(dir.join("plan.json")).unwrap_or_else(|_| die("run has no plan snapshot"));
    let p: Plan = serde_json::from_str(&text).unwrap_or_else(|err| die(&format!("bad plan snapshot: {err}")));
    p
}

fn pid_alive(pid: u32) -> bool {
    pid != 0 && pid != std::process::id() && unsafe { libc::kill(pid as i32, 0) } == 0
}

fn cmd_resume(args: &Args, raw: &[String]) -> i32 {
    let e = env();
    let id_flag = args.pos.first().map(String::from).or_else(|| args.get("--run").map(String::from));
    let dir = match id_flag {
        Some(id) => run_dir(&e, &id),
        None => find_run(&e, args),
    };
    let st = load_state(&dir).unwrap_or_else(|err| die(&err));
    if !st.finished && pid_alive(st.pid) {
        die(&format!("run {} is still running (pid {}) — `musher watch`", st.id, st.pid));
    }
    // Tasks added to musher.toml after the run started are not part of it.
    let plan_path = args.get("-f").map(PathBuf::from).unwrap_or_else(|| e.root.join("musher.toml"));
    let new_tasks: Vec<String> =
        Plan::load(&plan_path).map(|p| p.tasks.iter().filter(|t| st.task(&t.id).is_none()).map(|t| t.id.clone()).collect()).unwrap_or_default();
    if !new_tasks.is_empty() {
        eprintln!(
            "musher: note — {} in musher.toml {} not in run {}; `musher run --only {}` runs {}",
            new_tasks.join(", "),
            if new_tasks.len() == 1 { "is" } else { "are" },
            st.id,
            new_tasks.join(","),
            if new_tasks.len() == 1 { "it" } else { "them" }
        );
    }
    if st.tasks.iter().all(|t| t.status == Status::Done) {
        println!("run {} is already complete", st.id);
        return 0;
    }
    if args.has("--bg") {
        let mut raw2: Vec<String> = raw.to_vec();
        if args.pos.is_empty() && args.get("--run").is_none() {
            raw2.push(st.id.clone());
        }
        return go_background(&e, &raw2, &st.id);
    }
    let mut plan = load_run_plan(&dir);
    if let Some(fresh) = updated_plan(&e, args, &st) {
        eprintln!("musher: using the current musher.toml for unfinished tasks");
        plan = fresh;
    }
    apply_overrides(&mut plan, args);
    fs::write(dir.join("plan.json"), serde_json::to_vec_pretty(&plan).unwrap()).ok();
    let run = Arc::new(Run::open(dir).unwrap_or_else(|err| die(&err)));
    // Agents left behind by a crashed runner would race the new attempt in the same worktree.
    for t in st.tasks.iter().filter(|t| t.status != Status::Done && t.pid != 0 && pid_alive(t.pid)) {
        eprintln!("musher: stopping the previous agent of {} (pid {})", t.id, t.pid);
        run::kill_group(t.pid);
    }
    run.update(|s| {
        s.finished = false;
        s.ended = 0;
        for t in s.tasks.iter_mut().filter(|t| t.status != Status::Done) {
            t.pid = 0;
            t.status = Status::Queued;
            t.note.clear();
            t.activity.clear();
        }
    });
    run.event("", "resumed", "");
    drive(&e, plan, run, args)
}

/// The plan file, narrowed to this run's tasks, if it still defines all of them with the
/// same dependencies — so a prompt or check fixed in musher.toml takes effect on resume.
fn updated_plan(e: &Env, args: &Args, st: &RunState) -> Option<Plan> {
    let path = args.get("-f").map(PathBuf::from).unwrap_or_else(|| e.root.join("musher.toml"));
    let mut p = Plan::load(&path).ok()?;
    for t in &st.tasks {
        if p.task(&t.id)?.after != t.after {
            return None;
        }
    }
    p.tasks.retain(|x| st.task(&x.id).is_some());
    Some(p)
}

// ───────────────────────── observing ─────────────────────────

fn cmd_status(args: &Args) -> i32 {
    let e = env();
    let st = load_state(&find_run(&e, args)).unwrap_or_else(|err| die(&err));
    if args.has("--json") {
        println!("{}", serde_json::to_string_pretty(&st).unwrap());
    } else {
        for l in ui::render(&st, 0, ui::term_width(), ui::color()) {
            println!("{l}");
        }
        if !st.finished && !pid_alive(st.pid) {
            println!("  (runner is gone — `musher resume` to continue)");
        }
    }
    0
}

fn cmd_watch(args: &Args) -> i32 {
    let e = env();
    let dir = match args.pos.first() {
        Some(id) => run_dir(&e, id),
        None => find_active_run(&e, args),
    };
    run::install_signal_handlers();
    let mut live = ui::Live::new();
    loop {
        let Ok(st) = load_state(&dir) else {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        live.draw(&st);
        let orphaned = !st.finished && st.pid != 0 && !pid_alive(st.pid);
        if st.finished || orphaned || run::STOP.load(Ordering::SeqCst) {
            if orphaned {
                println!("runner is gone — `musher resume` to continue");
            }
            return if st.tasks.iter().all(|t| t.status == Status::Done) { 0 } else { 1 };
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn cmd_wait(args: &Args) -> i32 {
    let e = env();
    let dir = match args.pos.first() {
        Some(id) => run_dir(&e, id),
        None => find_run(&e, args),
    };
    loop {
        if let Ok(st) = load_state(&dir) {
            if st.finished || (st.pid != 0 && !pid_alive(st.pid)) {
                if args.has("--json") {
                    println!("{}", serde_json::to_string_pretty(&st).unwrap());
                }
                return if st.tasks.iter().all(|t| t.status == Status::Done) { 0 } else { 1 };
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn run_state_word(st: &RunState) -> &'static str {
    if st.finished {
        report::run_outcome(st)
    } else if pid_alive(st.pid) {
        "running"
    } else {
        "interrupted"
    }
}

fn cmd_ls(args: &Args) -> i32 {
    let e = env();
    let runs = list_runs(&e.home);
    if args.has("--json") {
        let v: Vec<serde_json::Value> = runs
            .iter()
            .filter_map(|d| load_state(d).ok())
            .map(|st| {
                let done = st.tasks.iter().filter(|t| t.status == Status::Done).count();
                serde_json::json!({"id": st.id, "state": run_state_word(&st), "done": done, "total": st.tasks.len(), "cost_usd": st.cost(), "started": st.started, "ended": st.ended, "tasks": st.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>()})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return 0;
    }
    if runs.is_empty() {
        println!("no runs yet");
    }
    for d in runs {
        if let Ok(st) = load_state(&d) {
            let done = st.tasks.iter().filter(|t| t.status == Status::Done).count();
            let names: Vec<&str> = st.tasks.iter().map(|t| t.id.as_str()).collect();
            println!("{}  {:<11} {}/{}  ${:.2}  {}", st.id, run_state_word(&st), done, st.tasks.len(), st.cost(), agent::clip(&names.join(", "), 60));
        }
    }
    0
}

fn cmd_stop(args: &Args) -> i32 {
    let e = env();
    let dir = match args.pos.first() {
        Some(id) => run_dir(&e, id),
        None => find_active_run(&e, args),
    };
    let st = load_state(&dir).unwrap_or_else(|err| die(&err));
    if st.finished || !pid_alive(st.pid) {
        println!("run {} is not running", st.id);
        return 0;
    }
    unsafe {
        libc::kill(st.pid as i32, libc::SIGTERM);
    }
    print!("stopping run {} (pid {}) ", st.id, st.pid);
    for _ in 0..60 {
        if let Ok(now) = load_state(&dir) {
            if now.finished || !pid_alive(now.pid) {
                println!("— stopped. Finished tasks are kept; `musher resume` continues the rest.");
                return 0;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    println!("— still shutting down; check `musher status`");
    1
}

fn cmd_report(args: &Args) -> i32 {
    let e = env();
    let dir = match args.pos.first().filter(|p| e.home.join("runs").join(p).join("state.json").exists()) {
        Some(id) => e.home.join("runs").join(id),
        None => find_run(&e, args),
    };
    let st = load_state(&dir).unwrap_or_else(|err| die(&err));
    let only: Vec<&str> = args.pos.iter().map(String::as_str).filter(|p| st.task(p).is_some()).collect();
    if let Some(bad) = args.pos.iter().find(|p| st.task(p).is_none() && **p != st.id) {
        die(&format!("run {} has no task {bad:?} — see `musher status`", st.id));
    }
    print!("{}", report::markdown(&dir, &st, &only));
    0
}

/// Push finished task branches and open a pull request for each, body = that task's report.
fn cmd_pr(args: &Args) -> i32 {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN); // finish every branch even if the reader went away
    }
    let e = env();
    let dir = find_run(&e, args);
    let st = load_state(&dir).unwrap_or_else(|err| die(&err));
    if st.isolation != "worktree" {
        die("this run edited the repo directly — no task branches to open PRs from");
    }
    if Command::new("gh").arg("--version").output().map(|o| !o.status.success()).unwrap_or(true) {
        die("`gh` (GitHub CLI) is required: https://cli.github.com");
    }
    let ids: Vec<&str> = if args.has("--all") {
        st.tasks.iter().filter(|t| t.status == Status::Done).map(|t| t.id.as_str()).collect()
    } else {
        args.pos.iter().map(String::as_str).collect()
    };
    if ids.is_empty() {
        die("usage: musher pr <task...> | --all  [--draft] [--base main] [--remote origin]");
    }
    let remote = args.get("--remote").unwrap_or("origin");
    if git::git(&e.root, &["remote", "get-url", remote]).is_err() {
        die(&format!("no git remote {remote:?} — add one (`git remote add origin <url>`) or pass --remote"));
    }
    let base = args.get("--base").map(String::from).unwrap_or_else(|| st.base_ref.clone());
    let mut code = 0;
    for id in ids {
        let Some(t) = st.task(id) else {
            eprintln!("skip {id}: not in run {}", st.id);
            code = 1;
            continue;
        };
        if t.status != Status::Done {
            eprintln!("skip {id}: {}{}", t.status.as_str(), if t.note.is_empty() { String::new() } else { format!(" — {}", t.note) });
            code = 1;
            continue;
        }
        if !git::branch_exists(&e.root, &t.branch) {
            eprintln!("skip {id}: branch {} is gone", t.branch);
            code = 1;
            continue;
        }
        if let Err(err) = git::git(&e.root, &["push", "-u", remote, &format!("{}:{}", t.branch, t.branch)]) {
            eprintln!("✗ {id}: {}", agent::clip(&err, 200));
            code = 1;
            continue;
        }
        // Already open? The push above updated it; do not open a second one.
        if let Ok(o) =
            Command::new("gh").args(["pr", "list", "--head", &t.branch, "--state", "open", "--json", "url", "-q", ".[0].url"]).current_dir(&e.root).output()
        {
            let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if o.status.success() && !url.is_empty() {
                say!("= {id}: already open, branch updated: {url}");
                continue;
            }
        }
        let summary = fs::read_to_string(dir.join("out").join(format!("{id}.md"))).unwrap_or_default();
        let title = args.get("--title").map(String::from).unwrap_or_else(|| {
            let first = summary.lines().find(|l| !l.trim().is_empty()).map(|l| l.trim_start_matches('#').trim()).unwrap_or("");
            if first.is_empty() || first.len() > 72 {
                format!("musher: {id}")
            } else {
                format!("{id}: {first}")
            }
        });
        let stat = git::git(&e.root, &["diff", "--stat", &format!("{}..{}", if t.start.is_empty() { &st.base_commit } else { &t.start }, t.branch)])
            .unwrap_or_default();
        let mut body = report::markdown(&dir, &st, &[id]);
        if !stat.trim().is_empty() {
            body.push_str(&format!("\n<details><summary>diff --stat</summary>\n\n```\n{}\n```\n</details>\n", stat.trim_end()));
        }
        body.push_str(&format!("\n---\nOpened by [musher](https://github.com/MUSE-CODE-SPACE/musher) run `{}`.\n", st.id));
        let body_path = dir.join(format!("pr-{id}.md"));
        fs::write(&body_path, &body).unwrap_or_else(|err| die(&err.to_string()));
        let mut c = Command::new("gh");
        c.arg("pr")
            .arg("create")
            .arg("--head")
            .arg(&t.branch)
            .arg("--base")
            .arg(&base)
            .arg("--title")
            .arg(&title)
            .arg("--body-file")
            .arg(&body_path)
            .current_dir(&e.root);
        if args.has("--draft") {
            c.arg("--draft");
        }
        match c.output() {
            Ok(o) if o.status.success() => say!("✓ {id}: {}", String::from_utf8_lossy(&o.stdout).trim()),
            Ok(o) => {
                eprintln!("✗ {id}: {}", String::from_utf8_lossy(&o.stderr).trim());
                code = 1;
            }
            Err(err) => die(&format!("cannot run gh: {err}")),
        }
    }
    code
}

/// Ask the configured agent to turn a goal into a task graph, validate it, write musher.toml.
fn cmd_plan(args: &Args) -> i32 {
    let e = env();
    let goal = args.pos.join(" ");
    if goal.trim().is_empty() {
        die("usage: musher plan \"<goal>\" [--agent claude] [--out musher.toml] [--force]");
    }
    let plan = load_plan(&e, args);
    let out = args.get("--out").map(PathBuf::from).unwrap_or_else(|| e.root.join("musher.toml"));
    let existing_tasks = Plan::load(&out).map(|p| !p.tasks.is_empty()).unwrap_or(false);
    if existing_tasks && !args.has("--force") && !args.has("--print") {
        die(&format!("{} already has tasks — pass --force to replace them, --print to only show the proposal, or --out <file>", out.display()));
    }
    let name = args.get("--agent").map(String::from).unwrap_or_else(|| plan.settings.agent.clone());
    let spec = plan.agents.get(&name).unwrap_or_else(|| die(&format!("unknown agent {name:?}")));
    let prompt = format!(
        "Look at this repository, then split the goal below into musher tasks.\n\n\
         Rules:\n\
         - Output ONLY a TOML document with one or more [[task]] entries (no [settings], no prose, no code fence).\n\
         - Each task: id (short, [a-z0-9-]), prompt (precise instructions a coding agent can finish alone; name files and behaviour), \
         check (a shell command that exits 0 only when the task is correctly done, e.g. the project's test command; omit it only if nothing is testable), \
         after (ids this task must build on; leave out when independent).\n\
         - Prefer independent tasks that can run in parallel in separate git worktrees; use after only for a real code dependency.\n\
         - 2 to 6 tasks. Do not modify any file.\n\n\
         Goal: {goal}\n"
    );
    let argv = agent::build_argv(
        &spec.cmd,
        &agent::Vars {
            prompt: &prompt,
            session: &state::uuid_v4(),
            cwd: &e.root.to_string_lossy(),
            permission: "plan",
            model: args.get("--model").unwrap_or(&spec.model),
            allow: "",
        },
    );
    eprintln!("musher: asking {name} to plan…");
    let started = std::time::Instant::now();
    let o = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(&e.root)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|err| die(&format!("cannot start {}: {err}", argv[0])));
    let text = String::from_utf8_lossy(&o.stdout);
    let (mut result, mut last, mut cost) = (String::new(), String::new(), 0.0);
    for line in text.lines() {
        let s = agent::parse_line(line);
        if let Some(r) = s.result {
            result = r;
        }
        if let Some(t) = s.text {
            last = t;
        }
        if let Some(c) = s.cost_usd {
            cost += c;
        }
    }
    eprintln!(
        "musher: plan ready in {}{}",
        state::fmt_dur(started.elapsed().as_secs()),
        if cost > 0.0 { format!(" · {}", state::fmt_cost(cost)) } else { String::new() }
    );
    if result.is_empty() {
        result = if spec.raw { text.to_string() } else { last };
    }
    let mut toml = extract_toml(&result);
    // --model names the model for planning *and* for the tasks it writes.
    if let Some(m) = args.get("--model") {
        toml =
            toml.lines().map(|l| if l.trim() == "[[task]]" { format!("[[task]]\nmodel = {m:?}") } else { l.to_string() }).collect::<Vec<_>>().join("\n") + "\n";
    }
    if toml.trim().is_empty() {
        die(&format!("the agent returned no plan (exit {}): {}", o.status.code().unwrap_or(-1), agent::clip(&String::from_utf8_lossy(&o.stderr), 200)));
    }
    let proposed = Plan::parse(&toml).unwrap_or_else(|err| die(&format!("the agent's plan is invalid: {err}\n\n{toml}")));
    if args.has("--print") {
        print!("{toml}");
        return 0;
    }
    // Keep the user's [settings]/[agents]; replace the tasks.
    let head = fs::read_to_string(&out).ok().map(|cur| cur.split("[[task]]").next().unwrap_or("").trim_end().to_string()).filter(|h| !h.is_empty());
    let mut file = match head {
        Some(h) => format!("{h}\n\n"),
        None => format!("# musher.toml — generated by `musher plan`\n[settings]\nparallel = {}\nagent = \"{name}\"\n\n", plan.settings.parallel),
    };
    file.push_str(&format!("# goal: {}\n", agent::clip(&goal, 200)));
    file.push_str(toml.trim_start());
    if !file.ends_with('\n') {
        file.push('\n');
    }
    Plan::parse(&file).unwrap_or_else(|err| die(&format!("internal: generated file is invalid: {err}")));
    fs::write(&out, &file).unwrap_or_else(|err| die(&err.to_string()));
    println!("wrote {} ({} tasks)\n", out.display(), proposed.tasks.len());
    dry_run(&proposed);
    println!("\nedit as you like, then `musher run`");
    0
}

/// The task tables of an agent answer: inside a ``` fence if there is one, from the first
/// `[[task]]` on. Anything the agent put before that (prose, a `[settings]` table despite the
/// instructions) is dropped, because the user's own settings are kept.
fn extract_toml(s: &str) -> String {
    let mut body = s;
    if let Some(i) = s.find("```") {
        let rest = &s[i + 3..];
        let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest);
        if let Some(j) = rest.find("```") {
            body = &rest[..j];
        }
    }
    match body.find("[[task]]") {
        Some(i) => body[i..].to_string(),
        None => String::new(),
    }
}

fn cmd_completions(args: &Args) -> i32 {
    const CMDS: &str =
        "init run do resume stop plan watch status wait ls report logs events diff ask attach merge pr mem clean doctor completions help version";
    const TASK_CMDS: &str = "logs diff ask check attach pr";
    const RUN_CMDS: &str = "resume stop watch wait report";
    match args.pos.first().map(String::as_str) {
        Some("zsh") => print!(
            r##"#compdef musher
_musher() {{
  local -a cmds; cmds=({CMDS})
  if (( CURRENT == 2 )); then _describe 'command' cmds; return; fi
  case $words[2] in
    {t}) _values 'task' $(musher __complete tasks 2>/dev/null) ;;
    {r}) _values 'run' $(musher __complete runs 2>/dev/null) ;;
    completions) _values 'shell' zsh bash fish ;;
    *) _files ;;
  esac
}}
compdef _musher musher
"##,
            t = TASK_CMDS.replace(' ', "|"),
            r = RUN_CMDS.replace(' ', "|")
        ),
        Some("bash") => print!(
            r##"_musher() {{
  local cur=${{COMP_WORDS[COMP_CWORD]}}
  if [ $COMP_CWORD -eq 1 ]; then COMPREPLY=($(compgen -W "{CMDS}" -- "$cur")); return; fi
  case ${{COMP_WORDS[1]}} in
    {t}) COMPREPLY=($(compgen -W "$(musher __complete tasks 2>/dev/null)" -- "$cur")) ;;
    {r}) COMPREPLY=($(compgen -W "$(musher __complete runs 2>/dev/null)" -- "$cur")) ;;
    completions) COMPREPLY=($(compgen -W "zsh bash fish" -- "$cur")) ;;
  esac
}}
complete -F _musher musher
"##,
            t = TASK_CMDS.replace(' ', "|"),
            r = RUN_CMDS.replace(' ', "|")
        ),
        Some("fish") => {
            println!("complete -c musher -f");
            for c in CMDS.split(' ') {
                println!("complete -c musher -n '__fish_use_subcommand' -a {c}");
            }
            println!("complete -c musher -n '__fish_seen_subcommand_from {TASK_CMDS}' -a '(musher __complete tasks 2>/dev/null)'");
            println!("complete -c musher -n '__fish_seen_subcommand_from {RUN_CMDS}' -a '(musher __complete runs 2>/dev/null)'");
            println!("complete -c musher -n '__fish_seen_subcommand_from completions' -a 'zsh bash fish'");
        }
        _ => die("usage: musher completions <zsh|bash|fish>   e.g. musher completions zsh > ~/.zfunc/_musher"),
    }
    0
}

/// Names for shell completion: task ids of the latest run, or run ids.
fn cmd_complete(args: &Args) -> i32 {
    let e = env();
    match args.pos.first().map(String::as_str) {
        Some("runs") => list_runs(&e.home).iter().filter_map(|d| d.file_name().map(|n| n.to_string_lossy().to_string())).for_each(|n| println!("{n}")),
        Some("tasks") => {
            if let Some(d) = list_runs(&e.home).first() {
                if let Ok(st) = load_state(d) {
                    st.tasks.iter().for_each(|t| println!("{}", t.id));
                }
            }
        }
        _ => {}
    }
    0
}

fn task_arg<'a>(args: &'a Args, usage: &str) -> &'a str {
    args.pos.first().map(String::as_str).unwrap_or_else(|| die(usage))
}

fn cmd_logs(args: &Args) -> i32 {
    let e = env();
    let dir = find_run(&e, args);
    let id = task_arg(args, "usage: musher logs <task> [-f]");
    let path = dir.join("logs").join(format!("{id}.log"));
    let mut f = fs::File::open(&path).unwrap_or_else(|_| die(&format!("no log for {id:?} yet")));
    // Stream: a chatty agent can leave a log of tens of megabytes.
    {
        let out = std::io::stdout();
        let mut out = std::io::BufWriter::new(out.lock());
        let mut reader = std::io::BufReader::new(&mut f);
        let mut line = String::new();
        while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            let _ = out.write_all(pretty_log(&line, args.has("--raw")).as_bytes());
            line.clear();
        }
        let _ = out.flush();
    }
    if !args.has("-f") {
        return 0;
    }
    run::install_signal_handlers();
    let mut pos = f.stream_position().unwrap_or(0);
    let mut pending = String::new();
    loop {
        if run::STOP.load(Ordering::SeqCst) {
            return 0;
        }
        let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(pos);
        if len > pos {
            let _ = f.seek(SeekFrom::Start(pos));
            let mut chunk = String::new();
            let _ = f.read_to_string(&mut chunk);
            pos = len;
            pending.push_str(&chunk);
            if let Some(cut) = pending.rfind('\n') {
                let complete: String = pending.drain(..=cut).collect();
                print!("{}", pretty_log(&complete, args.has("--raw")));
            }
        } else if let Ok(st) = load_state(&dir) {
            let fin = st.task(id).map(|t| t.status.is_final()).unwrap_or(true);
            if fin && (st.finished || !pid_alive(st.pid)) {
                return 0;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// JSON event streams are noisy; show the human parts.
fn pretty_log(text: &str, raw: bool) -> String {
    if raw {
        return text.to_string();
    }
    let mut out = String::new();
    for line in text.lines() {
        if !line.trim_start().starts_with('{') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let s = agent::parse_line(line);
        if let Some(a) = s.activity {
            out.push_str(&format!("  → {a}\n"));
        }
        if let Some(r) = s.result {
            out.push_str(&format!("\n{r}\n"));
        } else if let Some(t) = s.text {
            out.push_str(&format!("{t}\n"));
        }
        if let Some(c) = s.cost_usd {
            out.push_str(&format!("  (cost ${c:.4})\n"));
        }
        for d in s.denied {
            out.push_str(&format!("  ! denied: {d}\n"));
        }
    }
    out
}

fn cmd_events(args: &Args) -> i32 {
    let e = env();
    let dir = find_run(&e, args);
    let path = dir.join("events.jsonl");
    let print_line = |l: &str| {
        if args.has("--json") {
            println!("{l}");
            return;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
            let ts = v["ts"].as_u64().unwrap_or(0);
            let clock = state::fmt_clock(ts);
            println!("{clock}  {:<10} {:<13} {}", v["task"].as_str().unwrap_or("-"), v["kind"].as_str().unwrap_or(""), v["msg"].as_str().unwrap_or(""));
        }
    };
    let text = fs::read_to_string(&path).unwrap_or_default();
    text.lines().for_each(print_line);
    if !args.has("-f") {
        return 0;
    }
    run::install_signal_handlers();
    // Follow by reading only what was appended since the last look (the log is append-only).
    let mut pos = text.len() as u64;
    let mut pending = String::new();
    while !run::STOP.load(Ordering::SeqCst) {
        let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(pos);
        if len > pos {
            if let Ok(mut f) = fs::File::open(&path) {
                let _ = f.seek(SeekFrom::Start(pos));
                let mut chunk = String::new();
                let _ = f.read_to_string(&mut chunk);
                pos = len;
                pending.push_str(&chunk);
                if let Some(cut) = pending.rfind('\n') {
                    let complete: String = pending.drain(..=cut).collect();
                    complete.lines().for_each(print_line);
                }
            }
        } else if let Ok(st) = load_state(&dir) {
            // Nothing new and the runner is gone: the log is complete.
            if st.finished || !pid_alive(st.pid) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    0
}

// ───────────────────────── acting on results ─────────────────────────

fn run_task_state(e: &Env, args: &Args, usage: &str) -> (PathBuf, RunState, TaskState) {
    let dir = find_run(e, args);
    let st = load_state(&dir).unwrap_or_else(|err| die(&err));
    let id = task_arg(args, usage);
    let t = st.task(id).cloned().unwrap_or_else(|| die(&format!("run {} has no task {id:?}", st.id)));
    (dir, st, t)
}

fn cmd_diff(args: &Args) -> i32 {
    let e = env();
    let (_, st, t) = run_task_state(&e, args, "usage: musher diff <task> [--stat]");
    if t.branch.is_empty() {
        die("this task ran without worktree isolation — use plain `git diff`");
    }
    let mut c = Command::new("git");
    c.arg("-C").arg(&e.root).arg("diff");
    if args.has("--stat") {
        c.arg("--stat");
    }
    let from = if args.has("--all") || t.start.is_empty() { &st.base_commit } else { &t.start };
    c.arg(format!("{from}..{}", t.branch));
    c.status().map(|s| s.code().unwrap_or(1)).unwrap_or(1)
}

fn cmd_attach(args: &Args) -> i32 {
    let e = env();
    let (dir, _, t) = run_task_state(&e, args, "usage: musher attach <task>");
    let plan = load_run_plan(&dir);
    let spec = plan.agents.get(&t.agent).unwrap_or_else(|| die(&format!("unknown agent {:?}", t.agent)));
    if spec.attach.is_empty() {
        die(&format!("agent {:?} has no interactive mode — open the worktree yourself: cd {}", t.agent, t.worktree));
    }
    let cwd = if t.worktree.is_empty() { e.root.clone() } else { PathBuf::from(&t.worktree) };
    let argv =
        agent::build_argv(&spec.attach, &agent::Vars { prompt: "", session: &t.session, cwd: &cwd.to_string_lossy(), permission: "", model: "", allow: "" });
    eprintln!("musher: attaching to {} ({}) in {}", t.id, t.agent, cwd.display());
    eprintln!("musher: when you're done, `musher ask`/`musher resume` pick up from here; changes are committed on the next musher step");
    let err = Command::new(&argv[0]).args(&argv[1..]).current_dir(&cwd).exec();
    die(&format!("cannot start {}: {err}", argv[0]))
}

fn cmd_ask(args: &Args) -> i32 {
    let e = env();
    let (dir, st, t) = run_task_state(&e, args, "usage: musher ask <task> \"<message>\"");
    let msg = args.pos[1..].join(" ");
    if msg.trim().is_empty() {
        die("usage: musher ask <task> \"<message>\"");
    }
    if !st.finished && pid_alive(st.pid) {
        // The runner (or another ask) owns this run's state; two writers would race.
        die(&format!("run {} is still in progress (pid {}) — wait for it (`musher wait`) or `musher stop` first", st.id, st.pid));
    }
    let mut plan = load_run_plan(&dir);
    apply_overrides(&mut plan, args);
    plan.tasks.retain(|x| x.id == t.id);
    let run = Arc::new(Run::open(dir).unwrap_or_else(|err| die(&err)));
    run.update(|s| {
        s.pid = std::process::id();
        s.finished = false; // so a concurrent ask/resume sees the run as busy
        s.ended = 0;
    });
    run.event(&t.id, "ask", &agent::clip(&msg, 200));
    run::install_signal_handlers();
    let ctx = Arc::new(Ctx { root: e.root.clone(), home: e.home.clone(), plan, run: run.clone() });
    let id = t.id.clone();
    let worker = {
        let ctx = ctx.clone();
        std::thread::spawn(move || run::run_task(&ctx, &id, Some(&msg)))
    };
    let mut live = ui::Live::new();
    let tty = std::io::stdout().is_terminal() && !args.has("--no-ui");
    while !worker.is_finished() {
        if tty {
            live.draw(&run.snapshot());
        }
        if run::STOP.load(Ordering::SeqCst) {
            run::kill_children();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = worker.join();
    run.update(|s| {
        s.finished = true;
        s.ended = now();
    });
    let snap = run.snapshot();
    if tty {
        live.draw(&snap);
    }
    let t = snap.task(&t.id).unwrap();
    println!("{} {}{}", t.id, t.status.as_str(), if t.note.is_empty() { String::new() } else { format!(" — {}", t.note) });
    if let Ok(out) = fs::read_to_string(run.out_path(&t.id)) {
        if !out.trim().is_empty() {
            println!("\n{}", out.trim());
        }
    }
    if t.status == Status::Done {
        0
    } else {
        1
    }
}

/// Re-run a task's check on whatever is in its worktree now (your own edits included).
fn cmd_check(args: &Args) -> i32 {
    let e = env();
    let (dir, st, t) = run_task_state(&e, args, "usage: musher check <task>");
    if !st.finished && pid_alive(st.pid) {
        die(&format!("run {} is still in progress (pid {}) — wait for it or `musher stop` first", st.id, st.pid));
    }
    let mut plan = load_run_plan(&dir);
    plan.tasks.retain(|x| x.id == t.id);
    let run = Arc::new(Run::open(dir).unwrap_or_else(|err| die(&err)));
    run.update(|s| {
        s.pid = std::process::id();
        s.finished = false;
    });
    run.event(&t.id, "check", "manual");
    let ctx = Ctx { root: e.root.clone(), home: e.home.clone(), plan, run: run.clone() };
    let ok = run::recheck(&ctx, &t.id);
    run.update(|s| {
        s.finished = true;
        s.ended = now();
    });
    let t = run.snapshot().task(&t.id).cloned().unwrap();
    println!("{} {}{}", t.id, t.status.as_str(), if t.note.is_empty() { String::new() } else { format!(" — {}", t.note) });
    if ok {
        0
    } else {
        1
    }
}

fn cmd_merge(args: &Args) -> i32 {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN); // finish every branch even if the reader went away
    }
    let e = env();
    let dir = find_run(&e, args);
    if args.has("--resolve") {
        return merge_resolve(&e, args, &dir);
    }
    let st = load_state(&dir).unwrap_or_else(|err| die(&err));
    if st.isolation != "worktree" {
        die("this run edited the repo directly — nothing to merge");
    }
    if !st.finished && pid_alive(st.pid) {
        die("run is still in progress");
    }
    if git::is_dirty(&e.root) {
        die("your working tree has uncommitted changes — commit or stash them first");
    }
    let here = git::current_ref(&e.root);
    if !st.base_ref.is_empty() && st.base_ref != "HEAD" && here != st.base_ref {
        eprintln!("musher: note — merging into `{here}`; the run started from `{}`", st.base_ref);
    }
    let wanted: Vec<&str> = args.pos.iter().map(String::as_str).collect();
    // Plan order is a valid topological order only if deps come first; sort to be safe.
    let mut order: Vec<&TaskState> = Vec::new();
    let mut placed: Vec<&str> = Vec::new();
    while order.len() < st.tasks.len() {
        let before = order.len();
        for t in &st.tasks {
            if !placed.contains(&t.id.as_str()) && t.after.iter().all(|d| placed.contains(&d.as_str())) {
                placed.push(&t.id);
                order.push(t);
            }
        }
        if order.len() == before {
            break;
        }
    }
    let mut merged = 0;
    let mut candidates: Vec<&TaskState> = Vec::new();
    for t in order {
        if !wanted.is_empty() && !wanted.contains(&t.id.as_str()) {
            continue;
        }
        if t.status != Status::Done {
            if !wanted.is_empty() {
                eprintln!("skip {}: {}", t.id, t.status.as_str());
            }
            continue;
        }
        if !git::branch_exists(&e.root, &t.branch) {
            eprintln!("skip {}: branch {} is gone", t.id, t.branch);
            continue;
        }
        if git::git(&e.root, &["rev-list", "--count", &format!("{}..{}", st.base_commit, t.branch)]).map(|n| n.trim() == "0").unwrap_or(false) {
            say!("= {} made no commits", t.id);
            continue;
        }
        candidates.push(t);
    }
    // A branch stacked into a later task's branch arrives with it, conflicts already resolved there.
    let contained = |t: &TaskState| {
        candidates.iter().find(|u| u.id != t.id && git::git(&e.root, &["merge-base", "--is-ancestor", &t.branch, &u.branch]).is_ok()).map(|u| u.id.clone())
    };
    let mut via: Vec<(String, String)> = Vec::new();
    for t in candidates.clone() {
        if let Some(u) = contained(t) {
            via.push((t.id.clone(), u));
            continue;
        }
        let ahead = git::git(&e.root, &["rev-list", "--count", &format!("HEAD..{}", t.branch)]).unwrap_or_default();
        if ahead.trim() == "0" {
            say!("= {} already merged", t.id);
            continue;
        }
        let msg = format!("Merge musher task {} (run {})", t.id, st.id);
        match git::git(&e.root, &["merge", "--no-ff", "-m", &msg, &t.branch]) {
            Ok(_) => {
                let carried: Vec<&str> = via
                    .iter()
                    .map(|(x, _)| x.as_str())
                    .filter(|x| candidates.iter().any(|c| c.id == *x && git::git(&e.root, &["merge-base", "--is-ancestor", &c.branch, &t.branch]).is_ok()))
                    .collect();
                let with = if carried.is_empty() { String::new() } else { format!(" (includes {})", carried.join(", ")) };
                say!("✓ merged {}{with}", t.id);
                merged += 1;
            }
            Err(err) => {
                let files = git::git(&e.root, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default().replace('\n', ", ");
                let _ = git::git(&e.root, &["merge", "--abort"]);
                if files.is_empty() {
                    // Not a conflict: signing, hooks, permissions… show git's own words.
                    let why = err.split_once(": ").map(|(_, r)| r).unwrap_or(&err);
                    eprintln!("✗ {}: git could not create the merge commit: {}", t.id, agent::clip(why, 160));
                    eprintln!("  your branch is clean again (merge aborted). Fix the git setting, then `musher merge` again.");
                    return 1;
                }
                eprintln!("✗ {} conflicts with your branch ({files})", t.id);
                eprintln!("  your branch is clean again (merge aborted).");
                eprintln!("  `musher merge --resolve` lets an agent integrate all finished tasks on a branch, verified by their checks, then merges that.");
                return 1;
            }
        }
    }
    if merged == 0 {
        say!("nothing to merge");
    }
    0
}

/// Add an `integrate` task that stacks every finished branch (agents resolve conflicts),
/// verify it with the union of the tasks' checks, then merge just that branch.
fn merge_resolve(e: &Env, args: &Args, dir: &Path) -> i32 {
    let st = load_state(dir).unwrap_or_else(|err| die(&err));
    if st.isolation != "worktree" {
        die("this run edited the repo directly — nothing to merge");
    }
    if !st.finished && pid_alive(st.pid) {
        die("run is still in progress");
    }
    if git::is_dirty(&e.root) {
        die("your working tree has uncommitted changes — commit or stash them first");
    }
    let mut plan = load_run_plan(dir);
    let done: Vec<String> = st.tasks.iter().filter(|t| t.status == Status::Done && !t.id.starts_with("integrate")).map(|t| t.id.clone()).collect();
    if done.is_empty() {
        die("no finished tasks to integrate");
    }
    let mut id = "integrate".to_string();
    let mut n = 2;
    while st.task(&id).is_some() {
        id = format!("integrate-{n}");
        n += 1;
    }
    let mut checks: Vec<String> = Vec::new();
    for t in plan.tasks.iter().filter(|t| done.contains(&t.id)) {
        if let Some(c) = &t.check {
            if !checks.contains(c) {
                checks.push(c.clone());
            }
        }
    }
    let check = (!checks.is_empty()).then(|| checks.join(" && "));
    // Same agent/model as the tasks being integrated, when they agree (a cheap model stays cheap).
    let done_tasks: Vec<&Task> = plan.tasks.iter().filter(|t| done.contains(&t.id)).collect();
    let common = |pick: fn(&Task) -> Option<String>| -> Option<String> {
        let first = done_tasks.first().and_then(|t| pick(t));
        done_tasks.iter().all(|t| pick(t) == first).then_some(first).flatten()
    };
    let agent = common(|t| t.agent.clone());
    let model = common(|t| t.model.clone());
    plan.tasks.push(Task {
        id: id.clone(),
        prompt: format!(
            "Integrate the finished work of tasks {}. Their branches are merged into yours (any conflicts were resolved while stacking). \
             Make sure everything works together: every task's behaviour must survive. Fix integration problems only; add nothing new. \
             Verify with the check command exactly as given, nothing appended.",
            done.join(", ")
        ),
        agent: agent.clone(),
        after: done.clone(),
        check,
        retries: None,
        timeout: None,
        permission: None,
        model,
        allow: vec![],
    });
    apply_overrides(&mut plan, args);
    plan.validate().unwrap_or_else(|err| die(&err));
    fs::write(dir.join("plan.json"), serde_json::to_vec_pretty(&plan).unwrap()).ok();
    let run = Arc::new(Run::open(dir.to_path_buf()).unwrap_or_else(|err| die(&err)));
    run.update(|s| {
        s.finished = false;
        s.ended = 0;
        s.tasks.push(TaskState {
            id: id.clone(),
            agent: agent.clone().unwrap_or_else(|| plan.settings.agent.clone()),
            status: Status::Queued,
            after: done.clone(),
            branch: String::new(),
            worktree: String::new(),
            start: String::new(),
            session: String::new(),
            attempts: 0,
            started: 0,
            ended: 0,
            cost_usd: 0.0,
            tokens: 0,
            activity: String::new(),
            note: String::new(),
            check: None,
            files_changed: 0,
            pid: 0,
        });
    });
    run.event(&id, "integrate", &done.join(", "));
    if drive(e, plan, run.clone(), args) != 0 {
        eprintln!("integration did not pass — see `musher logs {id}`; nothing was merged");
        return 1;
    }
    let t = run.snapshot().task(&id).cloned().unwrap();
    let msg = format!("Merge musher tasks {} (run {}, integrated)", done.join(", "), st.id);
    match git::git(&e.root, &["merge", "--no-ff", "-m", &msg, &t.branch]) {
        Ok(_) => {
            say!("✓ merged {} via {id}", done.join(", "));
            0
        }
        Err(err) => {
            let _ = git::git(&e.root, &["merge", "--abort"]);
            eprintln!("✗ your branch moved since the run started and conflicts with {id}: {}", agent::clip(&err, 160));
            1
        }
    }
}

fn cmd_mem(args: &Args) -> i32 {
    let e = env();
    fs::create_dir_all(&e.home).ok();
    let path = e.home.join("memory.md");
    if args.has("--edit") {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
        return Command::new(editor).arg(&path).status().map(|s| s.code().unwrap_or(1)).unwrap_or(1);
    }
    if args.has("--clear") {
        fs::write(&path, "").ok();
        println!("team notes cleared");
        return 0;
    }
    let note = args.pos.join(" ");
    if note.trim().is_empty() {
        let cur = fs::read_to_string(&path).unwrap_or_default();
        if cur.trim().is_empty() {
            println!("no team notes yet — `musher mem \"Use pnpm, not npm\"` adds one; every agent prompt includes them");
        } else {
            print!("{cur}");
        }
        return 0;
    }
    let mut cur = fs::read_to_string(&path).unwrap_or_default();
    if !cur.is_empty() && !cur.ends_with('\n') {
        cur.push('\n');
    }
    cur.push_str(&format!("- {}\n", note.trim()));
    fs::write(&path, cur).unwrap_or_else(|err| die(&err.to_string()));
    println!("noted");
    0
}

fn cmd_clean(args: &Args) -> i32 {
    let e = env();
    let purge = args.has("--purge");
    let mut n = 0;
    for dir in list_runs(&e.home) {
        let Ok(st) = load_state(&dir) else { continue };
        if !st.finished && pid_alive(st.pid) {
            continue;
        }
        if let Some(id) = args.get("--run") {
            if st.id != id {
                continue;
            }
        }
        for t in &st.tasks {
            if !t.worktree.is_empty() && Path::new(&t.worktree).exists() {
                let _ = git::git(&e.root, &["worktree", "remove", "--force", &t.worktree]);
                n += 1;
            }
            if purge && !t.branch.is_empty() {
                let _ = git::git(&e.root, &["branch", "-D", &t.branch]);
            }
        }
        let _ = fs::remove_dir_all(e.home.join("wt").join(&st.id));
        if purge {
            let _ = fs::remove_dir_all(&dir);
        }
    }
    if e.is_git {
        let _ = git::git(&e.root, &["worktree", "prune"]);
    }
    println!("removed {n} worktrees{}", if purge { ", branches and run history" } else { " (branches kept — `musher merge` still works)" });
    0
}

fn cmd_doctor() -> i32 {
    let e = env();
    let ok = |b: bool| if b { "✓" } else { "✗" };
    let ver = |bin: &str, flag: &str| {
        Command::new(bin)
            .arg(flag)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").trim().to_string())
    };
    println!("musher {VERSION}");
    let g = ver("git", "--version");
    println!("{} git        {}", ok(g.is_some()), g.unwrap_or_else(|| "not found (required)".into()));
    println!("{} repository {}", ok(e.is_git), if e.is_git { e.root.display().to_string() } else { "not a git repo (worktree isolation unavailable)".into() });
    if e.is_git {
        println!("{} commits    {}", ok(git::head(&e.root).is_ok()), if git::head(&e.root).is_ok() { "HEAD found" } else { "no commits yet" });
    }
    let toml = e.root.join("musher.toml");
    match (toml.exists(), Plan::load(&toml)) {
        (false, _) => println!("· musher.toml missing (`musher init`)"),
        (true, Ok(p)) => println!("✓ musher.toml {} tasks, agent {}", p.tasks.len(), p.settings.agent),
        (true, Err(err)) => println!("✗ musher.toml {}", err.trim_start_matches(&format!("{}: ", toml.display()))),
    }
    // Custom agents from musher.toml: is the program there?
    if let Ok(p) = Plan::load(&toml) {
        for (name, a) in p.agents.iter().filter(|(n, _)| !plan::builtin_agents().contains_key(*n)) {
            let found = Command::new("sh").arg("-c").arg(format!("command -v {}", a.cmd[0])).output().map(|o| o.status.success()).unwrap_or(false);
            println!("{} agent {:<4} {}", ok(found), name, if found { format!("{} found", a.cmd[0]) } else { format!("{} not found in PATH", a.cmd[0]) });
        }
    }
    for (bin, flag) in [("claude", "--version"), ("codex", "--version"), ("gemini", "--version"), ("gh", "--version")] {
        let v = ver(bin, flag);
        let miss = if bin == "gh" { "not installed (optional, for `musher pr`)" } else { "not installed" };
        println!("{} {:<10} {}", if v.is_some() { "✓" } else { "·" }, bin, v.unwrap_or_else(|| miss.into()));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{extract_toml, Args};

    #[test]
    fn dash_f_is_follow_only_for_log_commands() {
        let raw: Vec<String> = ["api", "-f"].iter().map(|s| s.to_string()).collect();
        let a = Args::parse(&raw, true);
        assert!(a.has("-f") && a.pos == vec!["api".to_string()]);
        let raw: Vec<String> = ["-f", "plan.toml", "--bg"].iter().map(|s| s.to_string()).collect();
        let a = Args::parse(&raw, false);
        assert_eq!(a.get("-f"), Some("plan.toml"));
        assert!(a.has("--bg"));
    }

    #[test]
    fn plan_output_shapes() {
        let bare = "[[task]]\nid = \"a\"\nprompt = \"x\"\n";
        assert_eq!(extract_toml(bare), bare);
        let fenced = format!("Here is the plan:\n```toml\n[settings]\nparallel = 2\n\n{bare}```\nDone.");
        assert_eq!(extract_toml(&fenced), bare);
        let prose = format!("Sure.\n[settings]\nagent = \"codex\"\n{bare}");
        assert_eq!(extract_toml(&prose), bare);
        assert_eq!(extract_toml("no plan here"), "");
    }
}
