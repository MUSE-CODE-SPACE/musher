# musher

**Drive a team of AI coding agents.** Each agent gets its own git worktree. Tasks run as a dependency graph, and every result is checked by your tests, with failures sent back to the agent until they pass. A durable log records everything. Musher is one ~700 KB binary with no server.

[한국어 README](README.ko.md) · [Step-by-step beginner manual](https://www.resonance-space.net/musher/en/) (also in [한국어](https://www.resonance-space.net/musher/) · [日本語](https://www.resonance-space.net/musher/ja/) · [简体中文](https://www.resonance-space.net/musher/zh/) · [Español](https://www.resonance-space.net/musher/es/))

```
musher run 261027-1102  3/5 done  2 active  $0.41  04:12
 ✓ api        done      01:40  $0.12  4 files  check ✓
 ✓ schema     done      00:52  $0.06  2 files  check ✓  2 attempts
 ◐ client     working   01:05  $0.09  Edit sdk/health.ts
 ◑ docs       checking  00:31  $0.03
 ◌ e2e        queued                 after client
```

A **herder** watches a flock. A **musher** drives a sled team toward a destination, and every dog in harness pulls its share. Herdr shows you what your agents are doing. Musher gets a body of work *done* with them.

---

## Why

Running several coding agents at once tends to break in the same few ways:

| Pain | What musher does |
|---|---|
| Two agents edit the same checkout and clobber each other | One **git worktree + branch per task**, created and cleaned up for you |
| "B needs A's code first" | `after = ["a"]`: B starts **from A's branch, stacked**, with A's summary in its prompt |
| Parallel branches conflict when they come together | The task that stacks them **asks its agent to resolve the conflict**. Musher refuses leftover markers, then runs the check. `musher merge --resolve` does the same for a whole run |
| The agent says "done" but the tests are red | `check = "npm test"` is a **gate**. On failure the output goes back into the *same* agent session, which tries again. A timed-out attempt is resumed the same way, with its work so far kept |
| Squinting at panes to guess who is blocked | State comes from the agent's **structured event stream** (Claude `stream-json`, Codex `--json`), not from screen scraping |
| Context lost between agents | **Shared team notes** (`musher mem`) plus **upstream handoff** summaries go into every prompt |
| "What happened overnight?" | An **append-only event log** and per-task logs on disk. `musher resume` continues after crashes, Ctrl-C or edits |
| Babysitting a terminal | `--bg`, then `musher watch` from any terminal and `musher wait` from scripts. A desktop notification fires when the run ends |
| Runaway spend | Per-task and per-run **cost tracking** plus `budget_usd` |
| "Where do I even start?" | `musher plan "<goal>"`: an agent reads the repo and writes the task graph, with checks, for you to edit |
| Telling people what landed | `musher report` (markdown: status, time, cost, files, each task's summary), `musher pr` opens one PR per task, `on_task`/`on_finish` hooks ping Slack, ntfy or anything |

## Install

```sh
# prebuilt binary (macOS / Linux, arm64 / x86_64)
curl -fsSL https://raw.githubusercontent.com/MUSE-CODE-SPACE/musher/main/install.sh | sh
# pin a version / choose the directory: MUSHER_VERSION=v0.3.0 MUSHER_INSTALL_DIR=/usr/local/bin

# or from source
cargo install --git https://github.com/MUSE-CODE-SPACE/musher
```

Requirements: `git`, plus at least one agent CLI: [Claude Code](https://docs.anthropic.com/claude-code), [Codex CLI](https://github.com/openai/codex) or Gemini CLI. Any other command works via `[agents.*]`. Run `musher doctor` to check.

## 60-second tour

```sh
cd your-repo
musher init                 # writes musher.toml, hides .musher/ from git
$EDITOR musher.toml         # describe the tasks
musher run                  # live dashboard; Ctrl-C stops safely
musher diff api             # review one task's own changes
musher merge                # land every finished branch, in dependency order
```

One-off tasks need no plan file:

```sh
musher do "Fix the flaky date test in utils/" --check "npm test -- utils"
musher do "Add rate limiting to /login" --agent codex --bg
```

Or let an agent write the plan from a goal, then run it and open PRs:

```sh
musher plan "Add subtract, multiply and divide commands with tests, and document them"
musher run --bg && musher wait && musher pr --all --draft
```

## musher.toml

```toml
[settings]
parallel   = 3            # tasks running at once
agent      = "claude"     # claude | codex | gemini | shell | your own
isolation  = "worktree"   # or "none" to work in the repo itself
timeout    = "30m"        # per attempt
retries    = 2            # fix attempts after a failed check
permission = "acceptEdits"
setup      = "npm ci"     # once per fresh worktree
budget_usd = 5.0          # stop starting new tasks past this spend
allow      = ["Bash(npm run lint:*)"]   # extra tools allowed; each task's check is always allowed
on_task    = "say \"$MUSHER_TASK $MUSHER_STATUS\""                           # hook after each task
on_finish  = "curl -sS -d \"musher $MUSHER_STATUS $MUSHER_DONE/$MUSHER_TOTAL\" ntfy.sh/me"   # hook when the run ends

[[task]]
id     = "api"
prompt = "Add GET /health returning {\"ok\": true}, with a test."
check  = "npm test"

[[task]]
id     = "client"
after  = ["api"]          # stacked on api's branch + gets api's summary
prompt = "Add healthCheck() to the SDK."
check  = "npm test"
model  = "sonnet"         # per-task overrides: agent, model, timeout, retries, permission, allow

# Bring any CLI agent:
[agents.aider]
cmd = ["aider", "--yes", "--message", "{prompt}"]
```

Placeholders in agent commands: `{prompt}` `{session}` `{cwd}` `{permission}`, plus `{model:--flag}` and `{allow:--flag}`, which expand to a flag/value pair only when set. Typos in the file are errors, not silent no-ops. Dependency cycles and unknown ids are caught before anything runs (`musher run --dry-run` prints the execution waves).

## How a task runs

```
queued ─▶ setup ─▶ working ─▶ (commit) ─▶ checking ─┬─▶ done
            │                                         │
            │       fixing ◀── check output ◀────────┘  (up to `retries`)
            │         └─ same agent session (--resume), so it remembers what it tried
            └─ worktree from base commit, dependency branches merged in
               (a conflict goes to the agent first: "keep both sides"), then `setup`
```

* **failed**: the agent errored, timed out, or the check still failed after all retries.
* **blocked**: the check is failing *and* the agent was denied a tool it asked for (run `musher attach <task>` and grant it), or the agent hit a usage limit, quota or auth error. Those are not retried: a person has to act, then `musher resume`.
* **skipped**: a dependency did not finish. The reason is shown.
* **stopped**: you pressed Ctrl-C. `musher resume` picks up exactly here and keeps finished tasks. If you edited a prompt in `musher.toml` in the meantime, the edit is used.

## Commands

| | |
|---|---|
| `musher run [-j N] [--only a,b] [--dry-run] [--bg]` | run the plan (`--only` pulls in dependencies) |
| `musher do "<prompt>" [--agent] [--check] [--id] [--model] [--bg]` | one-off task |
| `musher plan "<goal>" [--out f] [--print] [--force]` | an agent reads the repo and writes `musher.toml`: 2–6 tasks with checks and dependencies; your `[settings]` are kept |
| `musher resume [run]` | continue a stopped or failed run |
| `musher stop [run]` | stop a background run; agents are terminated, finished work is kept |
| `musher watch` / `status [--json]` / `wait` / `ls [--json]` | observe from anywhere; `wait` exits 0 only if all tasks are done |
| `musher report [run] [task…]` | markdown report: status, attempts, time, cost, files changed and every task's own summary (also written to `report.md` when a run ends) |
| `musher logs <task> [-f] [--raw]` | readable agent transcript (tools used, answers, denials) and check output |
| `musher events [-f] [--json]` | the durable event log |
| `musher diff <task> [--stat] [--all]` | the task's own changes (`--all` includes stacked dependencies) |
| `musher ask <task> "<msg>"` | follow up in the same agent session, then re-check |
| `musher attach <task>` | drop into the session **interactively** in its worktree (`claude --resume …`) |
| `musher check <task>` | you edited the worktree yourself: commit that and re-run the check, without calling the agent |
| `musher merge [task…]` | land finished work: merges only the top branches (stacked ones come along), and aborts cleanly on conflict |
| `musher merge --resolve` | an `integrate` task stacks every finished branch, an agent resolves conflicts, all checks must pass, then that one branch is merged |
| `musher pr <task…> \| --all [--draft] [--base b]` | push each task branch and open a pull request with the task's report and diff stat as body (needs `gh`) |
| `musher mem ["note"] [--edit] [--clear]` | shared team notes injected into every prompt |
| `musher clean [--purge]` | remove worktrees (`--purge` also removes branches and history) |
| `musher doctor` | check git and installed agents |
| `musher completions <zsh\|bash\|fish>` | shell completion for commands, task ids and run ids |

### Hooks

`on_task` runs after every task reaches a final state, `on_finish` once when the run ends. Both are shell commands run from the repo root with the result in environment variables, so they work with anything: `say`, `notify-send`, `curl` to Slack or ntfy, or a script of yours.

| hook | variables |
|---|---|
| `on_task` | `MUSHER_RUN`, `MUSHER_TASK`, `MUSHER_STATUS` (done/failed/blocked/skipped/stopped), `MUSHER_NOTE`, `MUSHER_COST`, `MUSHER_BRANCH`, `MUSHER_SUMMARY` (path to the task's summary) |
| `on_finish` | `MUSHER_RUN`, `MUSHER_STATUS` (done/incomplete/stopped), `MUSHER_DONE`, `MUSHER_TOTAL`, `MUSHER_COST`, `MUSHER_REPORT` (path to `report.md`) |

Hooks never block the run: musher waits up to 10 s, a slower hook keeps running on its own, and its output goes to `.musher/runs/<id>/hooks.log`. A failing hook is an event, not an error.

### Let an agent be the lead

Every command is scriptable, and status is available as JSON. A "lead" agent can therefore fan out work on its own:

```sh
musher do "Write the migration" --id migrate --bg
musher wait --json | jq '.tasks[] | {id, status, note}'
```

A ready-made Claude Code skill lives in [`skills/musher/SKILL.md`](skills/musher/SKILL.md). Copy it to `~/.claude/skills/musher/`.

## Musher vs Herdr

[Herdr](https://herdr.dev) is an excellent agent-aware **terminal multiplexer**: tmux with a sidebar that shows which agent is working, blocked or done. Musher is an **orchestrator**. The two tools sit at different layers, so this comparison states plainly what each one is for.

| | **Herdr** | **musher** |
|---|---|---|
| Core idea | Multiplex interactive agent terminals | Run a graph of agent tasks to a verified result |
| Runtime model | Background server owns the PTYs | No server: a run is a process plus files in `.musher/` |
| Binary (macOS arm64) | **22.5 MB** (v0.9.5, measured) | **0.7 MB** (755 KB, measured) |
| Memory while agents work | **46.5 MB RSS**: server 29.5 + client 17.0 (2 agents, measured) | **3.1 MB RSS** (8 agents, measured) |
| Idle cost | Server process stays resident | **Nothing runs** between runs |
| Agent state | Process name + **screen-pattern manifests**; new prompts "may initially show as `idle` instead of `blocked`"² | **Structured events** from the agent (session id, tools, cost, final answer, permission denials) plus exit codes |
| Isolation between agents | Worktree workspaces **on request** (`herdr worktree create`); by default panes share one checkout | **Worktree + branch per task**, automatic, stacked by dependency |
| Conflicts between parallel work | Up to you | **Resolved by an agent** while stacking, verified by checks |
| Dependencies / task graph | Not provided² | `after = [...]` DAG, parallel waves, **branch stacking**, failure propagation |
| Verification | Not provided | `check` gate with **automatic fix loop** in the same session |
| Shared context | Through files/git only² | Team notes plus upstream summaries injected into prompts |
| History | Live state; no durable event history² | **Append-only JSONL events**, per-task logs, run snapshots |
| Crash / restart | Restart restores layout; conversations only where session ids were recorded² | `musher resume`: finished work kept, sessions resumed |
| Cost control | — | Cost per task and run, `budget_usd` |
| Planning | You type into each pane | `musher plan "<goal>"` drafts the task graph from the repo |
| Landing the work | Manual | `musher diff`, `musher merge` (dependency order), `musher pr` (one PR per task), `musher report` |
| Notifications | Sounds in the UI | `on_task` / `on_finish` shell hooks (Slack, ntfy, anything) |
| Watching agents type live, mouse, splits | **Yes, this is Herdr's strength** | Not a multiplexer: `logs -f`, `watch`, `attach` |
| Remote thin client, plugin marketplace | **Yes** | No (it's a CLI; run it over SSH) |
| Config | `config.toml` (keys, UI, sounds) | `musher.toml` (tasks), strict validation |

² From Herdr's own v0.9.5 docs ([agents](https://herdr.dev/docs/agents), [session state](https://herdr.dev/docs/session-state), [CLI reference](https://herdr.dev/docs/cli-reference)) and the [Developers Digest deep dive](https://www.developersdigest.tech/blog/herdr-deep-dive-agent-terminal-multiplexer). All sizes and memory figures were measured side by side on one Apple Silicon Mac (Oct 2026). Herdr ran from its official release binary.

**Use Herdr** when you want to sit with five live agent UIs side by side and steer them by hand. **Use musher** when you want to hand over a list of tasks, walk away, and come back to tested, isolated branches. You can also use both: run `musher watch` in a Herdr pane.

## Design notes

* **Small on purpose.** About 3.5k lines of Rust with three dependencies (`serde`, `serde_json`, `toml`) plus `libc`. There is no async runtime and no TUI framework.
* **Files are the API.** `.musher/runs/<id>/state.json` is rewritten atomically, `events.jsonl` is append-only, and `logs/` and `out/` hold plain text. Anything can read them.
* **Safe defaults.** The default permission mode is `acceptEdits`. Agents never push. `.musher/` is hidden through `.git/info/exclude`, so your `.gitignore` is never touched. Ctrl-C terminates whole agent process groups, so no orphan processes are left behind.
* **Platforms:** macOS and Linux. Windows is untested (WSL should work).

## Try it without an API key

```sh
cd examples/demo && ./run-demo.sh
```

The demo uses the built-in `shell` agent. It shows parallel waves, branch stacking, a check that fails and gets fixed, a failure that propagates, `resume` and `merge`.

## License

MIT © Muse
