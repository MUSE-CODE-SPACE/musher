import json,re,glob,os,statistics as S
rows=[json.loads(l) for l in open('logs/results.jsonl')]
def cost1(paths):
    tot=0
    for p in paths:
        d=os.path.expanduser('~/.claude/projects/'+re.sub(r'[^A-Za-z0-9]','-',os.path.realpath(p)))
        seen={}
        for f in sorted(glob.glob(d+'/*.jsonl'),key=os.path.getmtime)[-1:]:
            for l in open(f):
                try:j=json.loads(l)
                except:continue
                m=j.get('message') or {}
                if j.get('type')=='assistant' and m.get('usage'): seen[m.get('id')]=m['usage']
        for u in seen.values():
            w=u.get('cache_creation_input_tokens',0) or 0; w1=(u.get('cache_creation') or {}).get('ephemeral_1h_input_tokens',0) or 0
            tot+=((u.get('input_tokens')or 0)*4+(u.get('output_tokens')or 0)*20+(u.get('cache_read_input_tokens')or 0)*.4+(w-w1)*5+w1*8)/1e6
    return round(tot,4)
for r in rows:
    if r['scenario'] in 'ABC':
        if r['tool']=='herdr': ps=glob.glob(os.path.expanduser(f"~/.herdr/worktrees/herdr-{r['scenario']}-{r['trial']}-*/*"))
        else: ps=glob.glob(f"runs/musher-{r['scenario']}-{r['trial']}-{r['bin']}/.musher/wt/*/*")
        r['cost_same']=cost1(ps)
for sc in 'ABCD':
  for t,b in (('herdr','herdr-0.9.5'),('musher','musher-0.3.5'),('musher','musher-0.4.0')):
    rs=[r for r in rows if r['tool']==t and r['scenario']==sc and r['bin']==b]
    if not rs: continue
    print(sc,b,'n=',len(rs))
    for r in rs:
      print('  ',json.dumps({k:r.get(k) for k in ['setup_s','work_s','total_s','end_to_end_s','cost_same','cost','resolve_cost','conflicts','human_steps','approvals','final_tests','ci_after_agent_done','ci_after_done','noticed_s','approved_to_done_s','approval_steps','ran_command','end_status','merge_rc'] if k in r}))

print()
for sc in 'ABC':
  for t,b in (('herdr','herdr-0.9.5'),('musher','musher-0.3.5'),('musher','musher-0.4.0')):
    rs=[r for r in rows if r['tool']==t and r['scenario']==sc and r['bin']==b]
    print(sc,b,'time',round(S.mean(r['end_to_end_s'] for r in rs),1),'cost',round(S.mean(r['cost_same']+(r.get('resolve_cost') or 0) for r in rs),3),'costs',[r['cost_same'] for r in rs])
