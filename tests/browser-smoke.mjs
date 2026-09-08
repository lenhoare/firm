// Run against a fresh demo server and an isolated Chrome instance on port 9223.
// --providers / --snapshots also accept an existing paused demo and preserve counters.
// Uses Node's built-in WebSocket client; no browser-test packages are required.
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { createHash } from 'node:crypto';

const base = process.env.FIRM_TEST_URL || 'http://127.0.0.1:7433';
const initial = await (await fetch(base + '/api/state')).json();
const snapshotsOnly = process.argv.includes('--snapshots');
const providersOnly = process.argv.includes('--providers');
const meetingsOnly = process.argv.includes('--meetings');
const navigationOnly = process.argv.includes('--navigation');
const managerOnly = process.argv.includes('--manager');
const usageOnly = process.argv.includes('--usage');
assert(initial.state.demo && (snapshotsOnly || providersOnly || meetingsOnly || navigationOnly || managerOnly || usageOnly ? initial.state.paused && !initial.active : !initial.state.objective), 'Test requires an idle DEMO server; the cycle test additionally requires an empty objective');
const pages = await (await fetch('http://127.0.0.1:9223/json/list')).json();
const page = pages.find(p => p.type === 'page');
assert(page, 'Start headless Chrome with --remote-debugging-port=9223');
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
let next = 0;
const pending = new Map(), exceptions = [];
ws.onmessage = event => {
  const message = JSON.parse(event.data);
  if (message.id) {
    const entry = pending.get(message.id);
    if (entry) { pending.delete(message.id); clearTimeout(entry.timer); message.error ? entry.reject(Error(JSON.stringify(message.error))) : entry.resolve(message.result); }
  } else if (message.method === 'Runtime.exceptionThrown') exceptions.push(message.params);
};
function rpc(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++next;
    const timer = setTimeout(() => { pending.delete(id); reject(Error('CDP timeout: ' + method)); }, 10000);
    pending.set(id, { resolve, reject, timer });
    ws.send(JSON.stringify({ id, method, params }));
  });
}
async function evaluate(expression) {
  const result = await rpc('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  assert(!result.exceptionDetails, JSON.stringify(result.exceptionDetails));
  return result.result.value;
}
async function until(expression) {
  for (let i = 0; i < 100; i++) {
    if (await evaluate(expression)) return;
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  throw Error('Timed out: ' + expression);
}
try {
  await rpc('Runtime.enable');
  await rpc('Page.enable');
  await rpc('Emulation.setDeviceMetricsOverride', { width: 1365, height: 1000, deviceScaleFactor: 1, mobile: false });
  await rpc('Page.navigate', { url: base });
  await until("document.getElementById('mode')?.textContent.includes('DEMO')");
  assert.equal(await evaluate("getComputedStyle(document.documentElement).getPropertyValue('--accent').trim()"),'#f247f5');
  assert.equal(await evaluate("document.body.textContent.includes('Room for good ideas')"),false);
  if (usageOnly) {
    const expected = initial.providers.map(p=>p.id);
    for (const path of ['', '/team', '/meetings']) {
      await rpc('Page.navigate', {url:base+path});
      await until("document.getElementById('account-usage')?.dataset.loaded==='true'");
      assert.deepEqual(await evaluate("[...document.querySelectorAll('#account-usage [data-agent]')].map(n=>n.dataset.agent)"), expected);
      assert.equal(await evaluate("document.getElementById('account-usage').textContent.includes('Account usage')"), true);
      if (!path) assert.equal(await evaluate("document.getElementById('account-usage').getBoundingClientRect().bottom <= document.querySelector('.metrics').getBoundingClientRect().top"),true);
      for (const width of [1365,850,390]) {
        await rpc('Emulation.setDeviceMetricsOverride',{width,height:1000,deviceScaleFactor:1,mobile:width<600});
        assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true);
      }
    }
    // Browser-only fixtures exercise fresh/stale/unknown rendering without changing stored telemetry.
    await rpc('Page.navigate',{url:base});
    await until("document.getElementById('account-usage')?.dataset.loaded==='true'");
    await evaluate(`globalThis.originalUsageFetch=fetch;globalThis.fetch=async (...args)=>args[0]==='/api/usage'?{ok:true,json:async()=>({demo:true,agents:[{id:'codex',name:'Codex · Astra',enabled:true,status:'ok',reason:'Fixture',fetched_at:1000,windows:[{bucket:'codex',used_percent:25,resets_at:19000,stale:false}]},{id:'muse',name:'Muse',enabled:true,status:'ok',reason:'Raw total',fetched_at:1000,windows:[],raw_metrics:[{label:'Total tokens',value:12345,unit:'tokens',stale:false}]},{id:'extra',name:'<img src=x onerror=alert(1)>',enabled:false,status:'unavailable',reason:'Unknown quota',windows:[]}]})}:originalUsageFetch(...args)`);
    await until("document.querySelector('#account-usage [data-agent=codex] .usage-value')?.textContent==='25% used'");
    assert.equal(await evaluate("document.querySelector('#account-usage [data-agent=muse] .usage-value').textContent"),'12,345 tokens');
    assert.equal(await evaluate("document.querySelectorAll('#account-usage img').length"),0);
    await evaluate(`globalThis.fetch=async (...args)=>args[0]==='/api/usage'?{ok:true,json:async()=>({demo:true,agents:[{id:'codex',name:'Codex · Astra',enabled:true,status:'stale',reason:'Old reading',windows:[{bucket:'codex',used_percent:25,resets_at:19000,stale:true}]}]})}:originalUsageFetch(...args)`);
    await until("document.querySelector('#account-usage .usage-stale')?.textContent.includes('stale')");
    await evaluate(`globalThis.fetch=async (...args)=>{if(args[0]==='/api/usage')throw Error('offline');return originalUsageFetch(...args)}`);
    await until("document.getElementById('account-usage').textContent.includes('connection unavailable')");
    await rpc('Page.reload');
    await until("document.getElementById('account-usage')?.dataset.loaded==='true'");
    const final=await (await fetch(base+'/api/state')).json();
    for(const key of ['manager_provider','objective','stage','paused','tasks','decisions','manager_starts','worker_starts','provider_usage','allowances']) assert.deepEqual(final.state[key],initial.state[key],key+' changed');
    console.log('Usage header passed: all three pages, all providers, five-hour labels, fresh/stale/unknown/offline states, safe text, responsive layout, counters unchanged.');
    await rpc('Emulation.setDeviceMetricsOverride',{width:1365,height:1000,deviceScaleFactor:1,mobile:false});
  } else if (managerOnly) {
    await rpc('Page.navigate',{url:base+'/team'});
    await until("document.getElementById('mode')?.textContent.includes('DEMO')");
    await until("document.querySelectorAll('#manager-provider option').length === 4");
    assert.equal(await evaluate("document.querySelector('#manager-provider option').textContent"), 'Grok');
    const previous = initial.state.manager_provider;
    try {
      await evaluate("document.getElementById('manager-provider').value='muse'; document.getElementById('manager-provider').dispatchEvent(new Event('change'))");
      await until("snapshot.state.manager_provider==='muse'");
      await rpc('Page.reload');
      await until("document.getElementById('manager-provider')?.value==='muse'");
    } finally {
      await evaluate("action('manager',{provider:"+JSON.stringify(previous)+"})");
    }
    const final=await (await fetch(base+'/api/state')).json();
    for(const key of ['manager_provider','objective','stage','paused','tasks','decisions','manager_starts','worker_starts','provider_usage','allowances']) assert.deepEqual(final.state[key],initial.state[key],key+' changed');
    console.log('Manager selection passed: Grok first, switch and reload persistence, original selection restored, no work launched or counters changed.');
  } else if (navigationOnly) {
    const geometry = "[...document.querySelectorAll('header nav a'), document.querySelector('header')].map(n=>{const r=n.getBoundingClientRect();return [r.x,r.y,r.width,r.height]})";
    for (const width of [1365,1100,850,600,390]) {
      await rpc('Emulation.setDeviceMetricsOverride',{width,height:1000,deviceScaleFactor:1,mobile:width<600});
      await rpc('Page.navigate',{url:base});
      await until("document.getElementById('mode')?.textContent.includes('DEMO') && document.querySelector('header .header-controls')");
      await until("getComputedStyle(document.querySelector('header')).display==='grid'");
      const workshop=await evaluate(geometry);
      assert.equal(await evaluate("document.querySelectorAll('.metrics .metric').length"),4);
      assert.equal(await evaluate("document.querySelector('.metrics').contains(document.getElementById('connect'))"),true);
      assert.equal(await evaluate("document.getElementById('providers')===null && document.getElementById('manager-provider')===null"),true);
      assert.equal(await evaluate("document.getElementById('events')!==null"),true);
      await evaluate("document.querySelector('header a[href=\"/team\"]').click()");
      await until("document.getElementById('providers') && document.getElementById('mode').textContent.includes('DEMO')");
      assert.deepEqual(await evaluate(geometry),workshop,'Team navigation/header shifted at '+width+'px');
      assert.equal(await evaluate("[...document.querySelectorAll('h2')].some(h=>h.textContent==='Capacity for the project')"),true);
      assert.equal(await evaluate("document.getElementById('a-manager_turns').closest('details')===null"),true);
      assert.equal(await evaluate("document.getElementById('events')===null"),true);
      assert.equal(await evaluate("getComputedStyle(document.querySelector('header .header-controls')).visibility"),'hidden');
      await evaluate("document.querySelector('header a[href=\"/meetings\"]').click()");
      await until("document.getElementById('new-title') && document.getElementById('mode').textContent.includes('DEMO')");
      assert.deepEqual(await evaluate(geometry),workshop,'Navigation/header shifted at '+width+'px');
      assert.equal(await evaluate("getComputedStyle(document.querySelector('header .header-controls')).visibility"),'hidden');
      assert.equal(await evaluate("document.querySelector('header .header-controls').inert"),true);
      assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true);
    }
    const final=await (await fetch(base+'/api/state')).json();
    for(const key of ['objective','tasks','manager_starts','worker_starts','allowances']) assert.deepEqual(final.state[key],initial.state[key]);
    await rpc('Emulation.setDeviceMetricsOverride',{width:1365,height:1000,deviceScaleFactor:1,mobile:false});
    console.log('Navigation checks passed: identical link positions and header dimensions on all three pages at five widths; hidden controls inert; state untouched.');
  } else if (meetingsOnly) {
    await evaluate("document.querySelector('a[href=\"/meetings\"]').click()");
    await until("document.getElementById('new-title') && document.getElementById('mode').textContent.includes('DEMO')");
    await evaluate("document.getElementById('new-title').value='Meeting browser trial'; document.getElementById('new-meeting').requestSubmit()");
    await until("document.getElementById('meeting-title').textContent==='Meeting browser trial'");
    const id=await evaluate('selected');
    await evaluate("document.getElementById('context-notes').value='Pinned test context'; document.getElementById('question').value='How can we improve the experiment?'; document.getElementById('ask').requestSubmit()");
    await until("document.querySelectorAll('#chat .message-status').length===5 && [...document.querySelectorAll('#chat .message-status')].filter(n=>n.textContent==='answered').length===4");
    const meeting=await (await fetch(base+'/api/meetings/'+id)).json();
    assert.deepEqual(meeting.messages.map(m=>m.speaker),['Len','grok','muse','qwen','codex']);
    assert.equal(meeting.messages[4].prompt.match(/\[answered\]/g).length,3);
    assert(meeting.messages[4].prompt.includes('Pinned test context'));
    await rpc('Page.reload');
    await until("document.querySelectorAll('#chat .message-status').length===5");
    assert.equal(await evaluate("document.getElementById('context-notes').value"),'Pinned test context');
    const final=await (await fetch(base+'/api/state')).json();
    for(const key of ['stage','objective','tasks','decisions','thread_id']) assert.deepEqual(final.state[key],initial.state[key]);
    assert.equal(final.state.worker_starts.length,initial.state.worker_starts.length+3);
    assert.equal(final.state.manager_starts.length,initial.state.manager_starts.length+1);
    assert.equal(await evaluate("getComputedStyle(document.getElementById('error')).display"),'none');
    console.log('Meeting browser checks passed: ordered replies, shared context, reload persistence, isolated work queue, shared counters.');
  } else if (providersOnly) {
    await rpc('Page.navigate',{url:base+'/team'});
    await until("document.getElementById('mode')?.textContent.includes('DEMO')");
    assert.deepEqual(initial.providers.map(p=>p.id).sort(), ['codex','grok','muse','qwen']);
    await until("document.querySelectorAll('#providers [data-provider]').length === 4");
    const muse = initial.providers.find(p=>p.id==='muse'), grok = initial.providers.find(p=>p.id==='grok');
    const selector = "document.querySelector('[data-provider=\"muse\"] [data-action=\"toggle\"]')";
    await evaluate(selector+'.click()');
    await until('snapshot.providers.find(p=>p.id===\'muse\').enabled === '+JSON.stringify(!muse.enabled));
    await evaluate("const card=document.querySelector('[data-provider=\"grok\"]'); card.querySelector('input').value=1; card.querySelector('textarea').value='Temporary browser-test role'; [...card.querySelectorAll('button')].find(b=>b.textContent==='Save agent').click()");
    await until("snapshot.providers.find(p=>p.id==='grok').max_runs === 1 && snapshot.providers.find(p=>p.id==='grok').description === 'Temporary browser-test role'");
    await rpc('Page.reload');
    await until("typeof snapshot!=='undefined' && snapshot?.providers?.find(p=>p.id==='grok').max_runs === 1");
    assert.equal(await evaluate("snapshot.providers.find(p=>p.id==='muse').enabled"), !muse.enabled);
    assert.equal(await evaluate("document.querySelector('[data-provider=\"muse\"] [data-action=\"toggle\"]').textContent"), muse.enabled?'Enable':'Disable');
    // Restore effective settings, never alter an allowance counter or start inference.
    await evaluate(selector+'.click()');
    await until('snapshot.providers.find(p=>p.id===\'muse\').enabled === '+JSON.stringify(muse.enabled));
    await evaluate("const card=document.querySelector('[data-provider=\"grok\"]'); card.querySelector('input').value="+grok.max_runs+"; card.querySelector('textarea').value="+JSON.stringify(grok.description)+"; [...card.querySelectorAll('button')].find(b=>b.textContent==='Save agent').click()");
    await until("snapshot.providers.find(p=>p.id==='grok').max_runs === "+grok.max_runs+" && snapshot.providers.find(p=>p.id==='grok').description === "+JSON.stringify(grok.description));
    const final = await (await fetch(base + '/api/state')).json();
    for (const key of ['objective','stage','paused','manager_starts','worker_starts','provider_usage','allowances']) assert.deepEqual(final.state[key],initial.state[key],key+' changed during provider edits');
    assert.equal(await evaluate("document.getElementById('error').style.display"),'none');
    console.log('Provider browser checks passed: Team roster, editable role, enable/disable, per-provider caps, reload persistence, unchanged run counters.');
  } else if (snapshotsOnly) {
    await evaluate("document.getElementById('capture-snapshot').click()");
    await until("!document.getElementById('snapshot-inspector').hidden && selectedSnapshot !== null");
    const baseline = await evaluate('selectedSnapshot');
    const original = await (await fetch(base + '/api/snapshots/' + baseline)).json();
    await evaluate("const editor=document.getElementById('snapshot-recipe'); editor.closest('details').open=true; const candidate=JSON.parse(editor.value); candidate.label='More focused briefs (candidate)'; candidate.worker_instructions+='\\nReport uncertainty before attempting a broad repair.'; candidate.config.allowances.manager_turns=1; editor.value=JSON.stringify(candidate,null,2); document.getElementById('fork-snapshot').click()");
    await until("document.getElementById('snapshot-title').textContent.startsWith('candidate')");
    const candidate = await evaluate('selectedSnapshot');
    assert.notEqual(candidate, baseline);
    const diff = await (await fetch(base + '/api/snapshots/' + candidate + '/diff')).json();
    assert(diff.recipe_changed && diff.changes.some(c => c.path === 'recipe/worker.md'));
    const unchanged = await (await fetch(base + '/api/snapshots/' + baseline)).json();
    assert.deepEqual(unchanged, original);
    await evaluate("document.getElementById('snapshot-note').closest('details').open=true; document.getElementById('snapshot-note').value='Candidate saved for a future comparable trial; no performance claim yet.'; document.getElementById('evaluate-snapshot').click()");
    await until("document.getElementById('snapshot-title').textContent.startsWith('evaluation')");
    const evaluation = await evaluate('selectedSnapshot');
    const bundle = await (await fetch(base + '/api/snapshots/' + evaluation + '/export')).json();
    for (const [id,value] of Object.entries(bundle.objects)) assert.equal(createHash('sha256').update(Buffer.from(value,'base64')).digest('hex'),id);
    assert(bundle.manifests[baseline] && bundle.manifests[candidate]);
    await mkdir('.firm/exports',{recursive:true});
    const exportPath=resolve('.firm/exports/snapshot-smoke.json'); await writeFile(exportPath,JSON.stringify(bundle));
    await evaluate('inspectSnapshot('+JSON.stringify(baseline)+')');
    await rpc('DOM.enable'); const document=await rpc('DOM.getDocument');
    const input=await rpc('DOM.querySelector',{nodeId:document.root.nodeId,selector:'#snapshot-file'});
    await rpc('DOM.setFileInputFiles',{nodeId:input.nodeId,files:[exportPath]});
    await until('selectedSnapshot === '+JSON.stringify(evaluation));
    const final = await (await fetch(base + '/api/state')).json();
    for (const key of ['objective','stage','paused','manager_starts','worker_starts','allowances']) assert.deepEqual(final.state[key],initial.state[key],key+' changed during snapshot operations');
    assert.equal(await evaluate("document.getElementById('error').style.display"),'none');
    console.log('Snapshot browser checks passed: capture, immutable fork, hash diff, evaluation, export, upload/import, unchanged controller counters.');
  } else {
  await evaluate("document.getElementById('objective').value = 'Try one bounded assignment and capture a useful worker discovery'; document.getElementById('save-objective').click()");
  await until("document.getElementById('stage').textContent === 'READY PLAN'");
  await evaluate("document.getElementById('start').click()");
  await until("document.getElementById('stage').textContent === 'COMPLETE'");
  assert.match(await evaluate("document.getElementById('manager-count').textContent"), /^2 \/ 4$/);
  assert.match(await evaluate("document.getElementById('accepted').textContent"), /^1 \/ 1$/);
  assert(await evaluate("document.getElementById('decisions').textContent.includes('adopted')"));
  }
  assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true);
  await mkdir('.firm/screenshots', { recursive: true });
  const desktop = await rpc('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true });
  const screenshotPrefix = usageOnly ? 'usage-' : managerOnly ? 'manager-' : navigationOnly ? 'navigation-' : meetingsOnly ? 'meetings-' : providersOnly ? 'providers-' : snapshotsOnly ? 'snapshots-' : '';
  await writeFile('.firm/screenshots/'+screenshotPrefix+'desktop.png', Buffer.from(desktop.data, 'base64'));
  await rpc('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
  assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true);
  const mobile = await rpc('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true });
  await writeFile('.firm/screenshots/'+screenshotPrefix+'mobile.png', Buffer.from(mobile.data, 'base64'));
  assert.deepEqual(exceptions, []);
  console.log(usageOnly ? 'Usage dashboard: desktop/mobile layout passed, no runtime exceptions.' : managerOnly ? 'Manager dashboard: desktop/mobile layout passed, no runtime exceptions.' : navigationOnly ? 'Navigation layout passed, no runtime exceptions.' : meetingsOnly ? 'Meeting dashboard: desktop/mobile layout passed, no runtime exceptions.' : providersOnly ? 'Provider dashboard: desktop/mobile layout passed, no runtime exceptions.' : snapshotsOnly ? 'Snapshot dashboard: desktop/mobile layout passed, no runtime exceptions.' : 'Browser smoke passed: full demo cycle, accepted result, captured discovery, desktop/mobile layout, no runtime exceptions.');
} finally { ws.close(); }
