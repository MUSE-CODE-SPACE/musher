//! Turning an agent spec into argv, and agent output into structured signals.
//!
//! Herdr infers state by scraping the screen. Musher runs agents headless and reads their
//! structured event stream (Claude `stream-json`, Codex `--json`, Gemini `stream-json`),
//! so session ids, cost, tool activity and the final answer are facts, not guesses.

use serde_json::Value;

pub struct Vars<'a> {
    pub prompt: &'a str,
    pub session: &'a str,
    pub cwd: &'a str,
    pub permission: &'a str,
    pub model: &'a str,
    pub allow: &'a str,
}

/// Expand placeholders. `{model:--model}` becomes `--model <m>` when a model is set and
/// disappears otherwise; `{session}` args are dropped when there is no session.
pub fn build_argv(template: &[String], v: &Vars) -> Vec<String> {
    let mut out = Vec::with_capacity(template.len());
    for arg in template {
        let optional = [("{model:", v.model), ("{allow:", v.allow)];
        if let Some((flag, val)) = optional.iter().find_map(|(p, val)| arg.strip_prefix(p).and_then(|r| r.strip_suffix('}')).map(|f| (f, *val))) {
            if !val.is_empty() {
                out.push(flag.to_string());
                out.push(val.to_string());
            }
            continue;
        }
        if arg == "{session}" && v.session.is_empty() {
            // Also drop a preceding flag such as --session-id / --resume.
            if out.last().is_some_and(|p: &String| p.starts_with('-')) {
                out.pop();
            }
            continue;
        }
        out.push(expand(arg, &[("{prompt}", v.prompt), ("{session}", v.session), ("{cwd}", v.cwd), ("{permission}", v.permission)]));
    }
    out
}

/// Substitute placeholders in one pass over the template, so text that was inserted (a prompt
/// that itself mentions `{cwd}`) is never scanned again.
fn expand(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while !rest.is_empty() {
        let hit = vars.iter().filter_map(|(k, v)| rest.find(k).map(|i| (i, *k, *v))).min_by_key(|(i, _, _)| *i);
        match hit {
            Some((i, k, v)) => {
                out.push_str(&rest[..i]);
                out.push_str(v);
                rest = &rest[i + k.len()..];
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// What one line of agent output told us.
#[derive(Debug, Default, PartialEq)]
pub struct Signal {
    pub session: Option<String>,
    /// Short human-readable "what is it doing right now".
    pub activity: Option<String>,
    /// Assistant prose (latest wins).
    pub text: Option<String>,
    /// Final answer of the turn, if the agent reports one explicitly.
    pub result: Option<String>,
    pub cost_usd: Option<f64>,
    pub tokens: Option<u64>,
    pub is_error: bool,
    /// The agent wanted to do something it was not permitted to.
    pub denied: Vec<String>,
}

pub fn parse_line(line: &str) -> Signal {
    let line = line.trim();
    let mut s = Signal::default();
    if line.is_empty() {
        return s;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        s.activity = Some(clip(line, 90));
        s.text = Some(line.to_string());
        return s;
    };
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");

    if let Some(id) = str_at(&v, &["session_id"]).or_else(|| str_at(&v, &["thread_id"])) {
        s.session = Some(id);
    }

    match ty {
        // Claude Code stream-json
        "assistant" => {
            for c in v.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                match c.get("type").and_then(Value::as_str) {
                    Some("text") => s.text = str_at(c, &["text"]),
                    Some("tool_use") => s.activity = Some(describe_tool(c)),
                    _ => {}
                }
            }
        }
        "result" => {
            s.result = str_at(&v, &["result"]);
            s.cost_usd = v.get("total_cost_usd").and_then(Value::as_f64);
            s.tokens = usage_tokens(v.get("usage"));
            s.is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            for d in v.get("permission_denials").and_then(Value::as_array).into_iter().flatten() {
                s.denied.push(str_at(d, &["tool_name"]).unwrap_or_else(|| "tool".into()));
            }
        }
        // Codex exec --json
        "item.started" | "item.completed" | "item.updated" => {
            let item = v.get("item").unwrap_or(&Value::Null);
            match item.get("type").and_then(Value::as_str) {
                Some("agent_message") => {
                    s.text = str_at(item, &["text"]);
                    if ty == "item.completed" {
                        s.result = s.text.clone();
                    }
                }
                Some("command_execution") => s.activity = str_at(item, &["command"]).map(|c| format!("$ {}", clip(&c, 80))),
                Some("file_change") => {
                    let files: Vec<String> = item
                        .get("changes")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|c| str_at(c, &["path"]))
                        .map(|p| short_path(&p))
                        .collect();
                    s.activity = Some(format!("edit {}", files.join(", ")));
                }
                Some("reasoning") => s.activity = Some("thinking".into()),
                Some(other) => s.activity = Some(other.replace('_', " ")),
                None => {}
            }
        }
        "turn.completed" => s.tokens = usage_tokens(v.get("usage")),
        "turn.failed" | "error" => {
            s.is_error = true;
            s.text = str_at(&v, &["message"]).or_else(|| v.pointer("/error/message").and_then(Value::as_str).map(String::from));
        }
        // Gemini CLI stream-json
        "message" if v.get("role").and_then(Value::as_str) == Some("assistant") => s.text = str_at(&v, &["content"]),
        "tool_use" => s.activity = Some(describe_tool(&v)),
        _ => {}
    }
    s
}

fn describe_tool(c: &Value) -> String {
    let name = str_at(c, &["name"]).or_else(|| str_at(c, &["tool_name"])).unwrap_or_else(|| "tool".into());
    let input = c.get("input").or_else(|| c.get("parameters")).unwrap_or(&Value::Null);
    let detail = ["file_path", "path", "command", "pattern", "url", "description"].iter().find_map(|k| str_at(input, &[k])).map(|d| {
        if d.contains('/') && !d.contains(' ') {
            short_path(&d)
        } else {
            d
        }
    });
    match detail {
        Some(d) => clip(&format!("{name} {d}"), 90),
        None => name,
    }
}

fn usage_tokens(u: Option<&Value>) -> Option<u64> {
    let u = u?;
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let total = n("input_tokens") + n("output_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens") + n("cached_input_tokens");
    (total > 0).then_some(total)
}

fn str_at(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str)).filter(|s| !s.is_empty()).map(String::from)
}

fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.trim_end_matches('/').rsplitn(3, '/').collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[1], parts[0])
    } else {
        p.to_string()
    }
}

pub fn clip(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        one_line.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn argv_placeholders() {
        let vars = Vars { prompt: "do it", session: "", cwd: "/w", permission: "plan", model: "", allow: "" };
        let argv = build_argv(&s(&["claude", "-p", "{prompt}", "--resume", "{session}", "{model:--model}", "--permission-mode", "{permission}"]), &vars);
        assert_eq!(argv, s(&["claude", "-p", "do it", "--permission-mode", "plan"]));
        let vars = Vars { model: "opus", session: "abc", ..vars };
        let argv = build_argv(&s(&["x", "--resume", "{session}", "{model:-m}"]), &vars);
        assert_eq!(argv, s(&["x", "--resume", "abc", "-m", "opus"]));
        let vars = Vars { allow: "Bash(make test)", ..vars };
        assert_eq!(build_argv(&s(&["x", "{allow:--allowedTools}"]), &vars), s(&["x", "--allowedTools", "Bash(make test)"]));
    }

    #[test]
    fn placeholders_inside_the_prompt_survive() {
        let vars = Vars { prompt: "mention {cwd} and {session} and {prompt}", session: "S", cwd: "/w", permission: "p", model: "", allow: "" };
        let argv = build_argv(&s(&["x", "{prompt}", "{cwd}"]), &vars);
        assert_eq!(argv, s(&["x", "mention {cwd} and {session} and {prompt}", "/w"]));
    }

    #[test]
    fn claude_stream() {
        let init = parse_line(r#"{"type":"system","subtype":"init","session_id":"s-1"}"#);
        assert_eq!(init.session.as_deref(), Some("s-1"));
        let tool = parse_line(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/src/main.rs"}}]}}"#);
        assert_eq!(tool.activity.as_deref(), Some("Edit src/main.rs"));
        let res = parse_line(
            r#"{"type":"result","is_error":false,"result":"Done.","total_cost_usd":0.25,"usage":{"input_tokens":10,"output_tokens":5},"permission_denials":[{"tool_name":"Bash"}]}"#,
        );
        assert_eq!(res.result.as_deref(), Some("Done."));
        assert_eq!(res.cost_usd, Some(0.25));
        assert_eq!(res.tokens, Some(15));
        assert_eq!(res.denied, vec!["Bash".to_string()]);
    }

    #[test]
    fn codex_stream() {
        assert_eq!(parse_line(r#"{"type":"thread.started","thread_id":"t-9"}"#).session.as_deref(), Some("t-9"));
        let cmd = parse_line(r#"{"type":"item.started","item":{"type":"command_execution","command":"cargo test"}}"#);
        assert_eq!(cmd.activity.as_deref(), Some("$ cargo test"));
        let msg = parse_line(r#"{"type":"item.completed","item":{"type":"agent_message","text":"All green."}}"#);
        assert_eq!(msg.result.as_deref(), Some("All green."));
    }

    #[test]
    fn plain_text_lines() {
        let p = parse_line("compiling   foo");
        assert_eq!(p.activity.as_deref(), Some("compiling foo"));
    }
}
