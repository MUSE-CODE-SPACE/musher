//! musher.toml: settings, agent presets and the task graph.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSpec>,
    #[serde(default, rename = "task")]
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Max tasks running at once.
    pub parallel: usize,
    /// Default agent for tasks that do not name one.
    pub agent: String,
    /// "worktree" (one git worktree + branch per task) or "none" (run in the repo itself).
    pub isolation: String,
    /// Per-attempt timeout, e.g. "30m", "90s", "1h".
    pub timeout: String,
    /// Default number of fix attempts after a failed check.
    pub retries: u32,
    /// Default permission mode handed to the agent ({permission}).
    pub permission: String,
    /// Commit the agent's changes automatically after each attempt.
    pub auto_commit: bool,
    /// Desktop notification when a run finishes or a task needs you.
    pub notify: bool,
    /// Stop scheduling new tasks after the run spends this many USD (0 = no limit).
    pub budget_usd: f64,
    /// Command run once in every fresh worktree before the agent starts (e.g. "npm ci").
    pub setup: Option<String>,
    /// Extra tools the agent may use without asking ({allow}), e.g. ["Bash(npm run lint:*)"].
    /// A task's own `check` command is always allowed so the agent can verify itself.
    pub allow: Vec<String>,
    /// Shell command run when a task reaches a final state. Env: MUSHER_RUN, MUSHER_TASK,
    /// MUSHER_STATUS, MUSHER_NOTE, MUSHER_COST, MUSHER_BRANCH, MUSHER_SUMMARY (path).
    pub on_task: Option<String>,
    /// Shell command run once when the run ends. Env: MUSHER_RUN, MUSHER_STATUS (done|incomplete|stopped),
    /// MUSHER_COST, MUSHER_DONE, MUSHER_TOTAL, MUSHER_REPORT (path to the markdown report).
    pub on_finish: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            parallel: 3,
            agent: "claude".into(),
            isolation: "worktree".into(),
            timeout: "30m".into(),
            retries: 2,
            permission: "acceptEdits".into(),
            auto_commit: true,
            notify: true,
            budget_usd: 0.0,
            setup: None,
            allow: Vec::new(),
            on_task: None,
            on_finish: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    /// argv for a fresh attempt. Placeholders: {prompt} {session} {cwd} {permission},
    /// and optional `{model:<flag>}` / `{allow:<flag>}` that vanish when empty.
    pub cmd: Vec<String>,
    /// argv for a follow-up on an existing session (fix loop, `musher ask`).
    #[serde(default)]
    pub resume: Vec<String>,
    /// argv for jumping into the session interactively (`musher attach`).
    #[serde(default)]
    pub attach: Vec<String>,
    #[serde(default)]
    pub model: String,
    /// Send the task prompt verbatim: no musher preamble, notes or upstream context.
    /// Fix attempts re-run the same prompt with the failure in $MUSHER_FEEDBACK.
    #[serde(default)]
    pub raw: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub after: Vec<String>,
    /// Shell command that must exit 0 for the task to count as done.
    #[serde(default)]
    pub check: Option<String>,
    #[serde(default)]
    pub retries: Option<u32>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub allow: Vec<String>,
}

pub fn builtin_agents() -> BTreeMap<String, AgentSpec> {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut m = BTreeMap::new();
    m.insert(
        "claude".into(),
        AgentSpec {
            cmd: v(&[
                "claude",
                "-p",
                "{prompt}",
                "--session-id",
                "{session}",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "{permission}",
                "{allow:--allowedTools}",
                "{model:--model}",
            ]),
            resume: v(&[
                "claude",
                "-p",
                "{prompt}",
                "--resume",
                "{session}",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "{permission}",
                "{allow:--allowedTools}",
                "{model:--model}",
            ]),
            attach: v(&["claude", "--resume", "{session}"]),
            model: String::new(),
            raw: false,
        },
    );
    m.insert(
        "codex".into(),
        AgentSpec {
            cmd: v(&["codex", "exec", "--json", "--sandbox", "workspace-write", "--skip-git-repo-check", "{model:-m}", "{prompt}"]),
            resume: v(&["codex", "exec", "resume", "--json", "{session}", "{prompt}"]),
            attach: v(&["codex", "resume", "{session}"]),
            model: String::new(),
            raw: false,
        },
    );
    m.insert(
        "gemini".into(),
        AgentSpec {
            cmd: v(&["gemini", "-p", "{prompt}", "--output-format", "stream-json", "--yolo", "{model:-m}"]),
            resume: vec![],
            attach: v(&["gemini"]),
            model: String::new(),
            raw: false,
        },
    );
    // Any shell command as the "agent": great for scripted steps and for demos without API keys.
    m.insert("shell".into(), AgentSpec { cmd: v(&["sh", "-c", "{prompt}"]), resume: vec![], attach: vec![], model: String::new(), raw: true });
    m
}

impl Plan {
    pub fn load(path: &Path) -> Result<Plan, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Plan::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Plan, String> {
        let mut plan: Plan = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut agents = builtin_agents();
        agents.append(&mut plan.agents);
        plan.agents = agents;
        plan.validate()?;
        Ok(plan)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.settings.parallel == 0 {
            return Err("settings.parallel must be at least 1".into());
        }
        if !matches!(self.settings.isolation.as_str(), "worktree" | "none") {
            return Err(format!("settings.isolation must be \"worktree\" or \"none\", got {:?}", self.settings.isolation));
        }
        parse_duration(&self.settings.timeout)?;
        let mut seen = HashSet::new();
        for t in &self.tasks {
            if t.id.is_empty() || !t.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return Err(format!("task id {:?} must be non-empty and use only [A-Za-z0-9_-]", t.id));
            }
            if !seen.insert(t.id.as_str()) {
                return Err(format!("duplicate task id {:?}", t.id));
            }
            let agent = t.agent.as_deref().unwrap_or(&self.settings.agent);
            if !self.agents.contains_key(agent) {
                return Err(format!("task {:?} uses unknown agent {:?}", t.id, agent));
            }
            if let Some(d) = &t.timeout {
                parse_duration(d).map_err(|e| format!("task {:?}: {e}", t.id))?;
            }
        }
        for t in &self.tasks {
            for dep in &t.after {
                if !seen.contains(dep.as_str()) {
                    return Err(format!("task {:?} depends on unknown task {:?}", t.id, dep));
                }
            }
        }
        if let Some(cycle) = self.find_cycle() {
            return Err(format!("dependency cycle: {}", cycle.join(" -> ")));
        }
        Ok(())
    }

    fn find_cycle(&self) -> Option<Vec<String>> {
        let deps: HashMap<&str, &Vec<String>> = self.tasks.iter().map(|t| (t.id.as_str(), &t.after)).collect();
        // 0 = unvisited, 1 = on stack, 2 = done
        let mut mark: HashMap<&str, u8> = HashMap::new();
        fn visit<'a>(n: &'a str, deps: &HashMap<&'a str, &'a Vec<String>>, mark: &mut HashMap<&'a str, u8>, stack: &mut Vec<&'a str>) -> Option<Vec<String>> {
            match mark.get(n) {
                Some(1) => {
                    let i = stack.iter().position(|s| *s == n).unwrap_or(0);
                    let mut c: Vec<String> = stack[i..].iter().map(|s| s.to_string()).collect();
                    c.push(n.to_string());
                    return Some(c);
                }
                Some(2) => return None,
                _ => {}
            }
            mark.insert(n, 1);
            stack.push(n);
            for d in deps.get(n).map(|v| v.iter()).into_iter().flatten() {
                if let Some(c) = visit(d.as_str(), deps, mark, stack) {
                    return Some(c);
                }
            }
            stack.pop();
            mark.insert(n, 2);
            None
        }
        for t in &self.tasks {
            if let Some(c) = visit(t.id.as_str(), &deps, &mut mark, &mut Vec::new()) {
                return Some(c);
            }
        }
        None
    }

    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }
}

/// "90s", "15m", "1h30m", "45" (seconds).
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (mut total, mut num) = (0u64, String::new());
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num.parse().map_err(|_| format!("bad duration {s:?}"))?;
        num.clear();
        total += n * match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            _ => return Err(format!("bad duration {s:?} (use s/m/h)")),
        };
    }
    if !num.is_empty() {
        total += num.parse::<u64>().map_err(|_| format!("bad duration {s:?}"))?;
    }
    Ok(total)
}

pub const TEMPLATE: &str = r#"# musher.toml — drive a team of coding agents.
# Docs: https://github.com/MUSE-CODE-SPACE/musher

[settings]
parallel   = 3            # tasks running at once
agent      = "claude"     # claude | codex | gemini | shell | any [agents.<name>] you define
isolation  = "worktree"   # each task gets its own git worktree + branch
timeout    = "30m"        # per attempt
retries    = 2            # fix attempts after a failed check
permission = "acceptEdits"
# setup    = "npm ci"     # runs once in every fresh worktree
# budget_usd = 5.0        # stop scheduling new tasks after this spend
# allow    = ["Bash(npm run lint:*)"]   # extra tools allowed without asking (each task's check is always allowed)
# on_task   = "say \"$MUSHER_TASK $MUSHER_STATUS\""                 # hook after each task (env: MUSHER_TASK/STATUS/NOTE/COST)
# on_finish = "curl -sS -d \"musher: $MUSHER_STATUS $MUSHER_DONE/$MUSHER_TOTAL\" ntfy.sh/my-topic"   # hook when the run ends

[[task]]
id     = "api"
prompt = "Add a GET /health endpoint that returns {\"ok\": true}, with a test."
check  = "npm test"

[[task]]
id     = "docs"
prompt = "Document the /health endpoint in README.md."

[[task]]
id     = "client"
after  = ["api"]          # starts from api's branch, gets api's summary as context
prompt = "Add a healthCheck() helper to the client SDK that calls /health."
check  = "npm test"

# Custom agent example:
# [agents.aider]
# cmd = ["aider", "--yes", "--message", "{prompt}"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration("45").unwrap(), 45);
        assert!(parse_duration("3d").is_err());
    }

    #[test]
    fn template_is_valid() {
        let p = Plan::parse(TEMPLATE).unwrap();
        assert_eq!(p.tasks.len(), 3);
        assert!(p.agents.contains_key("codex"));
    }

    #[test]
    fn detects_cycles_and_unknown_deps() {
        let cyc = "[[task]]\nid='a'\nprompt='x'\nafter=['b']\n[[task]]\nid='b'\nprompt='y'\nafter=['a']\n";
        assert!(Plan::parse(cyc).unwrap_err().contains("cycle"));
        let unk = "[[task]]\nid='a'\nprompt='x'\nafter=['zz']\n";
        assert!(Plan::parse(unk).unwrap_err().contains("unknown task"));
        let dup = "[[task]]\nid='a'\nprompt='x'\n[[task]]\nid='a'\nprompt='y'\n";
        assert!(Plan::parse(dup).unwrap_err().contains("duplicate"));
    }

    #[test]
    fn rejects_typos() {
        assert!(Plan::parse("[[task]]\nid='a'\nprompt='x'\nchek='make'\n").is_err());
    }
}
