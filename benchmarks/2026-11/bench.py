#!/usr/bin/env python3
# Head-to-head: herdr vs musher. usage: bench.py <herdr|musher> <A|B|C|D> <trial> [musher-binary]
import json, os, re, shutil, subprocess as sp, sys, time, glob
B = os.path.dirname(os.path.abspath(__file__))
tool, sc, n = sys.argv[1], sys.argv[2], sys.argv[3]
MU = sys.argv[4] if len(sys.argv) > 4 else f'{B}/bin/musher-0.3.5'
H = [os.path.expanduser('~/.local/bin/herdr'), '--session', 'mbench']
FN = ['parseDuration', 'wrapText', 'csvParse']
ENVNOTE = os.path.basename(MU) if tool == 'musher' else 'herdr-0.9.5'
tag = f'{tool}-{sc}-{n}-{ENVNOTE}'
repo = f'{B}/runs/{tag}'
base = {'A': 'base-a', 'B': 'base-b', 'C': 'base-c', 'D': 'base-a'}[sc]
shutil.rmtree(repo, ignore_errors=True); os.makedirs(os.path.dirname(repo), exist_ok=True)
sp.run(['git', 'clone', '-q', f'{B}/{base}', repo], check=True)
sp.run(['git', 'config', 'user.name', 'bench'], cwd=repo); sp.run(['git', 'config', 'user.email', 'b@b'], cwd=repo)
R = {'tool': tool, 'scenario': sc, 'trial': n, 'bin': ENVNOTE}

def prompt(t):
    if sc == 'B':
        return f'Add and export {t} in src/textkit.js so that test/{t}.test.js passes. Do not modify tests. Verify with: node --test test/{t}.test.js'
    return f'Implement src/{t}.js so that test/{t}.test.js passes. Do not modify tests. Verify with: node --test test/{t}.test.js'

def tests(cwd, cmd='node --test test/'):
    o = sp.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True)
    t = o.stdout + o.stderr
    m = lambda k: int((re.search(rf'^ℹ {k} (\d+)', t, re.M) or [0, 0])[1])
    return {'exit': o.returncode, 'pass': m('pass'), 'fail': m('fail')}

def h(*a):
    r = sp.run(H + list(a), capture_output=True, text=True)
    try: return json.loads(r.stdout or r.stderr)
    except json.JSONDecodeError: return {'raw': r.stdout + r.stderr}

def sess_cost(cwds):
    # interactive sessions report no cost: same formula from session logs (Opus 5.5 $4/$20, cache read x0.1, write x1.25)
    total, tok = 0.0, 0
    for c in cwds:
        d = os.path.expanduser('~/.claude/projects/' + re.sub(r'[^A-Za-z0-9]', '-', os.path.realpath(c)))
        seen = {}
        for f in glob.glob(d + '/*.jsonl'):
            for l in open(f):
                try: j = json.loads(l)
                except Exception: continue
                m = j.get('message') or {}
                if j.get('type') == 'assistant' and m.get('usage'): seen[m.get('id')] = m['usage']
        for u in seen.values():
            i, o, r, w = [u.get(k, 0) or 0 for k in ('input_tokens', 'output_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens')]
            w1h = (u.get('cache_creation') or {}).get('ephemeral_1h_input_tokens', 0) or 0
            total += (i*4 + o*20 + r*0.4 + (w-w1h)*5 + w1h*8) / 1e6; tok += i+o+r+w
    return round(total, 4), tok

def herdr_setup(items):
    ws = h('workspace', 'create', '--cwd', repo, '--label', tag[:20], '--no-focus')['result']['workspace']['workspace_id']
    shutil.rmtree(os.path.expanduser(f'~/.herdr/worktrees/{tag}'), ignore_errors=True)
    panes = {}
    for name, t in items:
        r = h('worktree', 'create', '--workspace', ws, '--branch', f'{tag}-{t}'.lower(), '--no-focus', '--trust-repository')['result']
        panes[name] = (r['root_pane']['pane_id'], r['root_pane']['cwd'], t)
    for name, (pane, _, _) in panes.items():
        r = h('agent', 'start', name, '--kind', 'claude', '--pane', pane, '--', '--permission-mode', 'acceptEdits', '--allowedTools', 'Bash(node --test*)' + (',Bash(node scripts/ci.mjs*)' if sc == 'C' else ''))
        code = r.get('error', {}).get('code')
        if code == 'agent_not_ready': h('agent', 'send-keys', name, 'down', 'enter')
        elif code: sys.exit(f'agent start failed: {r}')
    for name in panes: h('agent', 'wait', name, '--until', 'idle', '--timeout', '60000')
    return ws, panes

def herdr_go(panes):
    names = set(panes)
    for name, (_, _, t) in panes.items(): h('agent', 'prompt', name, prompt(t))
    t0, seen, fin, blocked = time.time(), set(), {}, 0
    R['approvals'] = 0; last = {}
    while True:
        ag = {a['name']: a['agent_status'] for a in h('agent', 'list')['result']['agents'] if a.get('name') in names}
        now = time.time() - t0
        for k, v in ag.items():
            if v == 'blocked':
                blocked += 1
                if now - last.get(k, -9) > 3:
                    last[k] = now; R['approvals'] += 1; h('agent', 'send-keys', k, 'enter')
            if v in ('working', 'blocked'): seen.add(k); fin.pop(k, None)
            elif v in ('done', 'idle') and k in seen and k not in fin: fin[k] = round(now, 1)
        if len(fin) == len(names) or now > 400: break
        time.sleep(0.5)
    return fin, blocked

def mu(*a, **k):
    return sp.run([MU] + list(a), cwd=repo, capture_output=True, text=True, **k)

def mu_status():
    return json.loads(mu('status', '--json').stdout)

def write_plan(check_fn):
    s = '[settings]\nparallel = 3\nretries = 2\nallow = ["Bash(node --test*)"]\n'
    for t in FN:
        s += f'\n[[task]]\nid = "{t}"\nprompt = "{prompt(t)}"\ncheck = "{check_fn(t)}"\n'
    open(f'{repo}/musher.toml', 'w').write(s)
    mu('init')  # keeps existing plan; hides .musher
    open(f'{repo}/.git/info/exclude', 'a').write('\nmusher.toml\n.musher/\n')

T0 = time.time()
if tool == 'herdr' and sc in 'ABC':
    ws, panes = herdr_setup([(f'h{sc}{n}-{t}'.lower(), t) for t in FN])
    R['setup_s'] = round(time.time() - T0, 1)
    fin, bl = herdr_go(panes)
    R['work_s'] = max(fin.values()) if fin else None; R['per'] = fin; R['total_s'] = round(time.time() - T0, 1); R['blocked_polls'] = bl
    R['cost'], R['tokens'] = sess_cost([p[1] for p in panes.values()])
    cwds = {p[2]: p[1] for p in panes.values()}
    if sc == 'A':
        R['own'] = {t: tests(c, f'node --test test/{t}.test.js')['exit'] == 0 for t, c in cwds.items()}
    if sc == 'C':
        R['ci_after_agent_done'] = {t: tests(c, f'node scripts/ci.mjs && node --test test/{t}.test.js')['exit'] == 0 for t, c in cwds.items()}
    # land it
    for p in panes.values():
        sp.run('git add -A && git commit -qm work || true', shell=True, cwd=p[1])
    conflicts = 0
    for t in FN:
        br = f'{tag}-{t}'.lower()
        m = sp.run(['git', 'merge', '--no-edit', '-q', br], cwd=repo, capture_output=True, text=True)
        if m.returncode:
            conflicts += 1
            R.setdefault('conflict_files', sp.run('git diff --name-only --diff-filter=U', shell=True, cwd=repo, capture_output=True, text=True).stdout.split())
            if sc == 'B':
                # human-equivalent step: one more agent, one extra prompt per conflict
                t1 = time.time()
                r = sp.run(['claude', '-p', 'Resolve the git merge conflicts in this repo so that both sides keep working, remove markers, verify with: node --test test/, then git add and git commit.',
                            '--permission-mode', 'acceptEdits', '--allowedTools', 'Bash(node --test*),Bash(git add*),Bash(git commit*)', '--output-format', 'json'], cwd=repo, capture_output=True, text=True, stdin=sp.DEVNULL)
                try: R['resolve_cost'] = R.get('resolve_cost', 0) + json.loads(r.stdout).get('total_cost_usd', 0)
                except Exception: pass
                R['resolve_s'] = R.get('resolve_s', 0) + round(time.time() - t1, 1)
            else:
                sp.run(['git', 'merge', '--abort'], cwd=repo)
    R['conflicts'] = conflicts; R['human_steps'] = conflicts
    R['final_tests'] = tests(repo); R['end_to_end_s'] = round(time.time() - T0, 1)
    for name in panes: h('pane', 'close', panes[name][0])
    h('workspace', 'close', ws)
elif tool == 'musher' and sc in 'ABC':
    chk = (lambda t: f'node scripts/ci.mjs && node --test test/{t}.test.js') if sc == 'C' else (lambda t: f'node --test test/{t}.test.js')
    write_plan(chk)
    r = mu('run', '--bg', '--no-notify'); R['start_s'] = round(time.time() - T0, 2)
    R['approvals'] = 0
    while True:
        st = mu_status()
        for t in st['tasks']:
            if t['status'] == 'waiting':
                if mu('approve', t['id']).returncode == 0: R['approvals'] += 1
        if st.get('finished'): break
        time.sleep(0.25)
    R['total_s'] = round(time.time() - T0, 1)
    R['tasks'] = {t['id']: {'status': t['status'], 'attempts': t.get('attempts'), 'cost': t.get('cost_usd'), 'sec': t.get('ended', 0) - t.get('started', 0)} for t in st['tasks']}
    wt = {t['id']: t['worktree'] for t in st['tasks']}
    R['cost_reported'] = round(sum(t.get('cost_usd', 0) for t in st['tasks']), 4)
    R['cost'], R['tokens'] = sess_cost(list(wt.values()))
    if sc == 'C': R['ci_after_done'] = {t: tests(c, chk(t))['exit'] == 0 for t, c in wt.items()}
    m = mu('merge', '--resolve', '--no-notify') if sc == 'B' else mu('merge')
    R['merge_rc'] = m.returncode; R['merge_out'] = (m.stdout + m.stderr)[-400:]
    st2 = mu_status(); R['cost_end'] = round(sum(t.get('cost_usd', 0) for t in st2['tasks']), 4)
    R['human_steps'] = 0 if m.returncode == 0 else 1
    R['final_tests'] = tests(repo); R['end_to_end_s'] = round(time.time() - T0, 1)
elif sc == 'D':
    cmd = f'echo run-{n} && npm test'
    p = f'Run this exact shell command and tell me the output: {cmd}'
    ev = []
    if tool == 'herdr':
        ws = h('workspace', 'create', '--cwd', repo, '--label', tag[:20], '--no-focus')
        pane = ws['result']['root_pane']['pane_id']; name = f'blk{n}'
        r = h('agent', 'start', name, '--kind', 'claude', '--pane', pane, '--', '--permission-mode', 'acceptEdits')
        if r.get('error', {}).get('code') == 'agent_not_ready': h('agent', 'send-keys', name, 'down', 'enter')
        h('agent', 'wait', name, '--until', 'idle', '--timeout', '60000'); time.sleep(1)
        t0 = time.time(); h('agent', 'prompt', name, p)
        h('agent', 'wait', name, '--until', 'blocked', '--timeout', '60000'); R['noticed_s'] = round(time.time() - t0, 1)
        scr = sp.run(H + ['agent', 'read', name, '--source', 'visible'], capture_output=True, text=True).stdout
        R['dialog_visible'] = 'proceed' in scr.lower()
        t1 = time.time(); h('agent', 'send-keys', name, 'enter')  # approve: option 1 "Yes"
        h('agent', 'wait', name, '--until', 'done', 'idle', '--timeout', '60000') if False else None
        while time.time() - t1 < 120:
            st = [a['agent_status'] for a in h('agent', 'list')['result']['agents'] if a.get('name') == name]
            if st and st[0] in ('done', 'idle'): break
            time.sleep(0.25)
        R['approved_to_done_s'] = round(time.time() - t1, 1); R['approval_steps'] = 1
        out = sp.run(H + ['agent', 'read', name, '--source', 'recent', '--lines', '400'], capture_output=True, text=True).stdout
        R['ran_command'] = f'run-{n}' in out and ('fail' in out.lower() or 'pass' in out.lower())
        h('pane', 'close', pane)
    else:
        open(f'{repo}/.git/info/exclude', 'a').write('\n.musher/\n')
        t0 = time.time(); mu('do', p, '--id', 'blk', '--bg', '--no-notify')
        noticed = None; approved = None
        while time.time() - t0 < 180:
            st = mu_status(); t = st['tasks'][0]
            s = json.dumps(t)
            if noticed is None and (t['status'] in ('blocked', 'waiting') or 'denied' in (t.get('note') or '') or 'permission' in (t.get('note') or '').lower() or t.get('waiting')):
                noticed = round(time.time() - t0, 1); R['first_signal'] = {'status': t['status'], 'note': t.get('note')}
            if t['status'] == 'waiting' and approved is None:
                approved = time.time(); mu('approve', 'blk'); R['approval_steps'] = 1
            if st.get('finished'): break
            time.sleep(0.25)
        if approved: R['approved_to_done_s'] = round(time.time() - approved, 1)
        R['ran_command'] = f'run-{n}' in mu('logs', 'blk').stdout.split('[/prompt]')[-1].replace(f'echo run-{n}', '')
        R['noticed_s'] = noticed; R['ended_s'] = round(time.time() - t0, 1); R['end_status'] = t['status']; R['end_note'] = t.get('note')
        txt = mu('logs', 'blk').stdout; R['tail'] = txt[-300:]
print(json.dumps(R, ensure_ascii=False), flush=True)
open(f'{B}/logs/results.jsonl', 'a').write(json.dumps(R, ensure_ascii=False) + '\n')
