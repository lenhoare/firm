// Explicit live acceptance trial. This spends provider allowance: never run in CI.
// Requires a fresh, idle live experiment. Restores settings without resetting counters.
import assert from 'node:assert/strict';
const base = 'http://127.0.0.1:7433';
async function api(path, body) {
  const response = await fetch(base + '/api/' + path, {
    ...(body === undefined ? {} : {method:'POST',headers:{'Content-Type':'application/json','X-Firm-Control':'1'},body:JSON.stringify(body)}),
    signal:AbortSignal.timeout(15000),
  });
  const data = await response.json();
  if (!response.ok) throw Error(path + ': ' + (data.error || response.status));
  return data;
}
const initial = await api('state');
assert(!initial.state.demo && initial.state.paused && !initial.active && !initial.state.active_turn);
assert.equal(initial.state.stage, 'idle');
assert.equal(initial.state.manager_provider, 'codex');
assert(initial.codex_connected);
assert.equal(initial.state.allowances.max_used_percent, 50);
const ids = ['grok','qwen','muse'];
for (const id of ids) assert(initial.providers.some(p=>p.id===id));
const objective = `First live three-worker acceptance trial: finish the tiny Rust slug formatter in this workspace.
Hard contract: lowercase ASCII letters, preserve ASCII digits, collapse runs of all other characters (including non-ASCII) into a single hyphen, trim leading/trailing hyphens; empty or punctuation-only input returns an empty string. No dependencies, networking, installations, other agents, or edits outside this workspace. Preserve the four original tests.
Use exactly one bounded assignment per worker, in this order:
1. Grok: implement slug in src/lib.rs only, run cargo test --offline if permitted, report evidence and discoveries.
2. Qwen: independently inspect implementation; add focused edge-case tests in tests/edge_cases.rs only (ASCII digits, whitespace, Unicode boundaries, all-non-ASCII, idempotence). Do not change src/lib.rs. Run cargo test --offline if permitted and report discrepancies.
3. Muse: independently review code and both test sets without editing them; update README.md with accurate usage examples and limits, plus REVIEW.md with findings and test evidence. Do not fix implementation or tests. Run cargo test --offline if permitted.
Participation by all three workers is part of this trial: do not declare completion after Grok alone. Use the supplied recent decisions to track completed roles. If a worker is blocked, record it and attempt the remaining distinct roles without retrying that worker or concealing its failure. Do not substitute providers. If no allowed remaining role can run, return blocked. Final completion requires all three roles completed and controller verification passing; otherwise return a concise blocked summary after the remaining roles. At most four manager turns and three worker runs; no repair loop. Keep manager decisions concise and use supplied evidence rather than spending turns on additional investigation.`;
let changed = false;
try {
  const baseline = await api('snapshots',{label:'Before first live three-worker trial · original limits'});
  console.log(JSON.stringify({event:'baseline',snapshot:baseline}));
  changed = true;
  await api('allowances',{...initial.state.allowances,max_used_percent:80,manager_turns:4,worker_runs:3});
  for (const id of ids) await api('providers/'+id,{enabled:true,max_runs:1});
  await api('objective',{objective});
  const recipe = await api('snapshots',{label:'First live trial · 80% stop threshold · one run per worker'});
  console.log(JSON.stringify({event:'trial-configured',snapshot:recipe,manager:'codex',workers:ids,manager_turns:4,worker_runs:3}));
  await api('start',{});
  const deadline = Date.now()+25*60*1000;
  let last = '';
  while (Date.now()<deadline) {
    const view = await api('state'), s=view.state;
    const report={stage:s.stage,paused:s.paused,active:view.active,reason:s.reason,manager_runs:s.manager_starts.length,worker_runs:s.worker_starts.length,tasks:s.tasks.map(t=>({provider:t.assignment.provider,status:t.status,exit_code:t.exit_code})),latest_decision:s.decisions.at(-1)?.summary};
    const key=JSON.stringify({...report,reason:report.reason.replace(/Next manager turn in \d+s/, 'Waiting for manager spacing')});
    if(key!==last){console.log(JSON.stringify(report));last=key;}
    if(!view.active && ['complete','blocked','cancelled'].includes(s.stage)) {
      console.log(JSON.stringify({event:'trial-ended',stage:s.stage,tasks:s.tasks,decisions:s.decisions}));
      break;
    }
    if(s.paused && !view.active) break;
    await new Promise(resolve=>setTimeout(resolve,2000));
  }
} finally {
  if(changed) {
    let view=await api('state');
    if(view.active) await api('stop',{}); else await api('pause',{});
    const cleanupDeadline=Date.now()+55000;
    while(view.active && Date.now()<cleanupDeadline) {
      await new Promise(resolve=>setTimeout(resolve,1000));
      view=await api('state');
    }
    assert(!view.active,'Active work did not stop; restore the 50% threshold manually when idle');
    await api('allowances',initial.state.allowances);
    for(const id of ids) {
      const p=initial.providers.find(p=>p.id===id);
      await api('providers/'+id,{enabled:p.enabled,max_runs:p.max_runs});
    }
    const final=await api('state');
    assert(final.state.paused && final.state.allowances.max_used_percent===50);
    console.log(JSON.stringify({event:'limits-restored',paused:final.state.paused,max_used_percent:50,manager_runs:final.state.manager_starts.length,worker_runs:final.state.worker_starts.length}));
    await api('snapshots',{label:'After first live trial · normal limits restored'});
  }
}
