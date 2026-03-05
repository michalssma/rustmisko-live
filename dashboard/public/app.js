'use strict';
/* ── app.js — Dashboard frontend logic ── */

let ws;
let lastData = null;
let pendingOpen = false;

// ── WebSocket ─────────────────────────────────────────────────────────────────
function connectWs() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  ws = new WebSocket(`${proto}://${location.host}`);

  ws.addEventListener('open', () => setDot('conn-dot', 'green'));
  ws.addEventListener('close', () => {
    setDot('conn-dot', 'red');
    setTimeout(connectWs, 3000); // auto-reconnect
  });
  ws.addEventListener('error', () => setDot('conn-dot', 'yellow'));
  ws.addEventListener('message', e => {
    try {
      const msg = JSON.parse(e.data);
      if (msg.type === 'status') { lastData = msg.data; render(msg.data); }
    } catch {}
  });
}

// ── Render ────────────────────────────────────────────────────────────────────
function render(d) {
  // Balance
  if (d.balance_usd != null) {
    setText('balance', `$${d.balance_usd.toFixed(2)}`);
  }

  // Health chips
  renderChip('chip-ws',   d.health.ws_age_ms,  5000,  30000);
  renderChip('chip-gql',  d.health.gql_age_ms, 6000,  15000);
  renderChip('chip-exec', d.health.executor_ok ? 100 : -1, 5000, 10000);
  renderBotChip(d.health.alert_bot_age_ms);

  // Process chips
  renderProc('proc-feed-hub',  d.processes['feed-hub']);
  renderProc('proc-alert-bot', d.processes['alert-bot']);
  renderProc('proc-executor',  d.processes['executor']);

  // Stats
  const pnl = d.pnl_today || 0;
  const pnlEl = document.getElementById('pnl-today');
  pnlEl.textContent = (pnl >= 0 ? '+' : '') + `$${pnl.toFixed(2)}`;
  pnlEl.className = 'stat-value ' + (pnl >= 0 ? 'pos' : 'neg');

  setText('win-rate', d.win_rate != null ? `${d.win_rate}%` : '—');
  setText('bets-today', d.bets_today != null ? String(d.bets_today) : '—');

  // Pending count & list
  const count = d.pending_count || 0;
  setText('pending-count', String(count));
  renderPendingList(d.pending || []);

  // Bets tab
  renderBetsList(d.recent_bets || []);

  // Stats tab
  renderStats(d);
}

// ── Health chip ───────────────────────────────────────────────────────────────
function renderChip(id, ageMs, greenMs, redMs) {
  const el = document.getElementById(id);
  if (!el) return;
  if (ageMs < 0) { el.className = 'chip grey'; el.textContent = 'N/A'; return; }
  if (ageMs < greenMs)  { el.className = 'chip green';  el.textContent = fmtAge(ageMs); }
  else if (ageMs < redMs) { el.className = 'chip yellow'; el.textContent = fmtAge(ageMs); }
  else                  { el.className = 'chip red';    el.textContent = fmtAge(ageMs); }
}

function renderBotChip(ageMs) {
  const el = document.getElementById('chip-bot');
  if (!el) return;
  if (ageMs < 0)         { el.className = 'chip grey';   el.textContent = 'N/A'; }
  else if (ageMs < 30000)  { el.className = 'chip green';  el.textContent = fmtAge(ageMs); }
  else if (ageMs < 120000) { el.className = 'chip yellow'; el.textContent = fmtAge(ageMs); }
  else                   { el.className = 'chip red';    el.textContent = fmtAge(ageMs); }
}

function fmtAge(ms) {
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

// ── Process chip ──────────────────────────────────────────────────────────────
function renderProc(id, status) {
  const el = document.getElementById(id);
  if (!el) return;
  const dot = el.querySelector('.dot');
  if (!dot) return;
  if (status === 'running')  dot.className = 'dot green';
  else if (status === 'stopped') dot.className = 'dot red';
  else                       dot.className = 'dot yellow';
}

// ── Pending bets list ─────────────────────────────────────────────────────────
function renderPendingList(bets) {
  const el = document.getElementById('pending-list');
  if (!el) return;
  if (bets.length === 0) { el.innerHTML = '<div style="padding:12px;color:var(--muted);text-align:center">Žádné pending bety</div>'; return; }
  el.innerHTML = bets.map((b, i) => `
    <div class="pending-item" onclick="showBetModal(${i})">
      <div class="bet-match">${escHtml(shortMatchKey(b.match_key || b.condition_id || '?'))}</div>
      <div class="bet-meta">
        <div>$${+(b.stake || b.amount_usd || 0).toFixed(2)} @ ${+(b.odds || 0).toFixed(2)}</div>
        <div style="color:var(--info)">${b.path || ''}</div>
      </div>
    </div>
  `).join('');
}

function showBetModal(index) {
  const bets = (lastData?.pending || []);
  const b = bets[index];
  if (!b) return;
  document.getElementById('modal-title').textContent = shortMatchKey(b.match_key || '?');
  const rows = [
    ['Match', b.match_key || '—'],
    ['Stake', `$${+(b.stake || b.amount_usd || 0).toFixed(4)}`],
    ['Odds', b.odds || '—'],
    ['Path', b.path || '—'],
    ['Edge', b.edge_pct ? `${b.edge_pct.toFixed(1)}%` : '—'],
    ['Condition ID', (b.condition_id || '—').slice(0, 20) + '…'],
    ['Outcome ID', b.outcome_id || '—'],
    ['Time', b.ts ? new Date(b.ts).toLocaleTimeString('cs') : '—'],
  ];
  document.getElementById('modal-body').innerHTML = rows.map(([k, v]) =>
    `<div class="modal-row"><span class="modal-row-key">${k}</span><span>${escHtml(String(v))}</span></div>`
  ).join('');
  document.getElementById('modal').classList.remove('hidden');
}

function closeModal() {
  document.getElementById('modal').classList.add('hidden');
}

// ── Bets list ─────────────────────────────────────────────────────────────────
function renderBetsList(bets) {
  const el = document.getElementById('bets-list');
  if (!el) return;
  if (bets.length === 0) { el.innerHTML = '<div style="padding:14px;color:var(--muted)">Žádné bety</div>'; return; }
  el.innerHTML = bets.map(b => {
    const profit = b.profit_usd != null ? b.profit_usd
      : b.event === 'WON' ? (b.payout_usd || 0) - (b.amount_usd || 0)
      : b.event === 'LOST' ? -(b.amount_usd || 0) : null;
    const profitStr = profit != null
      ? `<span class="bet-profit ${profit >= 0 ? 'pos' : 'neg'}">${profit >= 0 ? '+' : ''}$${profit.toFixed(2)}</span>`
      : '';
    const time = b.ts ? new Date(b.ts).toLocaleTimeString('cs', { hour: '2-digit', minute: '2-digit' }) : '';
    return `
      <div class="bet-row">
        <span class="bet-result ${b.event}">${b.event}</span>
        <div class="bet-info">
          <div class="bet-match-name">${escHtml(shortMatchKey(b.match_key || '?'))}</div>
          <div class="bet-detail">$${+(b.amount_usd || b.stake || 0).toFixed(2)} @ ${+(b.odds || 0).toFixed(2)} · ${b.path || ''} · ${time}</div>
        </div>
        ${profitStr}
      </div>
    `;
  }).join('');
}

// ── Stats tab ─────────────────────────────────────────────────────────────────
function renderStats(d) {
  const el = document.getElementById('stats-content');
  if (!el) return;
  const bets = (d.recent_bets || []);
  const settled = bets.filter(b => b.event === 'WON' || b.event === 'LOST');
  const won     = settled.filter(b => b.event === 'WON');
  const lost    = settled.filter(b => b.event === 'LOST');
  const totalStake = settled.reduce((s, b) => s + (b.amount_usd || b.stake || 0), 0);
  const totalPnl   = settled.reduce((s, b) => {
    if (b.event === 'WON')  return s + ((b.payout_usd || 0) - (b.amount_usd || b.stake || 0));
    if (b.event === 'LOST') return s - (b.amount_usd || b.stake || 0);
    return s;
  }, 0);
  const avgOdds = settled.length ? settled.reduce((s, b) => s + (b.odds || 0), 0) / settled.length : 0;
  const wr = settled.length ? (won.length / settled.length * 100) : 0;

  el.innerHTML = `
    <div class="stats-grid">
      <div class="stats-item"><div class="stats-key">Výhry / Prohry</div><div class="stats-val">${won.length} / ${lost.length}</div></div>
      <div class="stats-item"><div class="stats-key">Win rate</div><div class="stats-val ${wr >= 55 ? 'pos' : wr >= 45 ? '' : 'neg'}" style="${wr>=55?'color:var(--green)':wr>=45?'':'color:var(--red)'}">${wr.toFixed(1)}%</div></div>
      <div class="stats-item"><div class="stats-key">Celk. P&L</div><div class="stats-val ${totalPnl>=0?'pos':'neg'}" style="${totalPnl>=0?'color:var(--green)':'color:var(--red)'}">${totalPnl>=0?'+':''}$${totalPnl.toFixed(2)}</div></div>
      <div class="stats-item"><div class="stats-key">Celk. stake</div><div class="stats-val">$${totalStake.toFixed(2)}</div></div>
      <div class="stats-item"><div class="stats-key">Avg odds</div><div class="stats-val">${avgOdds.toFixed(2)}</div></div>
      <div class="stats-item"><div class="stats-key">ROI</div><div class="stats-val ${totalPnl>=0?'pos':'neg'}" style="${totalPnl>=0?'color:var(--green)':'color:var(--red)'}">${totalStake>0?(totalPnl/totalStake*100).toFixed(1):0}%</div></div>
    </div>
    <div style="margin-top:14px;color:var(--muted);font-size:11px">Z posledních ${settled.length} settled betů</div>
    <div style="margin-top:6px;color:var(--muted);font-size:11px">Break-even WR při avg odds ${avgOdds.toFixed(2)}: ${avgOdds>0?(1/avgOdds*100).toFixed(1):0}%</div>
  `;
}

// ── Controls ──────────────────────────────────────────────────────────────────
async function startProc(name) {
  if (!confirm(`Spustit ${name}?`)) return;
  const r = await api('POST', `/api/process/start/${name}`);
  alert(r.ok ? `${name} spuštěn (PID ${r.pid || '?'})` : `Chyba: ${r.error}`);
}

async function stopProc(name) {
  if (!confirm(`Zastavit ${name}?`)) return;
  const r = await api('POST', `/api/process/stop/${name}`);
  alert(r.ok ? `${name} zastaven` : `Chyba: ${r.error}`);
}

async function killAll() {
  if (!confirm('⚠️ EMERGENCY STOP: zastavit všechny procesy?\n\nToto zastaví ALL auto-bety.')) return;
  const r = await api('POST', '/api/killall');
  alert(r.ok ? 'Všechny procesy zastaveny.' : `Chyba: ${r.error}`);
}

async function loadLog(name) {
  const el = document.getElementById('log-view');
  el.textContent = 'Načítám...';
  try {
    const r = await api('GET', `/api/log/${name}`);
    el.textContent = (r.lines || []).slice(-50).join('\n') || '(prázdný log)';
    el.scrollTop = el.scrollHeight;
  } catch { el.textContent = 'Chyba načtení logu'; }
}

// ── Tab switching ─────────────────────────────────────────────────────────────
function switchTab(name, btn) {
  document.querySelectorAll('.tab').forEach(t => t.classList.add('hidden'));
  document.getElementById(`tab-${name}`).classList.remove('hidden');
  document.querySelectorAll('.tabnav-btn').forEach(b => b.classList.remove('active'));
  btn.classList.add('active');
}

// ── Pending toggle ────────────────────────────────────────────────────────────
function togglePending() {
  pendingOpen = !pendingOpen;
  document.getElementById('pending-list').classList.toggle('hidden', !pendingOpen);
  document.getElementById('pending-chevron').style.transform = pendingOpen ? 'rotate(180deg)' : '';
}

// ── Helpers ───────────────────────────────────────────────────────────────────
function setText(id, val) {
  const el = document.getElementById(id);
  if (el) el.textContent = val;
}

function setDot(id, color) {
  const el = document.getElementById(id);
  if (el) el.className = `dot ${color}`;
}

function shortMatchKey(key) {
  // "esports::circleinner_vs_friendlycampers::map3_winner" → "circleinner vs friendlycampers (map3)"
  const parts = key.split('::');
  let match = parts[1] || key;
  match = match.replace(/_vs_/g, ' vs ').replace(/_/g, ' ');
  const market = parts[2] ? ` (${parts[2].replace('_winner','').replace(/_/g,' ')})` : '';
  return match + market;
}

function escHtml(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}

async function api(method, url, body) {
  const opts = { method, credentials: 'same-origin', headers: {} };
  if (body) { opts.headers['Content-Type'] = 'application/json'; opts.body = JSON.stringify(body); }
  const r = await fetch(url, opts);
  return r.json();
}

// ── Init ──────────────────────────────────────────────────────────────────────
connectWs();
