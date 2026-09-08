// Shared by Workshop and Meetings. Reads cached telemetry only; never starts work.
(() => {
  const root = document.getElementById('account-usage');
  if (!root) return;
  const make = (tag, text, className) => {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (className) node.className = className;
    return node;
  };
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
      const heading = make('div', undefined, 'account-usage-heading');
      heading.append(make('h2', 'Account usage'), make('span', 'Provider readings · % used where a limit is known', 'small muted'));
      root.append(heading);
      const cards = make('div', undefined, 'account-usage-cards');
      for (const agent of data.agents) {
        const card = make('article', undefined, 'account-usage-card');
        card.dataset.agent = agent.id;
        card.title = agent.reason;
        card.append(make('h3', agent.name + (agent.enabled ? '' : ' · disabled')));
        if (!agent.windows.length && !(agent.raw_metrics || []).length) {
          card.append(make('strong', 'Unknown', 'usage-value muted'), make('p', agent.reason, 'usage-detail muted'));
        } else {
          for (const window of agent.windows) {
            const row = make('div', undefined, 'account-usage-window');
            const value = window.used_percent.toLocaleString(undefined, {maximumFractionDigits:1}) + '% used';
            row.append(make('strong', value + (window.stale ? ' · stale' : ''), 'usage-value' + (window.stale ? ' usage-stale' : '')));
            row.append(make('span', window.label || window.bucket, 'usage-detail muted'));
            const progress = make('progress');
            progress.max = 100;
            progress.value = window.used_percent;
            progress.setAttribute('aria-label', agent.name + ' ' + (window.label || window.bucket) + ' used' + (window.stale ? ' (stale reading)' : ''));
            row.append(progress);
            const reset = window.resets_at ? new Date(window.resets_at * 1000) : null;
            const resetText = reset ? 'Reset: ' + reset.toLocaleString(undefined, {month:'short',day:'numeric',hour:'2-digit',minute:'2-digit'}) : window.reset_label ? 'Reset: ' + window.reset_label : 'Reset time unavailable';
            row.append(make('span', resetText, 'usage-detail muted'));
            card.append(row);
          }
          for (const metric of agent.raw_metrics || []) {
            const value = Number(metric.value).toLocaleString() + (metric.unit ? ' ' + metric.unit : '');
            card.append(make('strong', value + (metric.stale ? ' · stale' : ''), 'usage-value' + (metric.stale ? ' usage-stale' : '')),
              make('span', metric.label, 'usage-detail muted'));
          }
          if (agent.fetched_at) card.append(make('span', 'Checked ' + new Date(agent.fetched_at * 1000).toLocaleTimeString(), 'usage-detail muted'));
        }
        cards.append(card);
      }
      root.append(cards);
      root.dataset.loaded = 'true';
    } catch {
      // Never leave a previously fresh percentage looking current after disconnection.
      previous = '';
      root.replaceChildren(make('h2', 'Account usage'), make('p', 'Usage connection unavailable — readings cannot be refreshed.', 'small usage-stale'));
      delete root.dataset.loaded;
    } finally { busy = false; }
  }
  refresh();
  setInterval(refresh, 5000);
})();
