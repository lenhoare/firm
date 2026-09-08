// Shared by Workshop, Team and Meetings. Reads cached telemetry only; never starts work.
// Renders the CRT "CAPACITY" gutter: one row per provider — name · 2-digit percent
// · a 12-glyph bar. Reset times and checked-at timestamps live in the row tooltip.
(() => {
  const root = document.getElementById('account-usage');
  if (!root) return;
  const make = (tag, text, className) => {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (className) node.className = className;
    return node;
  };
  const GLYPHS = 12;
  // Highest known usage window for an agent, or null when nothing is readable.
  function agentPercent(agent) {
    let pct = null;
    for (const w of agent.windows || []) {
      if (typeof w.used_percent === 'number') pct = Math.max(pct ?? 0, w.used_percent);
    }
    return pct;
  }
  function tooltip(agent) {
    const lines = [agent.name + (agent.enabled ? '' : ' · disabled'), agent.reason].filter(Boolean);
    for (const w of agent.windows || []) {
      const reset = w.resets_at ? new Date(w.resets_at * 1000).toLocaleString(undefined, {month:'short',day:'numeric',hour:'2-digit',minute:'2-digit'}) : (w.reset_label || 'reset unavailable');
      lines.push((w.label || w.bucket) + ': ' + w.used_percent.toLocaleString(undefined,{maximumFractionDigits:1}) + '% used' + (w.stale ? ' (stale)' : '') + ' · reset ' + reset);
    }
    for (const m of agent.raw_metrics || []) {
      lines.push(m.label + ': ' + Number(m.value).toLocaleString() + (m.unit ? ' ' + m.unit : '') + (m.stale ? ' (stale)' : ''));
    }
    if (agent.fetched_at) lines.push('Checked ' + new Date(agent.fetched_at * 1000).toLocaleTimeString());
    return lines.join('\n');
  }
  function bar(pct) {
    const filled = pct == null ? 0 : Math.max(0, Math.min(GLYPHS, Math.round(pct / 100 * GLYPHS)));
    const wrap = make('span', undefined, 'crt-bar');
    if (filled) wrap.append(make('span', '▌'.repeat(filled), 'crt-bar-filled'));
    if (filled < GLYPHS) wrap.append(make('span', '▌'.repeat(GLYPHS - filled), 'crt-bar-empty'));
    return wrap;
  }
  let busy = false, previous = '';
  async function refresh() {
    if (busy) return;
    busy = true;
    try {
      const response = await fetch('/api/usage', {cache:'no-store', signal:AbortSignal.timeout(8000)});
      if (!response.ok) throw Error('Usage unavailable');
      const data = await response.json();
      const key = JSON.stringify([data.agents, data.demo]);
      if (key === previous) return;
      previous = key;
      root.replaceChildren();
      // Short ids keep the monospace grid tight in the 190px gutter; full names live in the tooltip.
      const width = data.agents.reduce((n, a) => Math.max(n, a.id.length), 0);
      for (const agent of data.agents) {
        const pct = agentPercent(agent);
        const known = pct != null || (agent.raw_metrics || []).length;
        const row = make('div', undefined, 'cap-row');
        row.dataset.agent = agent.id;
        row.title = tooltip(agent);
        row.append(make('span', agent.id.padEnd(width, ' ') + ' ', 'cap-name'));
        const shown = pct == null ? '--' : String(Math.round(pct)).padStart(2, '0');
        row.append(make('span', shown + ' ', 'cap-pct' + (pct === 0 || pct == null ? ' zero' : '')));
        row.append(bar(known ? (pct ?? 0) : 0));
        root.append(row);
      }
      if (!data.agents.length) root.append(make('div', 'no providers', 'last-line'));
      root.dataset.loaded = 'true';
    } catch {
      // Never leave a previously fresh percentage looking current after disconnection.
      previous = '';
      root.replaceChildren(make('div', 'readings unavailable', 'last-line usage-stale'));
      delete root.dataset.loaded;
    } finally { busy = false; }
  }
  refresh();
  setInterval(refresh, 5000);
})();
