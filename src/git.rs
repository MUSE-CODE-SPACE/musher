//! Thin wrappers over the git CLI.

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

/// git takes repo-wide locks for worktree/branch operations; serialise ours.
pub static LOCK: Mutex<()> = Mutex::new(());

pub fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_TERMINAL_PROMPT", "0").output().map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let err = if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err };
        Err(format!("git {}: {}", args.join(" "), err))
    }
}

pub fn root(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "--show-toplevel"]).ok()
}

pub fn head(dir: &Path) -> Result<String, String> {
    git(dir, &["rev-parse", "HEAD"]).map_err(|_| "the repository has no commits yet — make an initial commit first".to_string())
}

pub fn current_ref(dir: &Path) -> String {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "HEAD".into())
}

pub fn is_dirty(dir: &Path) -> bool {
    git(dir, &["status", "--porcelain", "--untracked-files=no"]).map(|s| !s.is_empty()).unwrap_or(false)
}

pub fn branch_exists(dir: &Path, branch: &str) -> bool {
    git(dir, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok()
}

/// Stage and commit everything the agent left behind. Returns true if a commit was made.
pub fn commit_all(dir: &Path, msg: &str) -> Result<bool, String> {
    git(dir, &["add", "-A"])?;
    if git(dir, &["diff", "--cached", "--quiet"]).is_ok() {
        return Ok(false);
    }
    git(dir, &["-c", "user.name=musher", "-c", "user.email=musher@localhost", "commit", "--no-verify", "-q", "-m", msg])?;
    Ok(true)
}

pub fn files_changed(dir: &Path, base: &str) -> u32 {
    git(dir, &["diff", "--name-only", &format!("{base}..HEAD")]).map(|s| s.lines().filter(|l| !l.is_empty()).count() as u32).unwrap_or(0)
}

/// Keep `.musher/` out of `git status` without touching the user's .gitignore.
pub fn exclude_musher_dir(root: &Path) {
    let Ok(common) = git(root, &["rev-parse", "--git-common-dir"]) else { return };
    let common = if Path::new(&common).is_absolute() { common.into() } else { root.join(common) };
    let info = common.join("info");
    let excl = info.join("exclude");
    let cur = std::fs::read_to_string(&excl).unwrap_or_default();
    if !cur.lines().any(|l| l.trim() == "/.musher/" || l.trim() == ".musher/") {
        let _ = std::fs::create_dir_all(&info);
        let sep = if cur.is_empty() || cur.ends_with('\n') { "" } else { "\n" };
        let _ = std::fs::write(&excl, format!("{cur}{sep}/.musher/\n"));
    }
}
