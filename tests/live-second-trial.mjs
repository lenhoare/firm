// Explicit live acceptance trial. This spends provider allowance: never run in CI.
import assert from 'node:assert/strict';

const base = 'http://127.0.0.1:7433';
const ids = ['grok', 'qwen', 'muse'];

async function api(path, body) {
  const response = await fetch(`${base}/api/${path}`, {
    ...(body === undefined
      ? {}
      : {
          method: 'POST',
          headers: {
            'Content-Type': 'application/json',
            'X-Firm-Control': '1',
          },
          body: JSON.stringify(body),
        }),
    signal: AbortSignal.timeout(20_000),
  });
  const text = await response.text();
  let data = {};
  try {
    data = text ? JSON.parse(text) : {};
  } catch {
    throw new Error(`${path}: ${response.status} ${text}`);
  }
  if (!response.ok) throw new Error(`${path}: ${data.error || response.status}`);
  return data;
}

async function state() {
  return api('state');
}

async function waitUntilIdle(timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  let view = await state();
  while (view.active && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 1_000));
    view = await state();
  }
  assert(!view.active, 'Firm did not become idle before the control timeout');
  return view;
}

let initial = await state();
assert(!initial.state.demo && initial.state.paused && !initial.active);
assert(initial.workspace.endsWith('/examples/taskboard'));
assert.equal(initial.state.allowances.worker_max_turns, 64);
assert.equal(initial.state.allowances.worker_max_tool_calls, 64);

if (!['idle', 'complete', 'cancelled'].includes(initial.state.stage)) {
  await api('stop', {});
  initial = await waitUntilIdle();
}

for (const id of ids) await api(`providers/${id}`, {enabled: true, max_runs: 6});
await api('manager', {provider: 'grok'});

const objective = `Second live multi-provider acceptance trial: finish the dependency-free Rust task-board library in this workspace.

The public API and acceptance tests already define the required behavior. Do not change src/lib.rs, src/model.rs, Cargo.toml, or tests. Do not add dependencies, use networking, install anything, invoke other agents, or edit outside this workspace.

Use exactly one bounded assignment per worker, in this order:
1. Grok: implement src/parser.rs only. Parse the documented four pipe-delimited fields, ignore blank/comment lines, retain 1-indexed source lines, and collect every invalid row as a ParseIssue in line order. Run the parser-focused acceptance tests if possible.
2. Qwen: implement src/analytics.rs only. Compute board and per-owner totals and select the next task using status priority (doing before todo; never done), then fewer minutes, then earlier source line. Run the analytics-focused acceptance tests if possible.
3. Muse: implement src/report.rs only. Render the exact deterministic report pinned by tests, including the empty board and trailing newline. Run the report-focused tests and then the full suite if possible.

Participation by all three workers is mandatory. Do not retry or substitute a provider. The controller runs the full suite after every assignment, so the first two tasks may be marked failed solely because later modules are still stubs; treat that expected intermediate failure as evidence to inspect, not a reason to abandon the sequence. Record genuine failures and continue to the next distinct role. Use recent decisions to track which role is next. Do not declare completion until all three workers were attempted and the final controller verification passes. Otherwise return blocked after the third role. At most four manager turns and three worker runs; no repair loop.`;

const baseline = await api('snapshots', {
  label: 'Before second live trial · taskboard stubs · six runs per provider',
});
await api('objective', {objective});
const configured = await api('snapshots', {
  label: 'Second live trial · Grok manager · three taskboard roles',
});
console.log(
  JSON.stringify({
    event: 'trial-configured',
    baseline: baseline.id,
    snapshot: configured.id,
    manager: 'grok',
    provider_caps: Object.fromEntries(ids.map((id) => [id, 6])),
  }),
);

await api('start', {});
const deadline = Date.now() + 35 * 60_000;
let last = '';
let ended = false;

while (Date.now() < deadline) {
  const view = await state();
  const s = view.state;
  const report = {
    stage: s.stage,
    paused: s.paused,
    active: view.active,
    reason: s.reason,
    manager_runs: s.manager_starts.length,
    worker_runs: s.worker_starts.length,
    tasks: s.tasks.map((task) => ({
      provider: task.assignment.provider,
      title: task.assignment.title,
      status: task.status,
      exit_code: task.exit_code,
    })),
    latest_decision: s.decisions.at(-1)?.summary,
  };
  const key = JSON.stringify({
    ...report,
    reason: report.reason.replace(/Next manager turn in \d+s/, 'Waiting for manager spacing'),
  });
  if (key !== last) {
    console.log(JSON.stringify(report));
    last = key;
  }
  if (!view.active && ['complete', 'blocked', 'cancelled'].includes(s.stage)) {
    console.log(
      JSON.stringify({
        event: 'trial-ended',
        stage: s.stage,
        manager_runs: s.manager_starts.length,
        worker_runs: s.worker_starts.length,
        tasks: report.tasks,
        decisions: s.decisions.map((decision) => ({
          manager: decision.manager_provider,
          action: decision.action,
          summary: decision.summary,
        })),
      }),
    );
    ended = true;
    break;
  }
  if (s.paused && !view.active) break;
  await new Promise((resolve) => setTimeout(resolve, 2_000));
}

if (!ended) {
  let view = await state();
  if (view.active || !view.state.paused) await api('stop', {});
  view = await waitUntilIdle();
  console.log(
    JSON.stringify({
      event: 'trial-stopped',
      stage: view.state.stage,
      reason: view.state.reason,
    }),
  );
}

const final = await state();
if (!final.state.paused) await api('pause', {});
await api('snapshots', {label: `After second live trial · ${final.state.stage}`});
