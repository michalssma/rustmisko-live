'use strict';
/* ── app.js — Dashboard frontend logic (v2 — iOS-quality UX) ── */

let ws;
let lastData          = null;
let activeBetFilter   = 'all';
let activeStatsPeriod = 1;
let currentSportFocus = ['all'];
let wsConnected       = false;
let renderCount       = 0;

// ── Fast prefetch via REST (instant load instead of waiting for WS) ───────────
(async function prefetch() {
  const el = document.getElementById('loading-status');
  try {
    if (el) el.textContent = 'Načítání dat…';
    const r = await fetch('/api/status', { credentials: 'same-origin' });
    if (r.status === 401) { window.location.href = '/login.html'; return; }
    const data = await r.json();
    lastData = data;
    render(data);
    if (el) { el.textContent = 'Připojeno'; el.classList.add('connected'); }
    dismissLoading();
  } catch {
    if (el) el.textContent = 'Čekání na server…';
  }
})();

function dismissLoading() {
  const overlay = document.getElementById('loading-overlay');
  if (!overlay) return;
  overlay.classList.add('fade-out');
  setTimeout(() => overlay.style.display = 'none', 450);
}

// ── WebSocket (real-time updates) ─────────────────────────────────────────────
function connectWs() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  ws = new WebSocket(`${proto}://${location.host}`);

  ws.addEventListener('open', () => {
    wsConnected = true;
    setDot('conn-dot', 'green');
    const el = document.getElementById('loading-status');
    if (el) { el.textContent = 'Připojeno'; el.classList.add('connected'); }
    dismissLoading();
  });
  ws.addEventListener('close', (e) => {
    wsConnected = false;
    if (e && e.code === 4001) { window.location.href = '/login.html'; return; }
    setDot('conn-dot', 'red');
    setTimeout(connectWs, 3000);
  });
  ws.addEventListener('error', () => setDot('conn-dot', 'yellow'));
  ws.addEventListener('message', e => {
    try {
      const msg = JSON.parse(e.data);
      if (msg.type === 'status') {
        lastData = msg.data;
        render(msg.data);
        dismissLoading();
      }
    } catch {}
  });
}

// ── Render ────────────────────────────────────────────────────────────────────
function render(d) {
  renderCount++;

  // Balance
  if (d.balance_usd != null) setText('balance', `$${d.balance_usd.toFixed(2)}`);
  setText('matic-balance', d.matic_balance != null ? `${(+d.matic_balance).toFixed(4)}` : '—');

  // LIVE indicator
  const live = d.health.feed_ok && d.health.executor_ok;
  const lb   = document.getElementById('live-dot');
  if (lb) {
    lb.textContent = live ? '🟢 LIVE' : '🔴 DOWN';
    lb.classList.toggle('pulse', live);
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

  syncNoBetToggle(d.config, d.processes);

  // Compute today's P&L from bets
  const todayStr  = new Date().toISOString().slice(0,10);
  const allBets   = d.recent_bets || [];
  const todayBets = allBets.filter(b => b.ts && b.ts.slice(0,10) === todayStr);
  const gainToday = todayBets.filter(b => b.event==='WON').reduce((s,b) => s+(b.payout_usd||0)-(b.amount_usd||b.stake||0), 0);
  const lossToday = todayBets.filter(b => b.event==='LOST').reduce((s,b) => s+(b.amount_usd||b.stake||0), 0);
  const pnlToday  = gainToday - lossToday;

  const gainEl = document.getElementById('gain-today');
  if (gainEl) { gainEl.textContent = `+$${gainToday.toFixed(2)}`; gainEl.className = 'stat-value pos'; }
  const pnlEl = document.getElementById('pnl-today');
  if (pnlEl) { pnlEl.textContent = (pnlToday>=0?'+':'') + `$${pnlToday.toFixed(2)}`; pnlEl.className = 'stat-value '+(pnlToday>=0?'pos':'neg'); }

  // Win rate + W/L (last 200 settled)
  const settled = allBets.filter(b => b.event==='WON' || b.event==='LOST');
  const wins    = settled.filter(b => b.event==='WON').length;
  const losses  = settled.length - wins;
  const wr      = settled.length > 0 ? (wins/settled.length*100).toFixed(1) : null;
  setText('win-rate',  wr ? `${wr}%` : '—');
  setText('wl-counts', `W:${wins} L:${losses}`);
  setText('bets-today', d.bets_today != null ? String(d.bets_today) : String(todayBets.length));

  // Pending
  const pending    = d.pending || [];
  const inflight   = d.inflight_pending || [];
  const totalStake = pending.reduce((s,b) => s+(b.amount||b.stake||b.amount_usd||0), 0);
  const mismatch   = d.pending_truth_mismatch || 0;
  const recentBetSummary = buildRecentBetSummary(allBets);
  const opsAuditSummary = buildOpsAuditSummary(d.runtime_audit || {});
  setText('pending-dots',    pending.length > 0 ? '●'.repeat(Math.min(pending.length,5)) : '○');
  setText('pending-summary', `${pending.length} chain • $${totalStake.toFixed(2)}${inflight.length ? ` • +${inflight.length} inflight` : ''}${mismatch ? ` • ${mismatch} stale` : ''}`);
  setText('recent-bet-summary', recentBetSummary || 'Bez čerstvého bet eventu');
  setText('ops-audit-summary', opsAuditSummary);

  // Loss limit bar — show REAL effective limit
  const configLimit = d.config?.loss_limit ?? 30;
  const sodBankroll = d.config?.sod_bankroll ?? d.balance_usd ?? 27;
  // Micro tier: daily_loss_frac = 60% of SOD bankroll
  const dlFrac = sodBankroll < 150 ? 0.60 : sodBankroll < 500 ? 0.20 : sodBankroll < 1500 ? 0.15 : 0.10;
  const tierCap = sodBankroll * dlFrac;
  const effectiveLimit = d.config?.effective_limit ?? Math.min(configLimit, tierCap);
  const lossPct   = Math.min(effectiveLimit > 0 ? (lossToday/effectiveLimit)*100 : 0, 100);
  setText('loss-val', `$${lossToday.toFixed(2)} / $${effectiveLimit.toFixed(2)}`);
  const detailEl = document.getElementById('loss-detail');
  if (detailEl) detailEl.textContent = `min($${configLimit.toFixed(0)} hard, $${tierCap.toFixed(1)} = ${(dlFrac*100).toFixed(0)}% × $${sodBankroll.toFixed(0)} SOD)`;
  const bar = document.getElementById('loss-bar');
  if (bar) { bar.style.width=`${lossPct}%`; bar.className='loss-bar-fill'+(lossPct>80?' danger':lossPct>50?' warn':''); }

  // Config
  if (d.config) applyConfig(d.config);

  // Lists
  renderBetsList(allBets, activeBetFilter);
  renderStats(allBets, activeStatsPeriod);
  renderSparkline(d.pnl_7d || []);
  renderSportStats(allBets);

  // Last updated
  const luEl = document.getElementById('last-updated');
  if (luEl && d.ts) {
    const t = new Date(d.ts);
    luEl.textContent = `Aktualizováno ${t.toLocaleTimeString('cs', {hour:'2-digit',minute:'2-digit',second:'2-digit'})}`;
  }
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

// ── Pending modal ─────────────────────────────────────────────────────────────
function openPendingModal() {
  if (!lastData) return;
  const bets = lastData.pending || [];
  const inflight = lastData.inflight_pending || [];
  const el   = document.getElementById('pending-modal-list');
  if (!el) return;
  if (bets.length === 0 && inflight.length === 0) {
    el.innerHTML = '<div style="padding:20px;text-align:center;color:var(--muted)">Žádné pending bety</div>';
  } else {
    const renderItems = (items, title, subtitle) => `
      <div class="pmi-section-title" style="padding:8px 4px 10px;color:var(--muted);font-size:12px;text-transform:uppercase;letter-spacing:.08em">${escHtml(title)}${subtitle ? ` <span style="font-size:11px;text-transform:none;letter-spacing:0;color:var(--muted)">${escHtml(subtitle)}</span>` : ''}</div>
      ${items.map(b => `
      <div class="pmi-item">
        <div class="pmi-top">
          <span class="pmi-emoji">${sportEmoji(b.sport||detectSport(b.matchKey||b.match_key||''))}</span>
          <span class="pmi-match">${escHtml(b.team || shortMatchKey(b.matchKey||b.match_key||'') || '?')}</span>
          <span class="pmi-odds">@ ${+(b.odds||0).toFixed(2)}</span>
        </div>
        <div class="pmi-bottom">
          <span>$${+(b.stake||b.amount_usd||0).toFixed(2)}</span>
          <span class="pmi-time">⏱ ${timeAgo(b.placedAt||b.ts)}</span>
        </div>
      </div>
      `).join('<hr class="pmi-sep">')}
    `;
    const sections = [];
    if (bets.length) sections.push(renderItems(bets, 'On-chain pending', `${bets.length} NFT`));
    if (inflight.length) sections.push(renderItems(inflight, 'Local inflight', `${inflight.length} čeká na reconcile`));
    el.innerHTML = sections.join('<div style="height:14px"></div>');
  }
  document.getElementById('pending-modal').classList.remove('hidden');
}
function closePendingModal() { document.getElementById('pending-modal').classList.add('hidden'); }

// ── Bet detail modal ──────────────────────────────────────────────────────────
function showBetDetail(b) {
  document.getElementById('modal-title').textContent = shortMatchKey(b.match_key||'?');
  const rows = [
    ['Match',  b.match_key||'—'],
    ['Result', b.event||'—'],
    ['Odds',   b.odds||'—'],
    ['Stake',  `$${+(b.stake||b.amount_usd||0).toFixed(4)}`],
    ['Payout', b.payout_usd ? `$${b.payout_usd.toFixed(2)}` : '—'],
    ['Path',   b.path||'—'],
    ['Edge',   b.edge_pct ? `${b.edge_pct.toFixed(1)}%` : '—'],
    ['Time',   b.ts ? new Date(b.ts).toLocaleString('cs') : '—'],
  ];
  document.getElementById('modal-body').innerHTML = rows.map(([k,v]) =>
    `<div class="modal-row"><span class="modal-row-key">${k}</span><span>${escHtml(String(v))}</span></div>`
  ).join('');
  document.getElementById('modal').classList.remove('hidden');
}
function closeModal() { document.getElementById('modal').classList.add('hidden'); }

// ── Bets filter + list ────────────────────────────────────────────────────────
function setBetFilter(filter, btn) {
  activeBetFilter = filter;
  document.querySelectorAll('#tab-bets .filter-btn').forEach(b => b.classList.remove('active'));
  btn.classList.add('active');
  if (lastData) renderBetsList(lastData.recent_bets || [], filter);
}

function renderBetsList(bets, filter) {
  filter = filter || activeBetFilter;
  const el = document.getElementById('bets-list');
  if (!el) return;
  const filtered = filter === 'won'    ? bets.filter(b => b.event==='WON')
                 : filter === 'lost'   ? bets.filter(b => b.event==='LOST')
                 : filter === 'placed' ? bets.filter(b => b.event==='PLACED')
                 : bets;
  if (filtered.length === 0) { el.innerHTML = '<div style="padding:14px;color:var(--muted)">Žádné bety</div>'; return; }
  el.innerHTML = filtered.slice(0,100).map(b => {
    const profit = b.profit_usd != null ? b.profit_usd
      : b.event==='WON'  ? (b.payout_usd||0)-(b.amount_usd||b.stake||0)
      : b.event==='LOST' ? -(b.amount_usd||b.stake||0) : null;
    const profitStr = profit != null
      ? `<span class="bet-profit ${profit>=0?'pos':'neg'}">${profit>=0?'+':''}$${profit.toFixed(2)}</span>` : '';
    const emoji = b.event==='WON'?'✅':b.event==='LOST'?'❌':b.event==='PLACED'?'⏳':'—';
    const time  = b.ts ? new Date(b.ts).toLocaleTimeString('cs',{hour:'2-digit',minute:'2-digit'}) : '';
    return `<div class="bet-row" onclick='showBetDetail(${JSON.stringify(b).replace(/'/g,"&#39;")})'>
      <span class="bet-emoji">${emoji}</span>
      <div class="bet-info">
        <div class="bet-match-name">${escHtml(shortMatchKey(b.match_key||'?'))}</div>
        <div class="bet-detail">${+(b.odds||0).toFixed(2)} · $${+(b.amount_usd||b.stake||0).toFixed(2)} · ${b.path||''} · ${time}</div>
      </div>
      ${profitStr}
    </div>`;
  }).join('');
}

// ── Stats tab ─────────────────────────────────────────────────────────────────
function setStatsPeriod(days, btn) {
  activeStatsPeriod = days;
  document.querySelectorAll('#tab-stats .filter-btn').forEach(b => b.classList.remove('active'));
  btn.classList.add('active');
  if (lastData) renderStats(lastData.recent_bets || [], days);
}

function renderStats(bets, days) {
  days = days || activeStatsPeriod;
  const el = document.getElementById('stats-content');
  if (!el) return;
  const label = document.getElementById('stats-period-label');
  if (label) label.textContent = `Stats — ${days===1?'Dnes':days===7?'7 dní':'30 dní'}`;
  const cutoff  = Date.now() - days*86400_000;
  const inPeriod = bets.filter(b => !b.ts || new Date(b.ts).getTime() > cutoff);
  const settled  = inPeriod.filter(b => b.event==='WON' || b.event==='LOST');
  const won      = settled.filter(b => b.event==='WON');
  const lost     = settled.filter(b => b.event==='LOST');
  const totalStake  = settled.reduce((s,b) => s+(b.amount_usd||b.stake||0), 0);
  const totalPayout = won.reduce((s,b) => s+(b.payout_usd||0), 0);
  const totalPnl    = totalPayout - totalStake;
  const avgOdds     = settled.length ? settled.reduce((s,b) => s+(b.odds||0),0)/settled.length : 0;
  const wr          = settled.length ? (won.length/settled.length*100) : 0;
  const roi         = totalStake > 0 ? totalPnl/totalStake*100 : 0;
  const grossWin    = won.reduce((s,b) => s+(b.payout_usd||0)-(b.amount_usd||b.stake||0), 0);
  const grossLoss   = lost.reduce((s,b) => s+(b.amount_usd||b.stake||0), 0);
  const pf          = grossLoss > 0 ? grossWin/grossLoss : null;
  const avgWin      = won.length  ? grossWin/won.length   : 0;
  const avgLoss     = lost.length ? grossLoss/lost.length : 0;
  const betsPnl     = settled.map(b => ({ key: shortMatchKey(b.match_key||'?'), pnl: b.event==='WON'?(b.payout_usd||0)-(b.amount_usd||b.stake||0):-(b.amount_usd||b.stake||0) }));
  const best  = betsPnl.length ? betsPnl.reduce((a,b) => b.pnl>a.pnl?b:a) : null;
  const worst = betsPnl.length ? betsPnl.reduce((a,b) => b.pnl<a.pnl?b:a) : null;

  el.innerHTML = `
    <div class="stats-grid">
      <div class="stats-item"><div class="stats-key">Výhry / Prohry</div><div class="stats-val">${won.length} / ${lost.length}</div></div>
      <div class="stats-item"><div class="stats-key">Win rate</div><div class="stats-val" style="${wr>=55?'color:var(--green)':wr>=45?'':'color:var(--red)'}">${wr.toFixed(1)}%</div></div>
      <div class="stats-item"><div class="stats-key">P&L</div><div class="stats-val" style="${totalPnl>=0?'color:var(--green)':'color:var(--red)'}">${totalPnl>=0?'+':''}$${totalPnl.toFixed(2)}</div></div>
      <div class="stats-item"><div class="stats-key">ROI</div><div class="stats-val" style="${roi>=0?'color:var(--green)':'color:var(--red)'}">${roi.toFixed(1)}%</div></div>
      <div class="stats-item"><div class="stats-key">Profit factor</div><div class="stats-val" style="${pf!=null&&pf>=1?'color:var(--green)':'color:var(--red)'}">${pf!=null?pf.toFixed(2):'—'}</div></div>
      <div class="stats-item"><div class="stats-key">Total betted</div><div class="stats-val">$${totalStake.toFixed(2)}</div></div>
      <div class="stats-item"><div class="stats-key">Avg odds</div><div class="stats-val">${avgOdds.toFixed(2)}</div></div>
      <div class="stats-item"><div class="stats-key">Avg win</div><div class="stats-val" style="color:var(--green)">+$${avgWin.toFixed(2)}</div></div>
      <div class="stats-item"><div class="stats-key">Avg loss</div><div class="stats-val" style="color:var(--red)">-$${avgLoss.toFixed(2)}</div></div>
      <div class="stats-item"><div class="stats-key">Break-even WR</div><div class="stats-val">${avgOdds>0?(1/avgOdds*100).toFixed(1):0}%</div></div>
    </div>
    ${best  ? `<div style="margin-top:10px;font-size:12px;color:var(--green)">🏆 Best: ${escHtml(best.key)} +$${best.pnl.toFixed(2)}</div>` : ''}
    ${worst ? `<div style="margin-top:4px;font-size:12px;color:var(--red)">💀 Worst: ${escHtml(worst.key)} $${worst.pnl.toFixed(2)}</div>` : ''}
    <div style="margin-top:8px;color:var(--muted);font-size:11px">Z ${settled.length} settled betů</div>
  `;
}

// ── Sparkline SVG ─────────────────────────────────────────────────────────────
function renderSparkline(data) {
  const svg = document.getElementById('sparkline');
  if (!svg || !data || data.length < 2) return;
  const vals = data.map(d => d.pnl);
  const min  = Math.min(...vals, 0);
  const max  = Math.max(...vals, 0);
  const rng  = (max - min) || 0.01;
  const W=300, H=60, P=10;
  const w=W-P*2, h=H-P*2;
  const xs = vals.map((_,i) => P + (i/(vals.length-1))*w);
  const ys = vals.map(v  => P + h - ((v-min)/rng)*h);
  const y0 = P + h - ((0-min)/rng)*h;
  let path = `M ${xs[0]} ${ys[0]}`;
  for (let i=1;i<xs.length;i++) path += ` L ${xs[i]} ${ys[i]}`;
  const fill = `${path} L ${xs[xs.length-1]} ${y0} L ${xs[0]} ${y0} Z`;
  const col  = vals[vals.length-1] >= 0 ? '#00c853' : '#f44336';
  svg.innerHTML = `
    <defs><linearGradient id="sg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0%" stop-color="${col}" stop-opacity="0.35"/>
      <stop offset="100%" stop-color="${col}" stop-opacity="0"/>
    </linearGradient></defs>
    <line x1="0" y1="${y0}" x2="300" y2="${y0}" stroke="#333" stroke-width="1"/>
    <path d="${fill}" fill="url(#sg)"/>
    <path d="${path}" fill="none" stroke="${col}" stroke-width="2" stroke-linejoin="round"/>
  `;
}

// ── Per sport stats ───────────────────────────────────────────────────────────
function renderSportStats(bets) {
  const el = document.getElementById('sport-stats');
  if (!el) return;
  const stats = {};
  for (const b of bets.filter(x => x.event==='WON'||x.event==='LOST')) {
    const sp = b.sport || detectSport(b.match_key||'') || 'other';
    if (!stats[sp]) stats[sp] = {w:0,l:0,pnl:0};
    if (b.event==='WON')  { stats[sp].w++; stats[sp].pnl += (b.payout_usd||0)-(b.amount_usd||b.stake||0); }
    if (b.event==='LOST') { stats[sp].l++; stats[sp].pnl -= (b.amount_usd||b.stake||0); }
  }
  const entries = Object.entries(stats).sort((a,b) => (b[1].w+b[1].l)-(a[1].w+a[1].l));
  if (!entries.length) { el.innerHTML='<div style="padding:14px;color:var(--muted)">Chybí sport data v ledgeru</div>'; return; }
  el.innerHTML = entries.map(([sp,s]) => `
    <div class="sport-stat-row">
      <span class="sport-stat-name">${sportEmoji(sp)} ${sp}</span>
      <span class="sport-stat-wl">W:${s.w} L:${s.l}</span>
      <span class="sport-stat-pnl ${s.pnl>=0?'pos':'neg'}">${s.pnl>=0?'+':''}$${s.pnl.toFixed(2)}</span>
    </div>
  `).join('');
}

// ── Controls ──────────────────────────────────────────────────────────────────
async function toggleAutobet() {
  const input = document.getElementById('autobet-toggle');
  if (!input) return;
  const noBetMode = input.checked;
  setText('autobet-label', noBetMode ? 'PŘEPÍNÁM NO BET…' : 'OBNOVUJU LIVE BET…');
  const result = await api('POST', '/api/config', { no_bet_mode: noBetMode });
  if (!result?.ok) {
    input.checked = !noBetMode;
    setText('autobet-label', 'CHYBA');
    return;
  }
  applyConfig(result.config || { no_bet_mode: noBetMode });
}

function applyConfig(cfg) {
  currentSportFocus = cfg.sport_focus || ['all'];
  const lil = document.getElementById('loss-limit-input');
  const msi = document.getElementById('max-stake-input');
  if (lil && document.activeElement !== lil) lil.value = cfg.loss_limit ?? 15.55;
  if (msi && document.activeElement !== msi) msi.value = cfg.max_stake  ?? 3.00;
  syncNoBetToggle(cfg, lastData?.processes || {});
  document.querySelectorAll('.sport-pill').forEach(btn =>
    btn.classList.toggle('active', currentSportFocus.includes(btn.dataset.sport))
  );
}

function syncNoBetToggle(cfg, processes) {
  const toggle = document.getElementById('autobet-toggle');
  if (!toggle) return;
  const noBetMode = !!(cfg && cfg.no_bet_mode);
  const botRunning = processes && processes['alert-bot'] === 'running';
  toggle.checked = noBetMode;
  if (!botRunning) {
    setText('autobet-label', noBetMode ? 'BOT OFFLINE · NO BET ON' : 'BOT OFFLINE');
    return;
  }
  setText('autobet-label', noBetMode ? 'NO BET ON' : 'LIVE BET ON');
}

function toggleSport(sport, btn) {
  if (sport === 'all') {
    currentSportFocus = ['all'];
  } else {
    currentSportFocus = currentSportFocus.filter(s => s !== 'all');
    if (currentSportFocus.includes(sport)) currentSportFocus = currentSportFocus.filter(s => s !== sport);
    else currentSportFocus.push(sport);
    if (!currentSportFocus.length) currentSportFocus = ['all'];
  }
  document.querySelectorAll('.sport-pill').forEach(b =>
    b.classList.toggle('active', currentSportFocus.includes(b.dataset.sport))
  );
  api('POST', '/api/config', { sport_focus: currentSportFocus }).catch(() => {});
}

async function saveLimits() {
  const ll = parseFloat(document.getElementById('loss-limit-input').value);
  const ms = parseFloat(document.getElementById('max-stake-input').value);
  if (isNaN(ll) || isNaN(ms) || ll <= 0 || ms <= 0) { showLimitsMsg('Neplatná hodnota','error'); return; }
  const r = await api('POST', '/api/config', { loss_limit: ll, max_stake: ms });
  r.ok ? showLimitsMsg('✅ Uloženo (restart alert-bot pro aplikaci)','ok') : showLimitsMsg('Chyba uložení','error');
}

function showLimitsMsg(msg, type) {
  const el = document.getElementById('limits-msg');
  if (!el) return;
  el.textContent = msg; el.className = 'limits-msg ' + type;
  setTimeout(() => el.classList.add('hidden'), 4000);
}

async function startProc(name) {
  if (!confirm(`Spustit ${name}?`)) return;
  const r = await api('POST', `/api/process/start/${name}`);
  alert(r.ok ? `${name} spuštěn (PID ${r.pid||'?'})` : `Chyba: ${r.error}`);
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
    const lines = r.lines || [];
    if (!lines.length) { el.textContent = '(prázdný log)'; return; }
    // Parse JSONL lines into human-readable format
    const formatted = lines.slice(-60).map(line => {
      try {
        const o = typeof line === 'string' ? JSON.parse(line) : line;
        const t = o.ts ? new Date(o.ts).toLocaleTimeString('cs',{hour:'2-digit',minute:'2-digit',second:'2-digit'}) : '';
        const ev = o.event || o.level || '?';
        if (o.event === 'PLACED')   return `${t} ✅ BET ${o.match_key||''} edge=${(o.edge_pct||0).toFixed(1)}% @${(o.odds||0).toFixed(2)} $${(o.amount_usd||o.stake||0).toFixed(2)}`;
        if (o.event === 'WON')      return `${t} 🏆 WON ${o.match_key||''} +$${((o.payout_usd||0)-(o.amount_usd||0)).toFixed(2)}`;
        if (o.event === 'LOST')     return `${t} ❌ LOST ${o.match_key||''} -$${(o.amount_usd||0).toFixed(2)}`;
        if (o.event === 'CANCELED') return `${t} ↩️ CANCEL ${o.match_key||''}`;
        if (o.event === 'BET_FAILED') return `${t} ⛔ FAIL ${o.match_key||''} ${o.error||o.reason_code||''}`;
        if (o.event === 'ON_CHAIN_ACCEPTED') return `${t} ⛓ CHAIN ${o.match_key||''} token=${o.token_id||'?'}`;
        if (o.event === 'EXECUTOR_CLAIM') return `${t} 💰 CLAIM $${(o.totalPayoutUsd||0).toFixed(2)} bal=$${o.newBalanceUsd||'?'}`;
        if (o.event === 'SAFETY_CLAIM') return `${t} 🔒 SAFE_CLAIM $${(o.payout_usd||0).toFixed(2)}`;
        if (o.event === 'LIMIT_OVERRIDE') return `${t} ⚡ LIMIT +$${o.delta||'?'} → $${o.new_limit||'?'}`;
        if (o.event === 'ESPORTS_PROMOTION_GATE_AUDIT') return `${t} ${o.allowed?'🟢':'🔴'} GATE ${o.allowed?'PASS':'BLOCK'} ${o.match_key||''} sport=${o.esports_family||'?'} conf=${o.confidence||'?'}`;
        if (o.event === 'DAILY_RESET') return `${t} 🔄 DAILY_RESET SOD=$${(o.sod_bankroll||0).toFixed(2)}`;
        if (o.event === 'LOSS_STREAK_PAUSE') return `${t} ⏸️ LOSS_STREAK_PAUSE ${o.reason||''}`;
        // Generic: just show ts + event + key info
        return `${t} [${ev}] ${o.match_key || o.msg || JSON.stringify(o).slice(0,80)}`;
      } catch { return typeof line === 'string' ? line.slice(0,120) : String(line); }
    });
    el.textContent = formatted.join('\n');
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
  if (!key || typeof key !== 'string') return '?';
  const parts = key.split('::');
  let match = parts[1] || key;
  match = match.replace(/_vs_/g,' vs ').replace(/_/g,' ');
  const market = parts[2] ? ` (${parts[2].replace('_winner','').replace(/_/g,' ')})` : '';
  return (match + market).slice(0,40);
}

function detectSport(key) {
  key = key.toLowerCase();
  if (/tennis|itf|atp|wta/.test(key))       return 'tennis';
  if (/cs2|csgo|counter/.test(key))          return 'cs2';
  if (/football|soccer|liga|bundesliga|premier/.test(key)) return 'football';
  if (/basketball|nba|euroleague/.test(key)) return 'basketball';
  if (/valorant/.test(key))                  return 'valorant';
  if (/dota/.test(key))                      return 'dota2';
  if (/lol|league/.test(key))                return 'lol';
  return null;
}

function sportEmoji(sp) {
  return {tennis:'🎾',cs2:'🎮',football:'⚽',basketball:'🏀',valorant:'🎮',dota2:'🎮',lol:'🎮',esports:'🎮'}[sp] || '🎯';
}

function timeAgo(ts) {
  if (!ts) return '';
  const m = Math.floor((Date.now()-new Date(ts).getTime())/60000);
  if (m < 1)  return 'právě';
  if (m < 60) return `${m} min ago`;
  const h = Math.floor(m/60), rem = m%60;
  return rem > 0 ? `${h}h ${rem} min ago` : `${h}h ago`;
}

function escHtml(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}

async function api(method, url, body) {
  const opts = { method, credentials: 'same-origin', headers: {} };
  if (body) { opts.headers['Content-Type'] = 'application/json'; opts.body = JSON.stringify(body); }
  const r = await fetch(url, opts);
  return r.json().catch(() => ({}));
}

// ── Limit modal ───────────────────────────────────────────────────────────────
function openLimitModal() {
  const d = lastData;
  const el = document.getElementById('limit-info');
  if (!el || !d) return;
  const configLimit = d.config?.loss_limit ?? 30;
  const sodBankroll = d.config?.sod_bankroll ?? d.balance_usd ?? 27;
  const dlFrac = sodBankroll < 150 ? 0.60 : sodBankroll < 500 ? 0.20 : sodBankroll < 1500 ? 0.15 : 0.10;
  const tierCap = sodBankroll * dlFrac;
  const effectiveLimit = d.config?.effective_limit ?? Math.min(configLimit, tierCap);
  const todayStr  = new Date().toISOString().slice(0,10);
  const todayBets = (d.recent_bets||[]).filter(b => b.ts && b.ts.slice(0,10) === todayStr);
  const lossToday = todayBets.filter(b => b.event==='LOST').reduce((s,b) => s+(b.amount_usd||b.stake||0), 0);
  const gainToday = todayBets.filter(b => b.event==='WON').reduce((s,b) => s+(b.payout_usd||0)-(b.amount_usd||b.stake||0), 0);
  const netLoss = Math.max(lossToday - gainToday, 0);
  const room = Math.max(effectiveLimit - netLoss, 0);
  el.innerHTML = `
    <b>Aktuální limit:</b> $${effectiveLimit.toFixed(2)}<br>
    <b>Hard limit:</b> $${configLimit.toFixed(0)} · <b>Tier cap:</b> $${tierCap.toFixed(1)} (${(dlFrac*100).toFixed(0)}% × $${sodBankroll.toFixed(0)})<br>
    <b>Net loss dnes:</b> $${netLoss.toFixed(2)} · <b>Zbývá:</b> $${room.toFixed(2)}<br>
    <span style="color:var(--muted);font-size:11px">⚡ Navýšení se projeví PŘÍMO v alert-botu.</span>
  `;
  document.getElementById('limit-result').className = 'limit-result hidden';
  document.getElementById('limit-modal').classList.remove('hidden');
}
function closeLimitModal() { document.getElementById('limit-modal').classList.add('hidden'); }

async function raiseLimit(delta) {
  const res = document.getElementById('limit-result');
  res.textContent = 'Odesílám...'; res.className = 'limit-result ok';
  try {
    const r = await api('POST', '/api/limit', { delta });
    if (r.ok) {
      res.textContent = `✅ Limit navýšen na $${r.new_limit.toFixed(0)} (room: $${r.room.toFixed(2)})`;
      res.className = 'limit-result ok';
      // Update config locally
      if (lastData?.config) lastData.config.loss_limit = r.new_limit;
    } else {
      res.textContent = `❌ ${r.error || 'Chyba'}`;
      res.className = 'limit-result error';
    }
  } catch (e) {
    res.textContent = '❌ Síťová chyba'; res.className = 'limit-result error';
  }
}

// ── Init ──────────────────────────────────────────────────────────────────────
connectWs();
fetchStrategy();
// Visibility API — reconnect on tab focus for instant sync
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible' && !wsConnected) {
    connectWs();
  }
});

// ── Strategy panel (fetched from /api/strategy) ───────────────────────────
async function fetchStrategy() {
  try {
    const s = await api('GET', '/api/strategy');
    if (!s || !s.score_edge) return;
    renderStrategy(s);
  } catch {}
}

function renderStrategy(s) {
  const el = document.getElementById('strategy-info');
  if (!el) return;
  const se = s.score_edge;
  const an = s.anomaly;
  const g  = s.guards;
  const gs = s.promotion_gate_stats || {};

  // Score-edge stakes
  const stakeRows = Object.entries(se.stakes).map(([sp, v]) =>
    `<div class="strat-row"><span>${sportEmoji(sp)} ${sp}</span><span class="strat-val">$${v.base.toFixed(2)}</span></div>`
  ).join('');

  // CS2 max odds tiers
  const tierRows = Object.values(se.cs2_max_odds_tiers).map(t =>
    `<div class="strat-row"><span>${escHtml(t.label)}</span><span class="strat-val">${t.max_odds.toFixed(2)}</span></div>`
  ).join('');

  // Anomaly per sport
  const anomalyRows = Object.entries(an.sports).map(([sp, v]) =>
    `<div class="strat-row"><span>${sportEmoji(sp)} ${sp}</span><span class="strat-val ${v.enabled?'on':'off'}">${v.enabled?'✅ ON':'⛔ OFF'}</span></div>`
  ).join('');

  el.innerHTML = `
    <div class="strat-section">
      <div class="strat-title">Path A: Score-edge</div>
      <div class="strat-row"><span>Min edge (default)</span><span class="strat-val">${se.min_edge_default}%</span></div>
      <div class="strat-row"><span>CS2 map_winner min edge</span><span class="strat-val accent">${se.cs2_map_winner_min_edge}% 🎯</span></div>
      <div class="strat-row"><span>Odds corridor</span><span class="strat-val">${se.min_odds} – ${se.max_odds_default}</span></div>
      ${stakeRows}
    </div>
    <div class="strat-section">
      <div class="strat-title">CS2 Max Odds Tiers</div>
      ${tierRows}
    </div>
    <div class="strat-section">
      <div class="strat-title">Path B: Odds anomaly</div>
      <div class="strat-row"><span>Max odds</span><span class="strat-val">${an.max_odds}</span></div>
      <div class="strat-row"><span>Min disc (global)</span><span class="strat-val">${an.min_disc_global}%</span></div>
      <div class="strat-row"><span>Tennis min disc</span><span class="strat-val">${an.tennis_min_disc}%</span></div>
      <div class="strat-row"><span>Stake formula</span><span class="strat-val" style="font-size:10px">${escHtml(an.stake_formula)}</span></div>
      ${anomalyRows}
    </div>
    <div class="strat-section">
      <div class="strat-title">🛡️ Guards</div>
      <div class="strat-row"><span>Daily loss limit</span><span class="strat-val">$${g.daily_loss_limit}</span></div>
      <div class="strat-row"><span>Min bankroll</span><span class="strat-val">$${g.min_bankroll}</span></div>
      <div class="strat-row"><span>Total execution guards</span><span class="strat-val">${g.total_guards}</span></div>
      <div class="strat-row"><span>Block generic esports</span><span class="strat-val ${g.block_generic_esports?'on':'off'}">${g.block_generic_esports?'✅ YES':'⛔ NO'}</span></div>
      <div class="strat-row"><span>Promotion gate</span><span class="strat-val" style="font-size:10px">${escHtml(g.promotion_gate)}</span></div>
      ${gs.passed != null ? `<div class="strat-row"><span>Gate stats</span><span class="strat-val">✅ ${gs.passed} passed · ⛔ ${gs.blocked} blocked</span></div>` : ''}
    </div>
    <div style="padding:6px 0 0;font-size:11px;color:var(--muted)">Tuning ${s.updated} · data-driven</div>
  `;
}

function buildRecentBetSummary(bets) {
  const recent = (bets || [])
    .filter(b => ['PLACED', 'WON', 'LOST', 'CANCELED'].includes(b.event) && b.ts)
    .sort((a, b) => new Date(b.ts).getTime() - new Date(a.ts).getTime());
  if (!recent.length) return '';

  const bet = recent[0];
  const age = timeAgo(bet.ts);
  const name = shortMatchKey(bet.match_key || '?');
  const stake = +(bet.amount_usd || bet.stake || 0);
  if (bet.event === 'LOST') return `Poslední settle: ❌ ${name} -$${stake.toFixed(2)} • ${age}`;
  if (bet.event === 'WON') {
    const profit = +((bet.payout_usd || 0) - stake).toFixed(2);
    return `Poslední settle: ✅ ${name} +$${profit.toFixed(2)} • ${age}`;
  }
  if (bet.event === 'PLACED') return `Poslední bet: ⏳ ${name} @${+(bet.odds || 0).toFixed(2)} • $${stake.toFixed(2)} • ${age}`;
  return `Poslední event: ↩️ ${name} • ${age}`;
}

function buildOpsAuditSummary(audit) {
  const hours = audit.window_hours || 24;
  const driftCount = audit.drift_alerts || 0;
  const maxAbs = +(audit.drift_max_abs || 0);
  const maxPct = +(audit.drift_max_pct || 0);
  const footballBlocked = audit.football_blocked || 0;
  const footballGuardBlocked = audit.football_guard_blocked || 0;
  const footballOddsBlocked = audit.football_odds_blocked || 0;

  const driftPart = driftCount > 0
    ? `Drift ${hours}h: ${driftCount}x, max ${maxAbs.toFixed(2)} / ${maxPct.toFixed(1)}%`
    : `Drift ${hours}h: 0`;
  const footballPart = footballBlocked > 0
    ? `Football containment: ${footballBlocked} bloků, guard ${footballGuardBlocked}, odds ${footballOddsBlocked}`
    : 'Football containment: 0 bloků';
  return `${driftPart} • ${footballPart}`;
}