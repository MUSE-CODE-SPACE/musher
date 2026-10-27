mod agent;
mod git;
mod plan;
mod run;
mod state;
mod ui;

use plan::{Plan, Task};
use run::{Ctx, Observer};
use state::{list_runs, load_state, now, Run, RunState, Status, TaskState};
use std::collections::HashMap;
use std::fs;
use std::io::{IsTerminal, Read, Seek, SeekFrom};
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

  musher watch [run]                live dashboard from any terminal
  musher status [run] [--json]      snapshot of a run
  musher wait [run]                 block until a run finishes (exit 0 = all done)
  musher ls                         list runs
  musher logs <task> [-f]           raw agent + check output
  musher events [-f] [--json]       durable event log

  musher diff <task> [--stat]       what a task changed (--all: including stacked deps)
  musher ask <task> \"<message>\"     follow up in the task's agent session, then re-check
  musher attach <task>              open the task's session interactively in its worktree
  musher merge [task...]            merge finished task branches into your current branch
  musher mem [note...]              show or append shared team notes (injected into prompts)
  musher clean [--purge]            remove worktrees of finished runs (--purge: branches + history)
  musher doctor                     check git and installed agents

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

const VALUE_FLAGS: &[&str] = &["-f", "-j", "--only", "--run", "--agent", "--check", "--id", "--retries", "--run-id", "--timeout", "--model", "--permission"];

impl Args {
    fn parse(raw: &[String]) -> Args {
        let (mut pos, mut flags) = (Vec::new(), HashMap::new());
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            if let Some((k, v)) = a.split_once('=').filter(|(k, _)| k.starts_with("--")) {
                flags.insert(k.to_string(), v.to_string());
            } else if VALUE_FLAGS.contains(&a.as_str()) {
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
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = raw.first().cloned() else {
        print!("{HELP}");
        return;
    };
    let args = Args::parse(&raw[1..]);
    if args.has("--no-notify") {
        ui::set_notify(false);
    }
    let code = match cmd.as_str() {
        "init" => cmd_init(),
        "run" => cmd_run(&args, &raw),
        "do" => cmd_do(&args, &raw),
        "resume" => cmd_resume(&args, &raw),
        "watch" => cmd_watch(&args),
        "status" | "st" => cmd_status(&args),
        "wait" => cmd_wait(&args),
        "ls" | "runs" => cmd_ls(),
        "logs" | "log" => cmd_logs(&args),
        "events" => cmd_events(&args),
        "diff" => cmd_diff(&args),
        "ask" => cmd_ask(&args),
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
    let (base_commit, base_ref) = if plan.settings.isolation == "worktree" {
        if !e.is_git {
            die("worktree isolation needs a git repository (git init && git commit), or pass --no-worktree");
        }
        let head = git::head(&e.root).unwrap_or_else(|err| die(&err));
        if git::is_dirty(&e.root) {
            eprintln!("musher: note — uncommitted changes are not visible to agents (worktrees start from HEAD)");
        }
        git::exclude_musher_dir(&e.root);
        (head, git::current_ref(&e.root))
    } else {
        (String::new(), String::new())
    };
    let id = args.get("--run-id").map(String::from).unwrap_or_else(state::new_run_id);
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
    let mut obs: Box<dyn Observer> =
        if args.has("--no-ui") || !std::io::stdout().is_terminal() { Box::new(ui::Plain::new()) } else { Box::new(ui::Live::new()) };
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
        return go_background(&e, raw, &state::new_run_id());
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
        return go_background(&e, raw, &state::new_run_id());
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

fn find_run(e: &Env, args: &Args) -> PathBuf {
    if let Some(id) = args.get("--run") {
        let d = e.home.join("runs").join(id);
        if !d.join("state.json").exists() {
            die(&format!("no run {id:?} — see `musher ls`"));
        }
        return d;
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
        Some(id) => e.home.join("runs").join(id),
        None => find_run(&e, args),
    };
    let st = load_state(&dir).unwrap_or_else(|err| die(&err));
    if !st.finished && pid_alive(st.pid) {
        die(&format!("run {} is still running (pid {}) — `musher watch`", st.id, st.pid));
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
    run.update(|s| {
        s.finished = false;
        s.ended = 0;
        for t in s.tasks.iter_mut().filter(|t| t.status != Status::Done) {
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
    let dir = find_run(&e, args);
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
        Some(id) => e.home.join("runs").join(id),
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

fn cmd_ls() -> i32 {
    let e = env();
    let runs = list_runs(&e.home);
    if runs.is_empty() {
        println!("no runs yet");
    }
    for d in runs {
        if let Ok(st) = load_state(&d) {
            let done = st.tasks.iter().filter(|t| t.status == Status::Done).count();
            let state = if st.finished {
                if done == st.tasks.len() {
                    "done"
                } else {
                    "incomplete"
                }
            } else if pid_alive(st.pid) {
                "running"
            } else {
                "interrupted"
            };
            let names: Vec<&str> = st.tasks.iter().map(|t| t.id.as_str()).collect();
            println!("{}  {:<11} {}/{}  ${:.2}  {}", st.id, state, done, st.tasks.len(), st.cost(), agent::clip(&names.join(", "), 60));
        }
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
    let mut buf = String::new();
    let _ = f.read_to_string(&mut buf);
    print!("{}", pretty_log(&buf, args.has("--raw")));
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
            let t = Command::new("date").args(["-r", &ts.to_string(), "+%H:%M:%S"]).output().ok().filter(|o| o.status.success());
            let clock = t.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(|| ts.to_string());
            println!("{clock}  {:<10} {:<13} {}", v["task"].as_str().unwrap_or("-"), v["kind"].as_str().unwrap_or(""), v["msg"].as_str().unwrap_or(""));
        }
    };
    let text = fs::read_to_string(&path).unwrap_or_default();
    text.lines().for_each(print_line);
    if !args.has("-f") {
        return 0;
    }
    run::install_signal_handlers();
    let mut seen = text.len();
    while !run::STOP.load(Ordering::SeqCst) {
        let t = fs::read_to_string(&path).unwrap_or_default();
        if t.len() > seen {
            t[seen..].lines().for_each(print_line);
            seen = t.len();
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
    if !st.finished && pid_alive(st.pid) && t.status.is_active() {
        die(&format!("{} is still running — wait for it or stop the run first", t.id));
    }
    let mut plan = load_run_plan(&dir);
    apply_overrides(&mut plan, args);
    plan.tasks.retain(|x| x.id == t.id);
    let run = Arc::new(Run::open(dir).unwrap_or_else(|err| die(&err)));
    run.update(|s| s.pid = std::process::id());
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
    run.update(|s| s.finished = true);
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

fn cmd_merge(args: &Args) -> i32 {
    let e = env();
    let dir = find_run(&e, args);
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
        let ahead = git::git(&e.root, &["rev-list", "--count", &format!("HEAD..{}", t.branch)]).unwrap_or_default();
        if ahead.trim() == "0" {
            println!("= {} already merged", t.id);
            continue;
        }
        let msg = format!("Merge musher task {} (run {})", t.id, st.id);
        match git::git(&e.root, &["merge", "--no-ff", "-m", &msg, &t.branch]) {
            Ok(_) => {
                println!("✓ merged {} ({} files)", t.id, t.files_changed);
                merged += 1;
            }
            Err(err) => {
                eprintln!("✗ conflict merging {}:\n{err}\nresolve it, commit, then run `musher merge` again (or `git merge --abort`)", t.id);
                return 1;
            }
        }
    }
    if merged == 0 {
        println!("nothing to merge");
    }
    0
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
    println!("{} musher.toml{}", ok(e.root.join("musher.toml").exists()), if e.root.join("musher.toml").exists() { "" } else { " missing (`musher init`)" });
    for (bin, flag) in [("claude", "--version"), ("codex", "--version"), ("gemini", "--version")] {
        let v = ver(bin, flag);
        println!("{} {:<10} {}", if v.is_some() { "✓" } else { "·" }, bin, v.unwrap_or_else(|| "not installed".into()));
    }
    0
}
