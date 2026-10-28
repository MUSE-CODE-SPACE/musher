//! The scheduler and the per-task lifecycle:
//! worktree → (stack dependency branches) → setup → agent → commit → check → fix loop → summary.

use crate::agent::{self, Vars};
use crate::git;
use crate::plan::{parse_duration, AgentSpec, Plan, Task};
use crate::state::{now, uuid_v4, Run, Status};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub static STOP: AtomicBool = AtomicBool::new(false);
/// Process groups of running agents/checks, so Ctrl-C can take the whole tree down.
static CHILDREN: Mutex<Vec<i32>> = Mutex::new(Vec::new());

extern "C" fn on_signal(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}

pub fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as extern "C" fn(i32) as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as extern "C" fn(i32) as libc::sighandler_t);
    }
}

pub fn kill_children() {
    for pg in CHILDREN.lock().unwrap().iter() {
        unsafe {
            libc::kill(-pg, libc::SIGTERM);
        }
    }
}

pub struct Ctx {
    pub root: PathBuf,
    pub home: PathBuf,
    pub plan: Plan,
    pub run: Arc<Run>,
}

/// What the scheduler loop reports back to the UI layer on every tick.
pub trait Observer {
    fn tick(&mut self, run: &Run);
    fn finish(&mut self, run: &Run);
}

/// Run every unfinished task in dependency order. Returns true if all tasks ended `done`.
pub fn execute(ctx: Arc<Ctx>, ui: &mut dyn Observer) -> bool {
    let (tx, rx) = mpsc::channel::<String>();
    let mut running: HashSet<String> = HashSet::new();
    let budget = ctx.plan.settings.budget_usd;
    let mut budget_hit = false;
    let worktrees = ctx.run.snapshot().isolation == "worktree";
    let root_before = if worktrees { git::git(&ctx.root, &["status", "--porcelain"]).unwrap_or_default() } else { String::new() };

    loop {
        if STOP.load(Ordering::SeqCst) {
            kill_children();
            break;
        }
        let snap = ctx.run.snapshot();
        if budget > 0.0 && !budget_hit && snap.cost() >= budget {
            budget_hit = true;
            ctx.run.event("", "budget", &format!("spent ${:.2} of ${budget:.2}; no new tasks will start", snap.cost()));
        }
        // Skip tasks whose dependencies can no longer succeed; launch tasks that are ready.
        for t in snap.tasks.iter().filter(|t| t.status == Status::Queued) {
            let deps: Vec<_> = t.after.iter().filter_map(|d| snap.task(d)).collect();
            if let Some(bad) = deps.iter().find(|d| d.status.is_final() && d.status != Status::Done) {
                ctx.run.set_status(&t.id, Status::Skipped, &format!("{} {}", bad.id, bad.status.as_str()));
                continue;
            }
            let ready = deps.iter().all(|d| d.status == Status::Done);
            if ready && !budget_hit && running.len() < ctx.plan.settings.parallel {
                running.insert(t.id.clone());
                ctx.run.set_status(&t.id, Status::Setup, "");
                let (ctx, tx, id) = (ctx.clone(), tx.clone(), t.id.clone());
                thread::spawn(move || {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_task(&ctx, &id, None)));
                    if r.is_err() {
                        ctx.run.set_status(&id, Status::Failed, "internal error (panic)");
                    }
                    let _ = tx.send(id);
                });
            }
        }
        ui.tick(&ctx.run);
        if running.is_empty() {
            let snap = ctx.run.snapshot();
            let launchable = !budget_hit && snap.tasks.iter().any(|t| t.status == Status::Queued);
            if !launchable {
                break;
            }
        }
        if let Ok(id) = rx.recv_timeout(Duration::from_millis(200)) {
            running.remove(&id);
            while let Ok(id) = rx.try_recv() {
                running.remove(&id);
            }
        }
    }

    // Wait for workers to notice the stop and mark themselves.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !running.is_empty() && Instant::now() < deadline {
        if let Ok(id) = rx.recv_timeout(Duration::from_millis(100)) {
            running.remove(&id);
        }
    }
    ctx.run.update(|s| {
        for t in s.tasks.iter_mut().filter(|t| !t.status.is_final()) {
            if STOP.load(Ordering::SeqCst) || t.status.is_active() {
                t.status = Status::Stopped;
                t.activity.clear();
            } else if budget_hit {
                t.status = Status::Skipped;
                t.note = "budget reached".into();
            }
        }
        s.ended = now();
        s.finished = true;
    });
    ctx.run.event("", if STOP.load(Ordering::SeqCst) { "stopped" } else { "finished" }, "");
    if worktrees {
        // Agents are told to stay in their worktree; say so if one did not.
        let after = git::git(&ctx.root, &["status", "--porcelain"]).unwrap_or_default();
        let new_lines: Vec<&str> = after.lines().filter(|l| !root_before.lines().any(|b| b == *l) && !l.ends_with("musher.toml")).collect();
        if !new_lines.is_empty() {
            let files: Vec<&str> = new_lines.iter().map(|l| l[3..].trim()).collect();
            ctx.run.event("", "root-changed", &files.join(", "));
            eprintln!(
                "musher: warning — files in your own working tree changed during the run: {} (agents should only touch their worktree; check `git diff`)",
                files.join(", ")
            );
        }
    }
    let snap = ctx.run.snapshot();
    let report = crate::report::write(&ctx.run.dir, &snap);
    if let Some(cmd) = &ctx.plan.settings.on_finish {
        let done = snap.tasks.iter().filter(|t| t.status == Status::Done).count();
        let env = vec![
            ("MUSHER_RUN", snap.id.clone()),
            ("MUSHER_STATUS", if STOP.load(Ordering::SeqCst) { "stopped".to_string() } else { crate::report::run_outcome(&snap).to_string() }),
            ("MUSHER_COST", format!("{:.2}", snap.cost())),
            ("MUSHER_DONE", done.to_string()),
            ("MUSHER_TOTAL", snap.tasks.len().to_string()),
            ("MUSHER_REPORT", report.to_string_lossy().to_string()),
        ];
        run_hook(&ctx, "", cmd, env);
    }
    ui.finish(&ctx.run);
    snap.tasks.iter().all(|t| t.status == Status::Done)
}

/// Fire a user hook and wait briefly; a slow hook keeps running on its own (its output goes to
/// hooks.log) so it never holds the scheduler. Failures are events, not errors.
fn run_hook(ctx: &Ctx, task: &str, cmd: &str, env: Vec<(&'static str, String)>) {
    let log = OpenOptions::new().create(true).append(true).open(ctx.run.dir.join("hooks.log")).ok();
    let (out, err) = match log {
        Some(f) => (Stdio::from(f.try_clone().unwrap()), Stdio::from(f)),
        None => (Stdio::null(), Stdio::null()),
    };
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd).current_dir(&ctx.root).stdin(Stdio::null()).stdout(out).stderr(err);
    c.env("MUSHER_ROOT", &ctx.root);
    for (k, v) in &env {
        c.env(k, v);
    }
    let hook = if task.is_empty() { "on_finish" } else { "on_task" };
    match c.spawn() {
        Ok(mut child) => {
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(st)) if !st.success() => {
                        ctx.run.event(task, "hook-failed", &format!("{hook} exited {}", st.code().unwrap_or(-1)));
                        break;
                    }
                    Ok(Some(_)) => break,
                    Ok(None) if start.elapsed() < Duration::from_secs(10) => thread::sleep(Duration::from_millis(50)),
                    _ => break,
                }
            }
        }
        Err(e) => ctx.run.event(task, "hook-failed", &format!("{hook}: {e}")),
    }
}

/// `on_task` hook for a task that just reached a final state.
fn task_hook(ctx: &Ctx, id: &str) {
    let Some(cmd) = &ctx.plan.settings.on_task else { return };
    let snap = ctx.run.snapshot();
    let Some(t) = snap.task(id) else { return };
    let env = vec![
        ("MUSHER_RUN", snap.id.clone()),
        ("MUSHER_TASK", id.to_string()),
        ("MUSHER_STATUS", t.status.as_str().to_string()),
        ("MUSHER_NOTE", t.note.clone()),
        ("MUSHER_COST", format!("{:.2}", t.cost_usd)),
        ("MUSHER_BRANCH", t.branch.clone()),
        ("MUSHER_SUMMARY", ctx.run.out_path(id).to_string_lossy().to_string()),
    ];
    run_hook(ctx, id, cmd, env);
}

fn agent_spec<'a>(ctx: &'a Ctx, task: &Task) -> (&'a str, &'a AgentSpec) {
    let name = task.agent.as_deref().unwrap_or(&ctx.plan.settings.agent);
    let (k, v) = ctx.plan.agents.get_key_value(name).expect("validated");
    (k.as_str(), v)
}

/// Full lifecycle of one task. `followup` is a message for `musher ask`.
pub fn run_task(ctx: &Ctx, id: &str, followup: Option<&str>) {
    run_task_inner(ctx, id, followup);
    if ctx.run.snapshot().task(id).is_some_and(|t| t.status.is_final()) {
        task_hook(ctx, id);
    }
}

fn run_task_inner(ctx: &Ctx, id: &str, followup: Option<&str>) {
    let task = ctx.plan.task(id).expect("task in plan").clone();
    let run = &ctx.run;
    let (_, spec) = agent_spec(ctx, &task);
    let mut log = Log::open(&run.log_path(id));

    let timeout = Duration::from_secs(parse_duration(task.timeout.as_deref().unwrap_or(&ctx.plan.settings.timeout)).unwrap_or(1800));
    let retries = task.retries.unwrap_or(ctx.plan.settings.retries);
    let permission = task.permission.clone().unwrap_or_else(|| ctx.plan.settings.permission.clone());
    let model = task.model.clone().unwrap_or_else(|| spec.model.clone());
    let mut allow: Vec<String> = ctx.plan.settings.allow.iter().chain(task.allow.iter()).cloned().collect();
    if let Some(c) = &task.check {
        // Exact match, plus anything that starts with it: agents like to append `&& echo OK`.
        allow.push(format!("Bash({c})"));
        allow.push(format!("Bash({c}:*)"));
    }
    let allow = allow.join(",");
    let inv = Invoke { permission: permission.clone(), model: model.clone(), allow: allow.clone(), timeout };

    let prepared = prepare_workdir(ctx, &task, &mut log).and_then(|(cwd, fresh)| {
        if fresh {
            if let Err(e) = stack_deps(ctx, &task, spec, &cwd, &inv, &mut log) {
                // Drop the half-stacked worktree so a resume starts this task cleanly.
                let _g = git::LOCK.lock().unwrap();
                let _ = git::git(&ctx.root, &["worktree", "remove", "--force", &cwd.to_string_lossy()]);
                let branch = run.snapshot().task(id).map(|t| t.branch.clone()).unwrap_or_default();
                let _ = git::git(&ctx.root, &["branch", "-D", &branch]);
                return Err(e);
            }
        }
        Ok(cwd)
    });
    let cwd = match prepared {
        Ok(p) => p,
        Err(e) => {
            log.line(&format!("!! {e}"));
            run.set_status(id, Status::Failed, &agent::clip(&e, 120));
            return;
        }
    };
    let cwd_s = cwd.to_string_lossy().to_string();
    let base = run.snapshot().task(id).map(|t| t.start.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| run.snapshot().base_commit);

    run.task(id, |t| {
        if t.started == 0 {
            t.started = now();
        }
        t.ended = 0;
        t.check = None;
    });

    let mut feedback: Option<String> = followup.map(String::from);
    let mut last_summary = String::new();
    let mut denied: Vec<String> = Vec::new();
    let fresh_prompt = if spec.raw { task.prompt.clone() } else { build_prompt(ctx, &task) };

    for attempt in 0..=retries {
        if STOP.load(Ordering::SeqCst) {
            run.set_status(id, Status::Stopped, "");
            return;
        }
        let session = run.snapshot().task(id).map(|t| t.session.clone()).unwrap_or_default();
        let (argv_t, prompt, session) = match &feedback {
            Some(fb) if !spec.resume.is_empty() && !session.is_empty() => (&spec.resume, fb.clone(), session),
            Some(_) if spec.raw => (&spec.cmd, fresh_prompt.clone(), new_session(spec)),
            Some(fb) => (&spec.cmd, format!("{fresh_prompt}\n\n## Feedback from the previous attempt\n{fb}"), new_session(spec)),
            None => (&spec.cmd, fresh_prompt.clone(), new_session(spec)),
        };
        if !session.is_empty() {
            run.task(id, |t| t.session = session.clone());
        }
        let status = if feedback.is_some() { Status::Fixing } else { Status::Working };
        run.task(id, |t| t.attempts += 1);
        let note = if attempt > 0 { format!("attempt {}", attempt + 1) } else { String::new() };
        run.set_status(id, status, &note);

        let argv = agent::build_argv(argv_t, &Vars { prompt: &prompt, session: &session, cwd: &cwd_s, permission: &permission, model: &model, allow: &allow });
        log.section(&format!("{} attempt {} — {}", status.as_str(), attempt + 1, shell_preview(&argv)));
        if !spec.raw {
            log.line(&format!("[prompt]\n{}\n[/prompt]", prompt.trim_end()));
        }
        let out = run_agent(ctx, id, &argv, &cwd, timeout, feedback.as_deref().unwrap_or(""), &mut log);
        if !out.summary.is_empty() {
            last_summary = out.summary.clone();
        }
        denied.extend(out.denied.iter().cloned());
        denied.sort();
        denied.dedup();

        if ctx.plan.settings.auto_commit && run.snapshot().isolation == "worktree" {
            // An agent that switched branches would leave its work (and any commits it made)
            // off the task branch. Point the task branch at whatever HEAD is now, files untouched.
            let branch = run.snapshot().task(id).map(|t| t.branch.clone()).unwrap_or_default();
            if !branch.is_empty() && git::current_ref(&cwd) != branch {
                log.line(&format!("!! agent left branch {branch} (now on {}); moving the task branch here", git::current_ref(&cwd)));
                run.event(id, "branch-restored", &git::current_ref(&cwd));
                let _ = git::git(&cwd, &["checkout", "-q", "-B", &branch]);
            }
            let n = run.snapshot().task(id).map(|t| t.attempts).unwrap_or(1);
            let msg = format!("musher({id}): attempt {n}\n\n{}", agent::clip(&last_summary, 400));
            match git::commit_all(&cwd, &msg) {
                Ok(true) => log.line("== committed changes"),
                Ok(false) => {}
                Err(e) => {
                    // Work that is not on the branch is invisible to diff, merge and pr: not "done".
                    log.line(&format!("!! commit failed: {e}"));
                    run.set_status(id, Status::Failed, &agent::clip(&format!("could not commit: {e}"), 120));
                    return;
                }
            }
        }
        if STOP.load(Ordering::SeqCst) {
            run.set_status(id, Status::Stopped, "");
            return;
        }
        if out.timed_out {
            let secs = timeout.as_secs();
            if attempt < retries {
                // Work so far is committed; the resumed session picks up where it stopped.
                run.event(id, "timeout", &format!("after {secs}s, retrying"));
                feedback = Some(format!("The previous attempt was stopped after {secs}s (time limit). Your changes so far are kept. Continue from where you left off and finish the task, keeping this attempt short."));
                continue;
            }
            run.set_status(id, Status::Failed, &format!("timed out after {secs}s ({} attempts)", attempt + 1));
            return;
        }
        if !out.ok {
            let why = if out.error.is_empty() { format!("agent exited with {}", out.code) } else { out.error.clone() };
            // The saved session no longer exists (storage cleared, other machine): start fresh
            // with the same feedback instead of retrying the dead session.
            let resumed = std::ptr::eq(argv_t, &spec.resume);
            if resumed && session_lost(&why) {
                log.line(&format!("!! session {session} is gone; starting a fresh session"));
                run.event(id, "session-lost", &agent::clip(&why, 100));
                run.task(id, |t| t.session.clear());
                if attempt < retries {
                    continue;
                }
            }
            if needs_human(&why) {
                // Quota, auth or billing: retrying only burns attempts. Hand it to a person.
                run.set_status(id, Status::Blocked, &agent::clip(&why, 120));
                return;
            }
            if attempt < retries {
                feedback = Some(format!("The previous attempt ended with an error: {why}\nPlease continue and finish the task."));
                continue;
            }
            run.set_status(id, Status::Failed, &agent::clip(&why, 120));
            return;
        }

        let Some(check) = task.check.as_deref() else {
            finish_done(ctx, id, &cwd, &base, &last_summary, &denied);
            return;
        };
        run.set_status(id, Status::Checking, "");
        log.section(&format!("check — {check}"));
        let (passed, code, tail) = run_check(id, check, &cwd, timeout, &mut log);
        run.task(id, |t| t.check = Some(passed));
        if passed {
            finish_done(ctx, id, &cwd, &base, &last_summary, &denied);
            return;
        }
        run.event(id, "check-failed", &format!("exit {code}"));
        if attempt < retries {
            feedback = Some(format!(
                "The verification command `{check}` failed (exit {code}). Last lines of its output:\n```\n{tail}\n```\nFix the code so this command passes. Do not weaken or delete the check itself."
            ));
            continue;
        }
        if !denied.is_empty() {
            run.set_status(id, Status::Blocked, &format!("check failing; agent was denied: {} — `musher attach {id}`", denied.join(", ")));
        } else {
            run.set_status(id, Status::Failed, &format!("check failed (exit {code}) after {} attempts", attempt + 1));
        }
        return;
    }
}

/// The agent could not find the session we asked it to resume.
fn session_lost(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    (e.contains("session") || e.contains("conversation") || e.contains("thread"))
        && (e.contains("not found") || e.contains("no conversation") || e.contains("does not exist") || e.contains("unknown") || e.contains("invalid session"))
}

/// Errors no retry can fix: the account, not the code, is the problem.
fn needs_human(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    [
        "usage limit",
        "rate limit",
        "quota",
        "insufficient credit",
        "billing",
        "purchase more",
        "unauthorized",
        "invalid api key",
        "not logged in",
        "please log in",
        "authentication",
        "401",
        "402",
        "429",
    ]
    .iter()
    .any(|p| e.contains(p))
}

/// `musher check <task>`: after hand edits in the worktree, commit them and re-run the check
/// without calling the agent. Returns true when the task is done.
pub fn recheck(ctx: &Ctx, id: &str) -> bool {
    let (ok, ran) = recheck_inner(ctx, id);
    // A check actually ran: tell the on_task hook, like any other attempt.
    if ran {
        task_hook(ctx, id);
    }
    ok
}

/// Returns (task is done, a check/commit actually happened).
fn recheck_inner(ctx: &Ctx, id: &str) -> (bool, bool) {
    let task = ctx.plan.task(id).expect("task in plan").clone();
    let run = &ctx.run;
    let mut log = Log::open(&run.log_path(id));
    let snap = run.snapshot();
    let t = snap.task(id).expect("task state");
    let cwd = if snap.isolation == "worktree" { PathBuf::from(&t.worktree) } else { ctx.root.clone() };
    if snap.isolation == "worktree" && !cwd.join(".git").exists() {
        // Nothing to check against; leave the task's status as it is.
        eprintln!("musher: {id}'s worktree is gone (musher clean?) — `musher resume` or `musher ask {id} \"…\"` recreates it from its branch");
        return (false, false);
    }
    let base = if t.start.is_empty() { snap.base_commit.clone() } else { t.start.clone() };
    log.section("check (manual, no agent)");
    if ctx.plan.settings.auto_commit && snap.isolation == "worktree" {
        match git::commit_all(&cwd, &format!("musher({id}): manual edits")) {
            Ok(true) => log.line("== committed manual edits"),
            Ok(false) => {}
            Err(e) => {
                run.set_status(id, Status::Failed, &agent::clip(&format!("could not commit: {e}"), 120));
                return (false, true);
            }
        }
    }
    let summary = fs::read_to_string(run.out_path(id)).unwrap_or_default();
    let Some(check) = task.check.as_deref() else {
        finish_done(ctx, id, &cwd, &base, &summary, &[]);
        return (true, true);
    };
    run.set_status(id, Status::Checking, "");
    let timeout = Duration::from_secs(parse_duration(task.timeout.as_deref().unwrap_or(&ctx.plan.settings.timeout)).unwrap_or(1800));
    let (passed, code, _) = run_check(id, check, &cwd, timeout, &mut log);
    run.task(id, |t| t.check = Some(passed));
    if passed {
        finish_done(ctx, id, &cwd, &base, &summary, &[]);
    } else {
        run.set_status(id, Status::Failed, &format!("check failed (exit {code}) — manual check"));
    }
    (passed, true)
}

fn finish_done(ctx: &Ctx, id: &str, cwd: &Path, base: &str, summary: &str, denied: &[String]) {
    let files = if ctx.run.snapshot().isolation == "worktree" { git::files_changed(cwd, base) } else { 0 };
    let _ = fs::write(ctx.run.out_path(id), summary);
    ctx.run.task(id, |t| t.files_changed = files);
    let note = if denied.is_empty() { String::new() } else { format!("denied: {}", denied.join(", ")) };
    ctx.run.set_status(id, Status::Done, &note);
}

fn new_session(spec: &AgentSpec) -> String {
    // Only agents that accept a session id up front get one from us; others report theirs.
    if spec.cmd.iter().any(|a| a == "{session}") {
        uuid_v4()
    } else {
        String::new()
    }
}

/// Returns the working directory and whether it was freshly created (and so needs stacking).
fn prepare_workdir(ctx: &Ctx, task: &Task, log: &mut Log) -> Result<(PathBuf, bool), String> {
    let snap = ctx.run.snapshot();
    if snap.isolation == "none" {
        return Ok((ctx.root.clone(), false));
    }
    let rid = &snap.id;
    let path = ctx.home.join("wt").join(rid).join(&task.id);
    let branch = format!("musher/{rid}/{}", task.id);
    ctx.run.task(&task.id, |t| {
        t.branch = branch.clone();
        t.worktree = path.to_string_lossy().to_string();
    });
    if path.join(".git").exists() {
        log.line(&format!("== reusing worktree {}", path.display()));
        return Ok((path, false));
    }
    {
        let _g = git::LOCK.lock().unwrap();
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let p = path.to_string_lossy();
        if git::branch_exists(&ctx.root, &branch) {
            git::git(&ctx.root, &["worktree", "add", "-q", &p, &branch])?;
        } else {
            git::git(&ctx.root, &["worktree", "add", "-q", "-b", &branch, &p, &snap.base_commit])?;
        }
        log.line(&format!("== worktree {} on {branch}", path.display()));
    }
    Ok((path, true))
}

/// Agent invocation settings shared by the main attempts and conflict resolution.
struct Invoke {
    permission: String,
    model: String,
    allow: String,
    timeout: Duration,
}

/// Merge dependency branches into a fresh worktree so the task builds on their work.
/// Branches developed in parallel can conflict; the task's own agent resolves that,
/// and musher refuses to continue while conflict markers remain.
fn stack_deps(ctx: &Ctx, task: &Task, spec: &AgentSpec, cwd: &Path, inv: &Invoke, log: &mut Log) -> Result<(), String> {
    let rid = ctx.run.id();
    let id = task.id.as_str();
    for dep in &task.after {
        let dep_branch = format!("musher/{rid}/{dep}");
        let merged = git::git(
            cwd,
            &["-c", "user.name=musher", "-c", "user.email=musher@localhost", "-c", "commit.gpgsign=false", "merge", "--no-ff", "--no-edit", "-q", &dep_branch],
        );
        if merged.is_ok() {
            log.line(&format!("== merged {dep_branch}"));
            continue;
        }
        let conflicted = git::git(cwd, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
        let files: Vec<&str> = conflicted.lines().filter(|l| !l.is_empty()).collect();
        if files.is_empty() || spec.raw {
            let _ = git::git(cwd, &["merge", "--abort"]);
            return Err(format!("merge conflict stacking {dep}: {}", agent::clip(&merged.unwrap_err(), 140)));
        }
        let list = files.join(", ");
        log.section(&format!("conflict merging {dep} — {list}"));
        ctx.run.event(id, "conflict", &format!("{dep}: {list}"));
        ctx.run.set_status(id, Status::Fixing, &format!("resolving conflict with {dep}"));
        let mut prompt = format!(
            "You are integrating upstream work before starting your own task.\n\
             Merging branch `{dep_branch}` (task `{dep}`, developed in parallel with the work already on this branch) \
             produced conflicts in: {list}.\n\
             Resolve every conflict so that BOTH sides keep working: combine the changes, do not drop either one. \
             Remove all conflict markers. Do not start any other work and do not commit — musher commits the merge.\n"
        );
        if let Some(c) = &task.check {
            prompt.push_str(&format!("You may verify with `{c}`, run exactly like that with nothing appended (other commands need approval).\n"));
        }
        let argv = agent::build_argv(
            &spec.cmd,
            &Vars {
                prompt: &prompt,
                session: &new_session(spec),
                cwd: &cwd.to_string_lossy(),
                permission: &inv.permission,
                model: &inv.model,
                allow: &inv.allow,
            },
        );
        log.line(&format!("[prompt]\n{}\n[/prompt]", prompt.trim_end()));
        let out = run_agent(ctx, id, &argv, cwd, inv.timeout, "", log);
        let markers = git::git(cwd, &["grep", "-n", "-I", "-E", "^(<<<<<<<|>>>>>>>)( |$)", "--", "."]).unwrap_or_default();
        if !out.ok || !markers.trim().is_empty() {
            let _ = git::git(cwd, &["merge", "--abort"]);
            let why = if out.ok { "conflict markers remain".to_string() } else { out.error };
            return Err(format!("could not resolve conflict with {dep}: {}", agent::clip(&why, 120)));
        }
        git::git(cwd, &["add", "-A"])?;
        git::git(
            cwd,
            &["-c", "user.name=musher", "-c", "user.email=musher@localhost", "-c", "commit.gpgsign=false", "commit", "--no-verify", "-q", "--no-edit"],
        )?;
        log.line(&format!("== resolved and merged {dep_branch}"));
        ctx.run.event(id, "resolved", dep);
    }
    let start = git::head(cwd)?;
    ctx.run.task(id, |t| t.start = start);
    if let Some(setup) = &ctx.plan.settings.setup {
        log.section(&format!("setup — {setup}"));
        let (ok, code, _) = run_check(id, setup, cwd, Duration::from_secs(1800), log);
        if !ok {
            return Err(format!("setup `{setup}` failed (exit {code})"));
        }
    }
    Ok(())
}

fn build_prompt(ctx: &Ctx, task: &Task) -> String {
    let snap = ctx.run.snapshot();
    let mut p = String::new();
    p.push_str("You are one agent in a team coordinated by musher.\n");
    if snap.isolation == "worktree" {
        let branch = snap.task(&task.id).map(|t| t.branch.as_str()).unwrap_or("");
        p.push_str(&format!(
            "- You work in an isolated git worktree on branch `{branch}`. Other agents work in parallel in their own worktrees.\n\
             - Do not push, do not switch branches, do not touch files outside this directory. Musher commits what you leave.\n"
        ));
    }
    if let Some(c) = &task.check {
        p.push_str(&format!(
            "- When you finish, musher verifies your work by running `{c}` (it must exit 0). You may run it yourself without asking, but only as that exact command: nothing appended (no `&& echo`, no `; echo $?`), or it will be blocked. If it fails afterwards, you get the output and another turn.\n"
        ));
    }
    p.push_str("- Finish the task fully, then reply with a short summary of what you changed and anything a follow-up task must know.\n");

    let memory = fs::read_to_string(ctx.home.join("memory.md")).unwrap_or_default();
    if !memory.trim().is_empty() {
        p.push_str("\n## Shared team notes\n");
        p.push_str(memory.trim());
        p.push('\n');
    }
    let upstream: Vec<(String, String)> =
        task.after.iter().filter_map(|d| fs::read_to_string(ctx.run.out_path(d)).ok().map(|s| (d.clone(), s))).filter(|(_, s)| !s.trim().is_empty()).collect();
    if !upstream.is_empty() {
        p.push_str("\n## Results from the tasks you build on (their code is already merged into your branch)\n");
        for (d, s) in upstream {
            p.push_str(&format!("### {d}\n{}\n", s.trim()));
        }
    }
    p.push_str("\n## Your task\n");
    p.push_str(task.prompt.trim());
    p.push('\n');
    p
}

struct AgentOut {
    ok: bool,
    code: i32,
    timed_out: bool,
    summary: String,
    error: String,
    denied: Vec<String>,
}

fn spawn_group(argv: &[String], cwd: &Path, ctx_env: &[(&str, &str)]) -> std::io::Result<std::process::Child> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    for (k, v) in ctx_env {
        cmd.env(k, v);
    }
    let child = cmd.spawn()?;
    CHILDREN.lock().unwrap().push(child.id() as i32);
    Ok(child)
}

fn reap(pid: u32) {
    CHILDREN.lock().unwrap().retain(|p| *p != pid as i32);
}

pub fn kill_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    thread::sleep(Duration::from_millis(300));
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn run_agent(ctx: &Ctx, id: &str, argv: &[String], cwd: &Path, timeout: Duration, feedback: &str, log: &mut Log) -> AgentOut {
    let run = ctx.run.clone();
    let rid = run.id();
    let root = ctx.root.to_string_lossy().to_string();
    let env = [("MUSHER_RUN", rid.as_str()), ("MUSHER_TASK", id), ("MUSHER_ROOT", root.as_str()), ("MUSHER_FEEDBACK", feedback)];
    let mut child = match spawn_group(argv, cwd, &env) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("cannot start `{}`: {e}", argv[0]);
            log.line(&format!("!! {msg}"));
            return AgentOut { ok: false, code: -1, timed_out: false, summary: String::new(), error: msg, denied: vec![] };
        }
    };
    let pid = child.id();
    run.task(id, |t| t.pid = pid);
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let shared_log = log.share();

    let err_log = shared_log.clone();
    let err_thread = thread::spawn(move || {
        let mut tail = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            Log::write_shared(&err_log, &format!("[stderr] {line}"));
            tail = line;
        }
        tail
    });

    let (tid, out_run) = (id.to_string(), run.clone());
    let out_thread = thread::spawn(move || {
        let (mut summary, mut text, mut error, mut denied, mut is_error) = (String::new(), String::new(), String::new(), Vec::new(), false);
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            Log::write_shared(&shared_log, &line);
            let s = agent::parse_line(&line);
            if let Some(r) = s.result {
                summary = r;
            }
            if let Some(t) = &s.text {
                if s.text_delta {
                    text.push_str(t);
                } else {
                    text = t.clone();
                }
            }
            if s.is_error {
                is_error = true;
                error = s.text.clone().unwrap_or_default();
            }
            denied.extend(s.denied);
            let (sess, act) = (s.session, s.activity);
            if sess.is_some() || act.is_some() || s.cost_usd.is_some() || s.tokens.is_some() {
                out_run.update_lazy(|st| {
                    if let Some(t) = st.task_mut(&tid) {
                        if let Some(x) = sess {
                            if t.session.is_empty() {
                                t.session = x;
                            }
                        }
                        if let Some(a) = act {
                            t.activity = a;
                        }
                    }
                });
            }
            if let Some(c) = s.cost_usd {
                out_run.task(&tid, |t| t.cost_usd += c);
            }
            if let Some(n) = s.tokens {
                out_run.task(&tid, |t| t.tokens += n);
            }
        }
        if summary.is_empty() {
            summary = text;
        }
        (summary, error, denied, is_error)
    });

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(_) => break None,
        }
        if STOP.load(Ordering::SeqCst) || start.elapsed() > timeout {
            timed_out = !STOP.load(Ordering::SeqCst);
            kill_group(pid);
            break child.wait().ok();
        }
        thread::sleep(Duration::from_millis(150));
    };
    reap(pid);
    run.task(id, |t| t.pid = 0);
    let (summary, mut error, denied, is_error) = out_thread.join().unwrap_or_default();
    let err_tail = err_thread.join().unwrap_or_default();
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    let ok = code == 0 && !is_error && !timed_out;
    if !ok && error.is_empty() {
        error = err_tail;
    }
    run.save();
    AgentOut { ok, code, timed_out, summary, error, denied }
}

/// Run a shell command; returns (passed, exit code, last lines of output).
fn run_check(id: &str, cmd: &str, cwd: &Path, timeout: Duration, log: &mut Log) -> (bool, i32, String) {
    let argv = vec!["sh".to_string(), "-c".to_string(), format!("{cmd} 2>&1")];
    let mut child = match spawn_group(&argv, cwd, &[("MUSHER_TASK", id)]) {
        Ok(c) => c,
        Err(e) => return (false, -1, e.to_string()),
    };
    let pid = child.id();
    let stdout = child.stdout.take().unwrap();
    let shared = log.share();
    let reader = thread::spawn(move || {
        let mut lines: Vec<String> = Vec::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            Log::write_shared(&shared, &line);
            lines.push(line);
            if lines.len() > 200 {
                lines.drain(..100);
            }
        }
        lines
    });
    let start = Instant::now();
    let status = loop {
        if let Ok(Some(st)) = child.try_wait() {
            break Some(st);
        }
        if STOP.load(Ordering::SeqCst) || start.elapsed() > timeout {
            kill_group(pid);
            break child.wait().ok();
        }
        thread::sleep(Duration::from_millis(100));
    };
    reap(pid);
    let lines = reader.join().unwrap_or_default();
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    let tail = lines[lines.len().saturating_sub(80)..].join("\n");
    log.line(&format!("== exit {code}"));
    (code == 0, code, tail)
}

fn shell_preview(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let a = agent::clip(a, 40);
            if a.contains(' ') {
                format!("'{a}'")
            } else {
                a
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Per-task raw log, shared between the output reader threads.
pub struct Log(Arc<Mutex<Option<File>>>);

impl Log {
    fn open(path: &Path) -> Log {
        let f = OpenOptions::new().create(true).append(true).open(path).ok();
        Log(Arc::new(Mutex::new(f)))
    }
    fn share(&self) -> Arc<Mutex<Option<File>>> {
        self.0.clone()
    }
    fn write_shared(f: &Arc<Mutex<Option<File>>>, line: &str) {
        if let Some(f) = f.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{line}");
        }
    }
    fn line(&mut self, line: &str) {
        Log::write_shared(&self.0, line);
    }
    fn section(&mut self, title: &str) {
        self.line(&format!("\n──── {title} ────"));
    }
}

#[cfg(test)]
mod tests {
    use super::needs_human;

    #[test]
    fn lost_sessions() {
        use super::session_lost;
        assert!(session_lost("No conversation found with session ID: 0000"));
        assert!(session_lost("thread abc does not exist"));
        assert!(!session_lost("tests failed"));
    }

    #[test]
    fn human_needed_errors() {
        assert!(needs_human("You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage"));
        assert!(needs_human("Error: Invalid API key"));
        assert!(needs_human("HTTP 429 Too Many Requests"));
        assert!(!needs_human("agent exited with 1"));
        assert!(!needs_human("tests failed: 3 errors"));
    }
}
