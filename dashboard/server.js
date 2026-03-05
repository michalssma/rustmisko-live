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

const FEED_HEALTH = 'http://127.0.0.1:8081/health';
const FEED_STATE  = 'http://127.0.0.1:8081/state';
const EXEC_HEALTH = 'http://127.0.0.1:3030/health';

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
  const defaults = { autobet_enabled: true, sport_focus: ['all'], loss_limit: 15.55, max_stake: 3.00 };
  try { return { ...defaults, ...JSON.parse(fs.readFileSync(CONFIG_FILE, 'utf8')) }; }
  catch { return defaults; }
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
async function getStatus() {
  const [feedHealth, execHealth] = await Promise.all([
    fetchJson(FEED_HEALTH),
    fetchJson(EXEC_HEALTH),
  ]);

  const activeBets  = readJson(path.join(DATA, 'active_bets.json'), []);
  const dailyPnl    = readJson(path.join(DATA, 'daily_pnl.json'), {});
  const recentBets  = getRecentBets(500);
  const alertBotAge = getAlertBotAge();

  const processes = {
    'feed-hub':  feedHealth  ? 'running' : getProcessStatus('feed-hub.exe'),
    'executor':  execHealth  ? 'running' : getProcessStatus('node.exe'),
    'alert-bot': alertBotAge >= 0 && alertBotAge < 120_000 ? 'running'
                 : getProcessStatus('alert-bot.exe'),
  };

  const balanceRaw   = execHealth?.balance ?? execHealth?.balanceUsd ?? null;
  const balance      = balanceRaw != null ? parseFloat(balanceRaw) : null;
  // MATIC: fetch native balance via RPC
  let maticBalance = null;
  if (execHealth?.wallet) {
    try {
      const rpcRes = await fetchJson('https://polygon-rpc.com', 5000, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ jsonrpc: '2.0', method: 'eth_getBalance', params: [execHealth.wallet, 'latest'], id: 1 })
      });
      if (rpcRes?.result) {
        const weiBalance = BigInt(rpcRes.result);
        maticBalance = parseFloat((Number(weiBalance) / 1e18).toFixed(4));
        console.log(`[dashboard] MATIC balance: ${maticBalance}`);
      } else {
        console.warn('[dashboard] MATIC RPC: no result field', rpcRes);
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
    pending:       Array.isArray(activeBets) ? activeBets : [],
    pending_count: Array.isArray(activeBets) ? activeBets.length : 0,
    pnl_today:     dailyPnl?.pnl_today  ?? dailyPnl?.total_pnl ?? 0,
    bets_today:    dailyPnl?.bets_today ?? dailyPnl?.total_bets ?? 0,
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
  const file  = path.join(LOGS_DIR, `${today}.jsonl`);
  if (!fs.existsSync(file)) return res.json({ lines: [] });
  try {
    const lines = fs.readFileSync(file, 'utf8').split('\n').filter(Boolean).slice(-100);
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

// ── Start ─────────────────────────────────────────────────────────────────────
server.listen(PORT, '0.0.0.0', () => {
  console.log(`[dashboard] Listening on http://0.0.0.0:${PORT}`);
  console.log(`[dashboard] Feed-hub health: ${FEED_HEALTH}`);
  console.log(`[dashboard] Executor health: ${EXEC_HEALTH}`);
});
