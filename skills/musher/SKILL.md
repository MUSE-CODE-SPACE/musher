---
name: musher
description: Delegate coding work to a team of parallel agents with musher. Each task runs in its own git worktree and is verified by a check command. Use this when a request splits into independent pieces, or when a change must pass tests before you accept it.
---

# Driving musher

`musher` runs coding-agent tasks in isolated git worktrees, in dependency order, and gates each one on a check command. Each command below is non-interactive.

## One task

```sh
musher do "<precise instructions>" --id <short-id> --check "<test command>" --bg
musher wait --json        # exit 0 = done; read .tasks[].status / .note
musher diff <short-id> --stat
```

## Several tasks

Let musher draft the plan from a goal, or write `musher.toml` at the repo root yourself:

```sh
musher plan "<goal>" --print     # proposal only; drop --print to write musher.toml (--force replaces existing tasks)
```

```toml
[settings]
parallel = 3

[[task]]
id = "a"
prompt = "..."
check = "npm test"

[[task]]
id = "b"
after = ["a"]        # starts from a's branch, receives a's summary
prompt = "..."
```

Then run `musher run --bg && musher wait --json`.

## After a run

- `musher report`: markdown summary of the run (status, time, cost, each task's summary). Paste it when you tell the user what happened.
- `musher stop`: stop a background run; finished tasks are kept.
- `musher logs <id>`: what the agent did, and why a check failed.
- `musher ask <id> "<follow-up>"`: continue in the same session, then re-check.
- `musher resume`: re-run unfinished tasks after you fix the plan.
- `musher merge`: land finished branches, in dependency order. Ask the user before you merge.
- `musher pr <id>` or `musher pr --all --draft`: push task branches and open pull requests (needs `gh`). Ask the user before you push.
- `musher mem "<fact every agent should know>"`: add to the shared team notes.

## Rules

- Write each prompt so it stands alone. The agent sees only the prompt, the team notes and its upstream summaries.
- Prefer a `check` on every task. Without one, "done" means only that the agent finished.
- Do not run `musher clean --purge` unless the user asks for it.
