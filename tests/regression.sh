#!/bin/sh
# End-to-end regression: the whole lifecycle with the shell agent, no API keys.
# run → stop mid-way → resume → hand-fix + check → hooks → report → merge → pr guard → clean → approvals.
# Exits non-zero on the first wrong answer. Used by CI and `make`-less local runs:
#   MUSHER=target/release/musher sh tests/regression.sh
set -eu
musher=${MUSHER:-$(cd "$(dirname "$0")/.." && pwd)/target/release/musher}
[ -x "$musher" ] || { echo "build first: cargo build --release"; exit 1; }
fail() { echo "FAIL: $*  (repo kept at $repo)"; exit 1; }

repo=$(mktemp -d "${TMPDIR:-/tmp}/musher-reg.XXXXXX")
cd "$repo"
git init -q -b main
git -c user.name=t -c user.email=t@localhost commit -q --allow-empty -m init
cat > musher.toml <<'EOF'
[settings]
agent     = "shell"
parallel  = 3
retries   = 0
on_task   = "echo $MUSHER_TASK=$MUSHER_STATUS >> hooks.txt"
on_finish = "echo finish=$MUSHER_STATUS >> hooks.txt"

[[task]]
id     = "a"
prompt = "echo a > a.txt"
check  = "test -f a.txt"

[[task]]
id     = "slow"
prompt = "sleep 20; echo slow > slow.txt"

[[task]]
id     = "needs-fix"
prompt = "echo partial > p.txt"
check  = "test -f fixed.txt"

[[task]]
id     = "after-a"
after  = ["a"]
prompt = "test -f a.txt && echo b > b.txt"
EOF

echo "== run --bg, stop while slow is working"
"$musher" run --bg --no-notify >/dev/null
sleep 2
"$musher" stop >/dev/null
"$musher" status --json | grep -q '"status": "stopped"' || fail "slow should be stopped"
"$musher" ls | head -1 | grep -q "incomplete\|stopped" || fail "ls state"

echo "== resume: slow (now quick) finishes, needs-fix still fails"
sed -i.bak 's/sleep 20; //' musher.toml && rm -f musher.toml.bak
"$musher" resume --no-ui --no-notify >/dev/null 2>&1 || true
st=$("$musher" status --json)
echo "$st" | python3 -c '
import json,sys; d=json.load(sys.stdin); s={t["id"]:t["status"] for t in d["tasks"]}
assert s["a"]=="done" and s["slow"]=="done" and s["after-a"]=="done", s
assert s["needs-fix"]=="failed", s'

echo "== hand-fix in the worktree, then musher check (no agent)"
wt=$("$musher" status --json | python3 -c 'import json,sys; print([t for t in json.load(sys.stdin)["tasks"] if t["id"]=="needs-fix"][0]["worktree"])')
echo fixed > "$wt/fixed.txt"
"$musher" check needs-fix >/dev/null || fail "check should pass after the fix"
grep -q "needs-fix=done" hooks.txt || fail "on_task hook after check"
grep -q "finish=" hooks.txt || fail "on_finish hook"

echo "== report, pr guard, merge"
"$musher" report | grep -q '| `needs-fix` | shell | done' || fail "report row"
"$musher" pr a 2>&1 | grep -q "no git remote" || fail "pr should refuse without a remote"
"$musher" merge | grep -q "merged" || fail "merge"
for f in a.txt b.txt p.txt fixed.txt slow.txt; do [ -f "$f" ] || fail "missing $f after merge"; done

echo "== events are a clean JSONL stream; ls --json parses"
"$musher" events --json | python3 -c 'import json,sys; [json.loads(l) for l in sys.stdin]'
"$musher" ls --json | python3 -c 'import json,sys; assert json.load(sys.stdin)[0]["state"]=="done"'

echo "== clean"
"$musher" clean --purge | grep -q removed || fail "clean"
[ "$(git branch --list 'musher/*' | wc -l | tr -d ' ')" = 0 ] || fail "branches left after --purge"

cd /; rm -rf "$repo"
echo "== approvals: a held task shows as waiting, approve/deny answer it, hook fires"
repo3=$(mktemp -d "${TMPDIR:-/tmp}/musher-reg.XXXXXX")
cd "$repo3"
git init -q -b main
git -c user.name=t -c user.email=t@localhost commit -q --allow-empty -m init
cat > fake-agent.sh <<EOF
#!/bin/sh
# Speaks the MCP call a real agent would make, through musher's own approval server.
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"approve","arguments":{"tool_name":"Bash","input":{"command":"npm test"}}}}' \
  | "$musher" __approve "\$MUSHER_ROOT/.musher/runs/\$MUSHER_RUN" "\$MUSHER_TASK" 30 > "\$MUSHER_TASK.answer"
echo finished
EOF
cat > musher.toml <<'EOF'
[settings]
agent = "fake"
on_approval = "echo $MUSHER_TASK >> $MUSHER_ROOT/approval-hook.txt"
[agents.fake]
cmd = ["sh", "{cwd}/../../../../fake-agent.sh"]
raw = true
[[task]]
id = "yes"
prompt = "x"
[[task]]
id = "no"
prompt = "x"
EOF
"$musher" init >/dev/null 2>&1 || true
"$musher" run --bg --no-notify >/dev/null
i=0
until [ "$("$musher" status --json | grep -c '"status": "waiting"')" = 2 ]; do
  i=$((i+1)); [ $i -lt 100 ] || fail "tasks should be waiting"; sleep 0.1
done
"$musher" approve 2>&1 | grep -q "name one" || fail "two waiting: approve must ask which"
rc=0; "$musher" wait --input || rc=$?; [ $rc = 3 ] || fail "wait --input should exit 3"
"$musher" approve yes | grep -q "npm test" || fail "approve should say what it allowed"
"$musher" deny no -m "not now" >/dev/null
"$musher" wait >/dev/null || true
grep -q allow .musher/wt/*/yes/yes.answer || fail "approve should reach the agent as allow"
grep -q "not now" .musher/wt/*/no/no.answer || fail "deny should carry the message"
[ "$(wc -l < approval-hook.txt | tr -d ' ')" = 2 ] || fail "on_approval should fire once per task"
"$musher" approve 2>/dev/null && fail "nothing should be waiting now"
cd /; rm -rf "$repo3"

echo "regression OK"
