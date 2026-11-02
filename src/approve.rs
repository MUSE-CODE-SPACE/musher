//! Live permission approvals for headless agents.
//!
//! `claude -p` cannot show a dialog: a tool that needs permission is refused on the spot and the
//! agent usually gives up. Claude's `--permission-prompt-tool` hands that question to an MCP tool
//! instead, so musher serves one (`musher __approve`). It parks the request in a file under the
//! run directory and holds the agent until someone answers from any terminal. The runner sees the
//! file and marks the task `waiting`. Files again, so there is no server to keep alive.

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const TOOL: &str = "mcp__musher__approve";

fn perm_dir(run: &Path) -> PathBuf {
    run.join("perm")
}
fn req_path(run: &Path, task: &str) -> PathBuf {
    perm_dir(run).join(format!("{task}.req.json"))
}
fn ans_path(run: &Path, task: &str) -> PathBuf {
    perm_dir(run).join(format!("{task}.ans.json"))
}
fn always_path(run: &Path) -> PathBuf {
    perm_dir(run).join("always.txt")
}

/// Write-then-rename, so a reader never sees half a file.
fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, body)?;
    fs::rename(tmp, path)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: String,
    pub task: String,
    pub tool: String,
    /// One line for humans: the command, the file, or the raw input.
    pub what: String,
    pub key: String,
    pub since: u64,
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn describe(tool: &str, input: &Value) -> String {
    let field = ["command", "file_path", "url", "pattern", "query"].iter().find_map(|k| input.get(k).and_then(Value::as_str));
    match field {
        Some(s) => s.to_string(),
        None => format!("{tool} {input}"),
    }
}

pub fn pending(run: &Path, task: &str) -> Option<Request> {
    let v: Value = serde_json::from_str(&fs::read_to_string(req_path(run, task)).ok()?).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Some(Request {
        id: s("id"),
        task: task.to_string(),
        tool: s("tool"),
        what: s("what"),
        key: s("key"),
        since: v.get("since").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// Everything waiting for a human in this run, oldest first.
pub fn all_pending(run: &Path) -> Vec<Request> {
    let mut out: Vec<Request> = fs::read_dir(perm_dir(run))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".req.json")).and_then(|t| pending(run, t)))
        .collect();
    out.sort_by_key(|r| r.since);
    out
}

pub fn answer(run: &Path, req: &Request, allow: bool, always: bool, message: &str) -> std::io::Result<()> {
    if allow && always {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(always_path(run))?;
        writeln!(f, "{}", req.key)?;
    }
    write_atomic(&ans_path(run, &req.task), &json!({"id": req.id, "allow": allow, "message": message}).to_string())
}

/// The agent is gone, so nobody is waiting for these any more.
pub fn clear(run: &Path, task: &str) {
    let _ = fs::remove_file(req_path(run, task));
    let _ = fs::remove_file(ans_path(run, task));
}

fn always_allowed(run: &Path, key: &str) -> bool {
    fs::read_to_string(always_path(run)).is_ok_and(|s| s.lines().any(|l| l == key))
}

/// Single line keyed on the exact input: `--always` never widens to other commands.
fn key_of(tool: &str, input: &Value) -> String {
    format!("{tool} {input}").replace(['\n', '\r'], " ")
}

/// Park one request and hold the agent until it is answered or `limit` runs out.
fn decide(run: &Path, task: &str, tool: &str, input: &Value, limit: Duration) -> Value {
    let allow = json!({"behavior": "allow", "updatedInput": input});
    let key = key_of(tool, input);
    if always_allowed(run, &key) {
        return allow;
    }
    let id = format!("{}-{}", std::process::id(), now_ms());
    let body = json!({"id": id, "tool": tool, "what": describe(tool, input), "key": key, "since": now_ms() / 1000});
    let _ = fs::remove_file(ans_path(run, task));
    if let Err(e) = write_atomic(&req_path(run, task), &body.to_string()) {
        return json!({"behavior": "deny", "message": format!("musher could not ask for approval: {e}")});
    }
    let start = Instant::now();
    loop {
        if let Some(v) = fs::read_to_string(ans_path(run, task)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
            if v.get("id").and_then(Value::as_str) == Some(id.as_str()) {
                clear(run, task);
                if v.get("allow").and_then(Value::as_bool).unwrap_or(false) {
                    return allow;
                }
                let m = v.get("message").and_then(Value::as_str).filter(|m| !m.is_empty()).unwrap_or("Denied by a person through musher.");
                return json!({"behavior": "deny", "message": m});
            }
        }
        if start.elapsed() > limit {
            clear(run, task);
            return json!({"behavior": "deny", "message": "Nobody answered this permission request in time. Do not work around it; report what you could not do."});
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `--mcp-config` value that points the agent back at this binary.
pub fn mcp_config(run: &Path, task: &str, limit_secs: u64) -> String {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| "musher".into());
    json!({"mcpServers": {"musher": {"command": exe, "args": ["__approve", run.to_string_lossy(), task, limit_secs.to_string()]}}}).to_string()
}

/// Minimal MCP stdio server with one tool. Requests arrive one per line.
pub fn serve(run: &Path, task: &str, limit: Duration) -> i32 {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        let Some(id) = v.get("id").cloned() else { continue }; // notifications get no reply
        let result = match v.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => json!({
                "protocolVersion": v.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2024-11-05")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "musher", "version": env!("CARGO_PKG_VERSION")},
            }),
            "tools/list" => json!({"tools": [{
                "name": "approve",
                "description": "Ask the person running musher to allow or deny a tool call.",
                "inputSchema": {"type": "object", "properties": {"tool_name": {"type": "string"}, "input": {"type": "object"}, "tool_use_id": {"type": "string"}}, "required": ["tool_name", "input"]},
            }]}),
            "tools/call" => {
                let a = v.pointer("/params/arguments").cloned().unwrap_or(Value::Null);
                let tool = a.get("tool_name").and_then(Value::as_str).unwrap_or("tool");
                let verdict = decide(run, task, tool, a.get("input").unwrap_or(&Value::Null), limit);
                json!({"content": [{"type": "text", "text": verdict.to_string()}]})
            }
            "ping" => json!({}),
            _ => {
                let _ = writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}}));
                let _ = out.flush();
                continue;
            }
        };
        let _ = writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": id, "result": result}));
        let _ = out.flush();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("musher-approve-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn wait_pending(d: &Path) -> Request {
        for _ in 0..200 {
            if let Some(r) = pending(d, "t") {
                return r;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no request appeared");
    }

    #[test]
    fn allow_keeps_the_input_and_clears_the_request() {
        let d = tmp("allow");
        let d2 = d.clone();
        let h = std::thread::spawn(move || decide(&d2, "t", "Bash", &json!({"command": "npm test"}), Duration::from_secs(5)));
        let r = wait_pending(&d);
        assert_eq!((r.tool.as_str(), r.what.as_str()), ("Bash", "npm test"));
        answer(&d, &r, true, false, "").unwrap();
        let v = h.join().unwrap();
        assert_eq!(v["behavior"], "allow");
        assert_eq!(v["updatedInput"]["command"], "npm test");
        assert!(pending(&d, "t").is_none());
    }

    #[test]
    fn deny_carries_the_message() {
        let d = tmp("deny");
        let d2 = d.clone();
        let h = std::thread::spawn(move || decide(&d2, "t", "Bash", &json!({"command": "rm x"}), Duration::from_secs(5)));
        answer(&d, &wait_pending(&d), false, false, "not that one").unwrap();
        let v = h.join().unwrap();
        assert_eq!((v["behavior"].as_str(), v["message"].as_str()), (Some("deny"), Some("not that one")));
    }

    #[test]
    fn nobody_answering_means_deny() {
        let d = tmp("late");
        let v = decide(&d, "t", "Bash", &json!({"command": "x"}), Duration::from_millis(50));
        assert_eq!(v["behavior"], "deny");
        assert!(pending(&d, "t").is_none());
    }

    #[test]
    fn always_covers_the_same_input_and_nothing_else() {
        let d = tmp("always");
        let d2 = d.clone();
        let h = std::thread::spawn(move || decide(&d2, "t", "Bash", &json!({"command": "npm test"}), Duration::from_secs(5)));
        answer(&d, &wait_pending(&d), true, true, "").unwrap();
        h.join().unwrap();
        assert_eq!(decide(&d, "t", "Bash", &json!({"command": "npm test"}), Duration::from_millis(1))["behavior"], "allow");
        assert_eq!(decide(&d, "t", "Bash", &json!({"command": "npm test && rm -rf /"}), Duration::from_millis(1))["behavior"], "deny");
    }

    #[test]
    fn an_answer_to_an_older_request_does_not_count() {
        let d = tmp("stale");
        write_atomic(&ans_path(&d, "t"), &json!({"id": "old", "allow": true}).to_string()).unwrap();
        // decide() drops leftovers before it asks, so this runs out the clock and denies.
        assert_eq!(decide(&d, "t", "Bash", &json!({"command": "x"}), Duration::from_millis(50))["behavior"], "deny");
    }
}
