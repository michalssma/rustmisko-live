'use strict';
// RustMiskoLive Dashboard Server — port 7777
// Auth: 6-digit PIN → bcrypt → JWT httpOnly cookie (24h)
// Data: feeds from feed-hub :8081, executor :3030, data/ files

const express  = require('express');
const http     = require('http');
const WebSocket = require('ws');
const bcrypt   = require('bcryptjs');
const jwt      = require('jsonwebtoken');
const fs       = require('fs');
const path     = require('path');
const { execSync, spawn } = require('child_process');

// ── Config ──────────────────────────────────────────────────────────────────
const PORT        = parseInt(process.env.DASHBOARD_PORT || '7777', 10);
const ROOT        = path.join(__dirname, '..');
const DATA        = path.join(ROOT, 'data');
const EXECUTOR    = path.join(ROOT, 'executor');
const SECRET_FILE = path.join(DATA, 'dashboard.secret');
const CONFIG_FILE  = path.join(DATA, 'dashboard_config.json');
const LOGS_DIR    = path.join(ROOT, 'logs');

// ── Current system strategy (update when alert_bot tuning changes) ──────────
const STRATEGY = {
  updated: '2026-03-12',
  score_edge: {
    min_edge_default: 38,
    cs2_map_winner_min_edge: 28,
    min_odds: 1.70,
    max_odds_default: 3.00,
    cs2_max_odds_tiers: {
      ultra: { label: 'ULTRA (90%+ prob, ≥16 rnd)', max_odds: 5.00 },
      high:  { label: 'HIGH (80%+ prob, ≥13 rnd)',  max_odds: 3.00 },
      medium:{ label: 'MEDIUM (70%+ prob)',          max_odds: 2.00 },
      low:   { label: 'LOW (<70%)',                  max_odds: 1.60 },
    },
    stakes: {
      cs2:        { base: 3.00, note: 'Score-edge path (includes promoted esports::→CS2)' },
      football:   { base: 3.00, note: 'Score-edge path' },
      tennis:     { base: 0.50, note: 'Reduced — volatile' },
      basketball: { base: 0.50, note: 'Reduced — volatile' },
    },
  },
  anomaly: {
    max_odds: 1.70,
    min_disc_global: 28,
    tennis_min_disc: 24,
    stake_formula: '$0.50 × (1.25/odds)^1.5 (max $1.00)',
    tennis_disc_tiers: {
      standard: { range: '24-35%', multiplier: '1.0×' },
      strong:   { range: '35-45%', multiplier: '1.5×' },
      extreme:  { range: '45%+',   multiplier: '2.0×' },
    },
    sports: {
      tennis:     { enabled: true,  note: 'set diff ≥1, underdog 40%+, disc-tiered staking' },
      basketball: { enabled: true,  note: 'score confirmation required' },
      football:   { enabled: false, note: 'WR=40%, net loss' },
      esports:    { enabled: false, note: 'WR=52%, net loss' },
    },
  },
  guards: {
    daily_loss_limit: 30,
    min_bankroll: 10,
    block_generic_esports: true,
    promotion_gate: 'A1: Kill ALL generic esports:: | A2: Promote to CS2 model via Azuro-derived sport or team markers (medium OK)',
    azuro_derived_sport: 'Fuzzy-matched Azuro odds reveal actual sport from match_key prefix',
    cs2_team_markers: '45 teams (Tier 1-3) as fallback when Azuro prefix unavailable',
    total_guards: 29,
  },
};

const FEED_HEALTH = 'http://127.0.0.1:8081/health';
const FEED_STATE  = 'http://127.0.0.1:8081/state';
const EXEC_HEALTH = 'http://127.0.0.1:3030/health';
const EXEC_ACTIVE = 'http://127.0.0.1:3030/active-bets';
const EXEC_MY_BETS = 'http://127.0.0.1:3030/my-bets';

// ── Load secret ──────────────────────────────────────────────────────────────
if (!fs.existsSync(SECRET_FILE)) {
  console.error('[dashboard] No PIN configured. Run: node dashboard/setup.js');
  process.exit(1);
}
const SECRET = JSON.parse(fs.readFileSync(SECRET_FILE, 'utf8'));

// ── Express + WS ─────────────────────────────────────────────────────────────
const app    = express();
const server = http.createServer(app);
const wss    = new WebSocket.Server({ server });

app.use(express.json());
app.use(express.urlencoded({ extended: false }));
// Do not auto-serve index.html from static; root route is auth-protected below.
app.use(express.static(path.join(__dirname, 'public'), { index: false }));

// Mini cookie parser (no extra deps)
app.use((req, _res, next) => {
  req.cookies = {};
  const h = req.headers.cookie;
  if (h) h.split(';').forEach(c => {
    const [k, ...v] = c.trim().split('=');
    req.cookies[k.trim()] = v.join('=');
  });
  next();
});

// ── Rate limiter (login brute force protection) ──────────────────────────────
const loginAttempts = new Map(); // ip → { count, resetAt }
function checkRateLimit(ip) {
  const now = Date.now();
  const rec = loginAttempts.get(ip) || { count: 0, resetAt: now + 15 * 60_000 };
  if (now > rec.resetAt) { rec.count = 0; rec.resetAt = now + 15 * 60_000; }
  if (rec.count >= 5) return false;
  rec.count++;
  loginAttempts.set(ip, rec);
  return true;
}

// ── Auth middleware ───────────────────────────────────────────────────────────
function auth(req, res, next) {
  const token = req.cookies.token;
  if (!token) return res.redirect('/login.html');
  try { jwt.verify(token, SECRET.jwt_secret); next(); }
  catch { res.clearCookie('token'); res.redirect('/login.html'); }
}
function authApi(req, res, next) {
  const token = req.cookies.token;
  if (!token) return res.status(401).json({ error: 'Unauthorized' });
  try { jwt.verify(token, SECRET.jwt_secret); next(); }
  catch { res.status(401).json({ error: 'Session expired' }); }
}

// ── HTTP fetch helper ─────────────────────────────────────────────────────────
function fetchJson(url, timeoutMs = 2000, opts = {}) {
  return new Promise(resolve => {
    const mod = url.startsWith('https') ? require('https') : require('http');
    try {
      const reqOpts = { timeout: timeoutMs };
      if (opts.method === 'POST') {
        const urlObj = new URL(url);
        reqOpts.hostname = urlObj.hostname;
        reqOpts.path = urlObj.pathname;
        reqOpts.method = 'POST';
        reqOpts.headers = opts.headers || {};
        const req = mod.request(reqOpts, res => {
          let buf = '';
          res.on('data', d => buf += d);
          res.on('end', () => { try { resolve(JSON.parse(buf)); } catch { resolve(null); } });
        });
        req.on('error', () => resolve(null));
        req.on('timeout', () => { req.destroy(); resolve(null); });
        if (opts.body) req.write(opts.body);
        req.end();
      } else {
        const req = mod.get(url, reqOpts, res => {
          let buf = '';
          res.on('data', d => buf += d);
          res.on('end', () => { try { resolve(JSON.parse(buf)); } catch { resolve(null); } });
        });
        req.on('error', () => resolve(null));
        req.on('timeout', () => { req.destroy(); resolve(null); });
      }
    } catch { resolve(null); }
  });
}

// ── Data helpers ──────────────────────────────────────────────────────────────
function readJson(file, fallback = null) {
  try { return JSON.parse(fs.readFileSync(file, 'utf8')); }
  catch { return fallback; }
}

function getRecentBets(n = 50) {
  const file = path.join(DATA, 'ledger.jsonl');
  if (!fs.existsSync(file)) return [];
  try {
    const lines = fs.readFileSync(file, 'utf8').split('\n').filter(Boolean);
    const out = [];
    for (let i = lines.length - 1; i >= 0 && out.length < n; i--) {
      try {
        const o = JSON.parse(lines[i]);
        // Only include bet events that have match_key (skip claim/system events)
        if (['WON','LOST','CANCELED','PLACED'].includes(o.event) && o.match_key) out.push(o);
      } catch {}
    }
    return out;
  } catch { return []; }
}

function getAlertBotAge() {
  // Proxy for alert-bot health: age of today's log file modification
  const today = new Date().toISOString().slice(0, 10);
  const logFile = path.join(LOGS_DIR, `${today}.jsonl`);
  if (!fs.existsSync(logFile)) return -1;
  try {
    const stat = fs.statSync(logFile);
    return Date.now() - stat.mtimeMs;
  } catch { return -1; }
}

function getProcessStatus(name) {
  try {
    const out = execSync(`tasklist /FI "IMAGENAME eq ${name}" /NH /FO CSV`, { timeout: 3000 }).toString();
    return out.includes(name) ? 'running' : 'stopped';
  } catch { return 'unknown'; }
}

function getConfig() {
  const defaults = { autobet_enabled: true, sport_focus: ['all'], loss_limit: 30, max_stake: 3.00 };
  let config;
  try { config = { ...defaults, ...JSON.parse(fs.readFileSync(CONFIG_FILE, 'utf8')) }; }
  catch { config = defaults; }
  // Enrich with SOD bankroll + effective limit from daily_pnl.json
  const dailyPnl = readJson(path.join(DATA, 'daily_pnl.json'), {});
  config.sod_bankroll = dailyPnl?.sod_bankroll ?? null;
  if (config.sod_bankroll != null) {
    const br = config.sod_bankroll;
    const dlFrac = br < 150 ? 0.60 : br < 500 ? 0.20 : br < 1500 ? 0.15 : 0.10;
    config.effective_limit = Math.min(config.loss_limit, br * dlFrac);
  }
  return config;
}

function getPnl7d() {
  const file = path.join(DATA, 'ledger.jsonl');
  if (!fs.existsSync(file)) return [];
  const result = {};
  const cutoff = Date.now() - 8 * 86400_000;
  try {
    const lines = fs.readFileSync(file, 'utf8').split('\n').filter(Boolean);
    for (const line of lines) {
      try {
        const o = JSON.parse(line);
        if (!o.ts) continue;
        const t = new Date(o.ts).getTime();
        if (t < cutoff) continue;
        const day = o.ts.slice(0,10);
        if (!result[day]) result[day] = 0;
        if (o.event==='WON')  result[day] += (o.payout_usd||0) - (o.amount_usd||o.stake||0);
        if (o.event==='LOST') result[day] -= (o.amount_usd||o.stake||0);
      } catch {}
    }
  } catch {}
  const out = [];
  for (let i=6; i>=0; i--) {
    const d = new Date(Date.now()-i*86400_000).toISOString().slice(0,10);
    out.push({ date: d, pnl: +(result[d]||0).toFixed(2) });
  }
  return out;
}

// ── Status snapshot ───────────────────────────────────────────────────────────
let cachedMaticBalance = null;
let maticCacheTs = 0;
const MATIC_CACHE_TTL = 30_000; // cache MATIC balance for 30s (RPC is slow)
let cachedMyBets = null;
let myBetsCacheTs = 0;
const MY_BETS_CACHE_TTL = 30_000;

async function getStatus() {
  const [feedHealth, execHealth, execActive] = await Promise.all([
    fetchJson(FEED_HEALTH),
    fetchJson(EXEC_HEALTH),
    fetchJson(EXEC_ACTIVE),
  ]);

  const localActiveBets = Array.isArray(execActive?.bets)
    ? execActive.bets
    : readJson(path.join(DATA, 'active_bets.json'), []);
  const dailyPnl    = readJson(path.join(DATA, 'daily_pnl.json'), {});
  const recentBets  = getRecentBets(500);
  const alertBotAge = getAlertBotAge();
  const nowStatus = Date.now();

  if (execHealth && (nowStatus - myBetsCacheTs > MY_BETS_CACHE_TTL || !cachedMyBets)) {
    cachedMyBets = await fetchJson(EXEC_MY_BETS, 15000);
    myBetsCacheTs = nowStatus;
  }

  const onchainBets = Array.isArray(cachedMyBets?.bets) ? cachedMyBets.bets : [];
  const pendingTruth = onchainBets.filter(b => b.status === 'pending' || b.status === 'claimable');
  const inflightPending = Array.isArray(localActiveBets)
    ? localActiveBets.filter(b => !b.tokenId && !b.token_id)
    : [];
  const truthMismatch = Math.max((Array.isArray(localActiveBets) ? localActiveBets.length : 0) - pendingTruth.length, 0);

  const processes = {
    'feed-hub':  feedHealth  ? 'running' : getProcessStatus('feed-hub.exe'),
    'executor':  execHealth  ? 'running' : getProcessStatus('node.exe'),
    'alert-bot': alertBotAge >= 0 && alertBotAge < 120_000 ? 'running'
                 : getProcessStatus('alert-bot.exe'),
  };

  const balanceRaw   = execHealth?.balance ?? execHealth?.balanceUsd ?? null;
  const balance      = balanceRaw != null ? parseFloat(balanceRaw) : null;
  // MATIC: fetch native balance via RPC (cached to avoid slowness)
  let maticBalance = cachedMaticBalance;
  const now = Date.now();
  if (execHealth?.wallet && (now - maticCacheTs > MATIC_CACHE_TTL)) {
    try {
      const rpcRes = await fetchJson('https://polygon-rpc.com', 5000, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ jsonrpc: '2.0', method: 'eth_getBalance', params: [execHealth.wallet, 'latest'], id: 1 })
      });
      if (rpcRes?.result) {
        const weiBalance = BigInt(rpcRes.result);
        maticBalance = parseFloat((Number(weiBalance) / 1e18).toFixed(4));
        cachedMaticBalance = maticBalance;
        maticCacheTs = now;
      }
    } catch (e) {
      console.error('[dashboard] MATIC RPC error:', e.message);
    }
  }

  return {
    ts: new Date().toISOString(),
    balance_usd:   balance,
    matic_balance: maticBalance,
    health: {
      gql_age_ms:       feedHealth?.gql_age_ms  ?? -1,
      ws_age_ms:        feedHealth?.ws_age_ms   ?? -1,
      feed_ok:          feedHealth?.ok === true,
      executor_ok:      execHealth != null,
      alert_bot_age_ms: alertBotAge,
    },
    processes,
    pending:       pendingTruth,
    pending_count: pendingTruth.length,
    inflight_pending: inflightPending,
    inflight_count: inflightPending.length,
    local_pending_count: Array.isArray(localActiveBets) ? localActiveBets.length : 0,
    pending_truth_mismatch: truthMismatch,
    claimable_count: cachedMyBets?.claimable ?? 0,
    claimable_usd: cachedMyBets?.claimableUsd ?? 0,
    pnl_today:     dailyPnl?.pnl_today  ?? (dailyPnl?.returned != null ? +(dailyPnl.returned - dailyPnl.wagered).toFixed(4) : 0),
    wagered_today: dailyPnl?.wagered ?? 0,
    returned_today:dailyPnl?.returned ?? 0,
    bets_today:    dailyPnl?.bets_today ?? dailyPnl?.total_bets ?? recentBets.filter(b => b.ts && b.ts.slice(0,10) === new Date().toISOString().slice(0,10)).length,
    recent_bets:   recentBets,
    pnl_7d:        getPnl7d(),
    config:        getConfig(),
  };
}

// ── WebSocket push (every 2s) ─────────────────────────────────────────────────
let lastStatus = null;
let wsPushCount = 0;
setInterval(async () => {
  if (wss.clients.size === 0) return;
  try {
    lastStatus = await getStatus();
    const msg = JSON.stringify({ type: 'status', data: lastStatus });
    wss.clients.forEach(c => { if (c.readyState === WebSocket.OPEN) c.send(msg); });
    wsPushCount++;
    if (wsPushCount % 5 === 1) console.log(`[dashboard] WS push #${wsPushCount} → ${wss.clients.size} clients`);
  } catch (e) { console.error('[dashboard] WS push error:', e.message); }
}, 2000);

wss.on('connection', async (ws, req) => {
  // Auth check on WS upgrade via cookie
  const cookieHeader = req.headers.cookie || '';
  const tokenMatch = cookieHeader.match(/(?:^|;\s*)token=([^;]+)/);
  if (!tokenMatch) { ws.close(4001, 'Unauthorized'); return; }
  try { jwt.verify(tokenMatch[1], SECRET.jwt_secret); }
  catch { ws.close(4001, 'Session expired'); return; }

  // Send current status immediately
  try {
    const s = lastStatus || await getStatus();
    ws.send(JSON.stringify({ type: 'status', data: s }));
  } catch {}
});

// ── Routes ────────────────────────────────────────────────────────────────────
// Login page is served from public/login.html
// Root → dashboard (auth required)
app.get('/', auth, (_req, res) => res.sendFile(path.join(__dirname, 'public/index.html')));

// Auth
app.post('/auth', (req, res) => {
  const ip = req.ip || req.socket.remoteAddress || 'unknown';
  const { pin } = req.body;

  if (!pin || typeof pin !== 'string' || !/^\d{6}$/.test(pin)) {
    return res.status(400).json({ error: 'Invalid PIN format' });
  }
  if (!checkRateLimit(ip)) {
    return res.status(429).json({ error: 'Too many attempts. Wait 15 minutes.' });
  }

  bcrypt.compare(pin, SECRET.hash, (err, ok) => {
    if (err || !ok) return res.status(401).json({ error: 'Wrong PIN' });
    const token = jwt.sign({ user: 'admin' }, SECRET.jwt_secret, { expiresIn: '24h' });
    res.cookie('token', token, { httpOnly: true, sameSite: 'strict', maxAge: 86400_000 });
    res.json({ ok: true });
  });
});

app.post('/logout', (_req, res) => {
  res.clearCookie('token');
  res.redirect('/login.html');
});

// API — all protected
app.get('/api/status', authApi, async (_req, res) => {
  try { res.json(await getStatus()); }
  catch (e) { res.status(500).json({ error: e.message }); }
});

app.get('/api/bets', authApi, (_req, res) => {
  res.json(getRecentBets(100));
});

app.get('/api/strategy', authApi, (_req, res) => {
  // Also count promotion gate events from ledger
  const file = path.join(DATA, 'ledger.jsonl');
  let gatePass = 0, gateBlock = 0;
  if (fs.existsSync(file)) {
    try {
      const lines = fs.readFileSync(file, 'utf8').split('\n').filter(Boolean);
      for (const line of lines) {
        try {
          const o = JSON.parse(line);
          if (o.event === 'ESPORTS_PROMOTION_GATE_AUDIT') {
            if (o.allowed) gatePass++; else gateBlock++;
          }
        } catch {}
      }
    } catch {}
  }
  res.json({ ...STRATEGY, promotion_gate_stats: { passed: gatePass, blocked: gateBlock } });
});

// Process control
const BIN   = path.join(ROOT, 'target/release');
const PROCS = {
  'feed-hub':  { cmd: path.join(BIN, 'feed-hub.exe'),  args: [], cwd: ROOT,     exe: 'feed-hub.exe' },
  'alert-bot': { cmd: path.join(BIN, 'alert-bot.exe'), args: [], cwd: ROOT,     exe: 'alert-bot.exe' },
  'executor':  { cmd: 'node', args: ['index.js'],       cwd: EXECUTOR,           exe: null },
};

app.post('/api/process/start/:name', authApi, (req, res) => {
  const p = PROCS[req.params.name];
  if (!p) return res.status(404).json({ error: 'Unknown process' });
  try {
    const child = spawn(p.cmd, p.args, {
      cwd: p.cwd, detached: true, stdio: 'ignore', env: process.env,
    });
    child.unref();
    res.json({ ok: true, pid: child.pid });
  } catch (e) { res.status(500).json({ error: e.message }); }
});

app.post('/api/process/stop/:name', authApi, (req, res) => {
  const p = PROCS[req.params.name];
  if (!p) return res.status(404).json({ error: 'Unknown process' });
  try {
    if (p.exe) {
      execSync(`taskkill /IM "${p.exe}" /F`, { timeout: 5000 });
    } else {
      // executor: kill node.exe running index.js — find by window title or use port
      execSync(`for /f "tokens=5" %a in ('netstat -aon ^| find ":3030"') do taskkill /PID %a /F`, { shell: 'cmd.exe', timeout: 5000 });
    }
    res.json({ ok: true });
  } catch (e) { res.json({ ok: true, note: 'Process may already be stopped' }); }
});

app.post('/api/killall', authApi, (_req, res) => {
  ['feed-hub.exe', 'alert-bot.exe'].forEach(exe => {
    try { execSync(`taskkill /IM "${exe}" /F`, { timeout: 3000 }); } catch {}
  });
  res.json({ ok: true });
});

// Log tail
app.get('/api/log/:name', authApi, (req, res) => {
  const allowed = ['feed-hub', 'alert-bot'];
  if (!allowed.includes(req.params.name)) return res.status(403).json({ error: 'Forbidden' });
  const today = new Date().toISOString().slice(0, 10);
  // Ledger JSONL has all bet events
  const ledgerFile = path.join(DATA, 'ledger.jsonl');
  // Today's log file
  const logFile = path.join(LOGS_DIR, `${today}.jsonl`);
  
  // For alert-bot: use ledger (bet events)
  // For feed-hub: use today's log file
  const file = req.params.name === 'alert-bot' ? ledgerFile : logFile;
  if (!fs.existsSync(file)) return res.json({ lines: [] });
  try {
    const raw = fs.readFileSync(file, 'utf8').split('\n').filter(Boolean);
    // Return last 100 lines (already pre-parsed as strings, frontend handles formatting)
    const lines = raw.slice(-100);
    res.json({ lines });
  } catch (e) { res.status(500).json({ error: e.message }); }
});

// ── Config API ───────────────────────────────────────────────────────────────
app.get('/api/config', authApi, (_req, res) => {
  res.json(getConfig());
});

app.post('/api/config', authApi, (req, res) => {
  const current = getConfig();
  const body    = req.body || {};
  const next = {
    autobet_enabled: typeof body.autobet_enabled === 'boolean' ? body.autobet_enabled : current.autobet_enabled,
    sport_focus:     Array.isArray(body.sport_focus) ? body.sport_focus.filter(s => typeof s === 'string' && s.length < 32) : current.sport_focus,
    loss_limit:      (typeof body.loss_limit === 'number' && body.loss_limit > 0 && body.loss_limit < 1000) ? +body.loss_limit.toFixed(2) : current.loss_limit,
    max_stake:       (typeof body.max_stake  === 'number' && body.max_stake  > 0 && body.max_stake  < 100)  ? +body.max_stake.toFixed(2)  : current.max_stake,
  };
  try {
    fs.writeFileSync(CONFIG_FILE, JSON.stringify(next, null, 2), 'utf8');
    res.json({ ok: true, config: next });
  } catch (e) { res.status(500).json({ error: e.message }); }
});

// ── Limit raise API (writes signal file for alert_bot) ───────────────────────
const LIMIT_SIGNAL_FILE = path.join(DATA, 'limit_signal.json');

app.post('/api/limit', authApi, (req, res) => {
  const { delta } = req.body || {};
  if (typeof delta !== 'number' || delta <= 0 || delta > 500) {
    return res.status(400).json({ error: 'Delta musí být 1-500 USD' });
  }
  // Read current config / daily_pnl to compute effective state
  const config = getConfig();
  const dailyPnl = readJson(path.join(DATA, 'daily_pnl.json'), {});
  const sodBankroll = dailyPnl?.sod_bankroll ?? 27;
  const baseLimit = 30; // DAILY_LOSS_LIMIT_USD in alert_bot
  const currentOverride = config.loss_limit || baseLimit;
  const newLimit = currentOverride + delta;

  // Update config
  config.loss_limit = +newLimit.toFixed(2);
  try { fs.writeFileSync(CONFIG_FILE, JSON.stringify(config, null, 2), 'utf8'); } catch {}

  // Write signal file for alert_bot to pick up
  const signal = {
    ts: new Date().toISOString(),
    action: 'raise_limit',
    delta,
    new_limit: newLimit,
    source: 'dashboard',
  };
  try { fs.writeFileSync(LIMIT_SIGNAL_FILE, JSON.stringify(signal), 'utf8'); } catch {}

  // Compute room
  const dlFrac = sodBankroll < 150 ? 0.60 : sodBankroll < 500 ? 0.20 : sodBankroll < 1500 ? 0.15 : 0.10;
  const tierCap = sodBankroll * dlFrac;
  const effectiveLimit = Math.min(newLimit, tierCap);
  const netLoss = Math.max((dailyPnl?.wagered || 0) - (dailyPnl?.returned || 0), 0);
  const room = Math.max(effectiveLimit - netLoss, 0);

  res.json({ ok: true, new_limit: newLimit, effective_limit: effectiveLimit, room, delta });
});

// ── Start ─────────────────────────────────────────────────────────────────────
server.listen(PORT, '0.0.0.0', () => {
  console.log(`[dashboard] Listening on http://0.0.0.0:${PORT}`);
  console.log(`[dashboard] Feed-hub health: ${FEED_HEALTH}`);
  console.log(`[dashboard] Executor health: ${EXEC_HEALTH}`);
});
