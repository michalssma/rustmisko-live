import fs from 'node:fs/promises';
import { createReadStream } from 'node:fs';
import path from 'node:path';

function nowIso() {
  return new Date().toISOString();
}

function parseArgs(argv) {
  const out = {
    minutes: 300,
    intervalSec: 20,
    feedHubStateUrl: 'http://127.0.0.1:8081/state',
    executorHealthUrl: 'http://127.0.0.1:3030/health',
    ledgerPath: path.join(process.cwd(), 'data', 'ledger.jsonl'),
    pendingPath: path.join(process.cwd(), 'data', 'pending_claims.txt'),
    historyPath: path.join(process.cwd(), 'data', 'bet_history.txt'),
    outDir: path.join(process.cwd(), 'temp'),
    reportDir: path.join(process.cwd(), 'logs'),
    tag: null,
  };

  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--minutes' || a === '--mins' || a === '-m') {
      const v = Number(argv[i + 1]);
      if (Number.isFinite(v) && v > 0) out.minutes = Math.floor(v);
      i++;
    } else if (a === '--interval' || a === '--intervalSec' || a === '-i') {
      const v = Number(argv[i + 1]);
      if (Number.isFinite(v) && v >= 5) out.intervalSec = Math.floor(v);
      i++;
    } else if (a === '--tag') {
      const v = argv[i + 1];
      if (v) out.tag = String(v);
      i++;
    }
  }
  return out;
}

async function safeReadText(filePath) {
  try {
    return await fs.readFile(filePath, 'utf8');
  } catch {
    return '';
  }
}

async function safeStat(filePath) {
  try {
    return await fs.stat(filePath);
  } catch {
    return null;
  }
}

function splitIntoJsonCandidates(text) {
  return text
    .replace(/\r\n/g, '\n')
    .split('\n')
    .flatMap((ln) => {
      const s = ln.trim();
      if (!s) return [];
      if (s.startsWith('{') && s.endsWith('}')) return [s];
      if (s.includes('}{') || /}\s+{/.test(s)) {
        return s.replace(/}\s*{/g, '}\n{').split('\n').map((x) => x.trim()).filter(Boolean);
      }
      return [s];
    });
}

function tryParseJson(jsonText) {
  try {
    const obj = JSON.parse(jsonText);
    if (!obj || typeof obj !== 'object') return null;
    return obj;
  } catch {
    return null;
  }
}

function getNumber(obj, keys) {
  for (const k of keys) {
    const v = obj?.[k];
    if (typeof v === 'number' && Number.isFinite(v)) return v;
    if (typeof v === 'string') {
      const n = Number(v);
      if (Number.isFinite(n)) return n;
    }
  }
  return null;
}

function inc(map, key, by = 1) {
  map.set(key, (map.get(key) ?? 0) + by);
}

function fmtMoney(n) {
  if (!Number.isFinite(n)) return 'n/a';
  return n.toFixed(2);
}

async function fetchJson(url, timeoutMs = 5000) {
  const ctrl = new AbortController();
  const t = setTimeout(() => ctrl.abort(), timeoutMs);
  try {
    const res = await fetch(url, {
      headers: { accept: 'application/json' },
      signal: ctrl.signal,
    });
    if (!res.ok) throw new Error(`HTTP ${res.status} ${res.statusText}`);
    return await res.json();
  } finally {
    clearTimeout(t);
  }
}

function eventDedupKey(e) {
  const ev = typeof e.event === 'string' ? e.event : 'UNKNOWN';
  const betId = typeof e.bet_id === 'string' ? e.bet_id : '';
  const tokenId = typeof e.token_id === 'string' ? e.token_id : (typeof e.token_id === 'number' ? String(e.token_id) : '');
  const ts = typeof e.ts === 'string' ? e.ts : '';
  return `${ev}|${betId}|${tokenId}|${ts}`;
}

function pushLimited(arr, item, limit) {
  arr.push(item);
  if (arr.length > limit) arr.splice(0, arr.length - limit);
}

async function preflight(cfg) {
  const ledgerStat = await safeStat(cfg.ledgerPath);
  if (!ledgerStat) {
    throw new Error(`Ledger not found/readable: ${cfg.ledgerPath}`);
  }

  // Validate endpoints once (fail-fast before 5h run).
  const [feed, executor] = await Promise.all([
    fetchJson(cfg.feedHubStateUrl, 5000),
    fetchJson(cfg.executorHealthUrl, 5000),
  ]);

  // Minimal shape sanity.
  if (!feed || typeof feed !== 'object') throw new Error('feed-hub /state returned non-object JSON');
  if (!executor || typeof executor !== 'object') throw new Error('executor /health returned non-object JSON');
}

async function parseLedgerDeltaStreaming(ledgerPath, startLedgerSize) {
  const endStat = await safeStat(ledgerPath);
  const endSize = endStat?.size ?? 0;

  const readFromRaw = Math.min(startLedgerSize, endSize);
  const bytesToRead = Math.max(0, endSize - readFromRaw);
  const MAX_DELTA_BYTES = 250 * 1024 * 1024; // 250MB safety cap

  // If delta is unexpectedly huge, prefer the last part so we still see end-of-run events.
  const readFrom = bytesToRead > MAX_DELTA_BYTES ? Math.max(0, endSize - MAX_DELTA_BYTES) : readFromRaw;
  const truncated = readFrom !== readFromRaw;

  const stats = {
    endSize,
    startLedgerSize,
    readFrom,
    truncated,
    deltaBytesPlanned: Math.max(0, endSize - readFrom),
    deltaBytesActual: 0,
    candidates: 0,
    parsed: 0,
    parseFailed: 0,
    eventsDeduped: 0,
  };

  const seen = new Set();
  const events = [];

  if (endSize <= readFrom) {
    stats.eventsDeduped = 0;
    return { stats, events };
  }

  const stream = createReadStream(ledgerPath, {
    start: readFrom,
    end: endSize - 1,
    encoding: 'utf8',
  });

  let carry = '';
  for await (const chunk of stream) {
    stats.deltaBytesActual += Buffer.byteLength(chunk, 'utf8');
    carry += chunk;
    const lines = carry.split(/\n/);
    carry = lines.pop() ?? '';
    for (const rawLine of lines) {
      const line = rawLine.replace(/\r/g, '').trim();
      if (!line) continue;
      const candidates = splitIntoJsonCandidates(line);
      for (const c of candidates) {
        stats.candidates++;
        const obj = tryParseJson(c);
        if (!obj) {
          stats.parseFailed++;
          continue;
        }
        stats.parsed++;
        const k = eventDedupKey(obj);
        if (seen.has(k)) continue;
        seen.add(k);
        events.push(obj);
      }
    }
  }

  // Tail
  const tail = carry.trim();
  if (tail) {
    const candidates = splitIntoJsonCandidates(tail);
    for (const c of candidates) {
      stats.candidates++;
      const obj = tryParseJson(c);
      if (!obj) {
        stats.parseFailed++;
        continue;
      }
      stats.parsed++;
      const k = eventDedupKey(obj);
      if (seen.has(k)) continue;
      seen.add(k);
      events.push(obj);
    }
  }

  stats.eventsDeduped = events.length;
  return { stats, events };
}

async function main() {
  const cfg = parseArgs(process.argv);
  const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..+/, ''); // 20260303T222142Z -> compact
  const tag = cfg.tag ? `_${cfg.tag}` : '';

  await fs.mkdir(cfg.outDir, { recursive: true });
  await fs.mkdir(cfg.reportDir, { recursive: true });

  await preflight(cfg);

  const samplesPath = path.join(cfg.outDir, `watch_300m_samples_${stamp}${tag}.jsonl`);
  const metaPath = path.join(cfg.outDir, `watch_300m_meta_${stamp}${tag}.json`);
  const reportPath = path.join(cfg.reportDir, `watch_300m_report_${stamp}${tag}.txt`);

  const startTime = new Date();
  const endTime = new Date(startTime.getTime() + cfg.minutes * 60 * 1000);

  const startLedgerStat = await safeStat(cfg.ledgerPath);
  const startLedgerSize = startLedgerStat?.size ?? 0;

  const startPendingText = await safeReadText(cfg.pendingPath);
  const startPendingCount = startPendingText ? startPendingText.split(/\r?\n/).filter(Boolean).length : 0;
  const startHistText = await safeReadText(cfg.historyPath);
  const startHistCount = startHistText ? startHistText.split(/\r?\n/).filter(Boolean).length : 0;

  const meta = {
    stamp,
    tag: cfg.tag ?? null,
    start: startTime.toISOString(),
    planned_end: endTime.toISOString(),
    minutes: cfg.minutes,
    interval_sec: cfg.intervalSec,
    urls: {
      feedHubStateUrl: cfg.feedHubStateUrl,
      executorHealthUrl: cfg.executorHealthUrl,
    },
    files: {
      ledgerPath: cfg.ledgerPath,
      pendingPath: cfg.pendingPath,
      historyPath: cfg.historyPath,
      samplesPath,
      reportPath,
    },
    baselines: {
      startLedgerSize,
      startPendingCount,
      startHistCount,
    },
  };

  await fs.writeFile(metaPath, JSON.stringify(meta, null, 2), 'utf8');
  await fs.writeFile(samplesPath, '', 'utf8');

  // Sampling loop
  let sampleCount = 0;
  let stopRequested = false;
  const onSigint = () => { stopRequested = true; };
  process.on('SIGINT', onSigint);

  while (new Date() < endTime) {
    if (stopRequested) break;
    const ts = nowIso();
    const sample = {
      ts,
      ok: true,
      feed: null,
      executor: null,
      error: null,
    };
    try {
      const [feed, executor] = await Promise.all([
        fetchJson(cfg.feedHubStateUrl, 5000),
        fetchJson(cfg.executorHealthUrl, 5000),
      ]);
      sample.feed = {
        live_items: feed.live_items ?? null,
        odds_items: feed.odds_items ?? null,
        fused_ready: feed.fused_ready ?? null,
        connections: feed.connections ?? null,
      };
      sample.executor = {
        balance: executor.balance ?? null,
        balanceUsd: executor.balanceUsd ?? null,
        activeBets: executor.activeBets ?? null,
      };
    } catch (e) {
      sample.ok = false;
      sample.error = String(e?.message ?? e);
    }
    try {
      await fs.appendFile(samplesPath, JSON.stringify(sample) + '\n', 'utf8');
    } catch (e) {
      // Disk/write issue: stop early but still try to generate the report.
      stopRequested = true;
    }
    sampleCount++;
    await new Promise((r) => setTimeout(r, cfg.intervalSec * 1000));
  }

  process.off('SIGINT', onSigint);

  const finishTime = new Date();

  // Ledger delta parse (streaming)
  const { stats: ledgerStats, events } = await parseLedgerDeltaStreaming(cfg.ledgerPath, startLedgerSize);

  const eventCounts = new Map();
  const placedByPath = new Map();
  const failedByReason = new Map();
  const rejectedByReason = new Map();

  let stakePlacedSum = 0;
  let placedCount = 0;

  let settledWon = 0;
  let settledLost = 0;
  let settledCanceled = 0;
  let settledStakeSum = 0;
  let settledPayoutSum = 0;
  let settledPnlSum = 0;

  const placedDetails = [];
  const failedDetails = [];

  for (const e of events) {
    const ev = typeof e.event === 'string' ? e.event : 'UNKNOWN';
    inc(eventCounts, ev);

    if (ev === 'PLACED') {
      const p = typeof e.path === 'string' ? e.path : 'unknown';
      inc(placedByPath, p);
      const stake = getNumber(e, ['stake', 'amount_usd', 'stake_usd']) ?? 0;
      stakePlacedSum += stake;
      placedCount++;
      placedDetails.push({
        ts: e.ts ?? '',
        match_key: e.match_key ?? '',
        path: p,
        odds: getNumber(e, ['odds']) ?? null,
        stake,
        on_chain_state: e.on_chain_state ?? null,
        bet_id: e.bet_id ?? null,
        token_id: e.token_id ?? null,
      });

      if (placedDetails.length > 120) placedDetails.splice(0, placedDetails.length - 120);
    }

    if (ev === 'BET_FAILED' || ev === 'ON_CHAIN_REJECTED' || ev === 'REJECTED') {
      const reason =
        (typeof e.reason_code === 'string' && e.reason_code) ||
        (typeof e.reason === 'string' && e.reason) ||
        (typeof e.error === 'string' && e.error) ||
        'unknown';
      const map = ev === 'BET_FAILED' ? failedByReason : rejectedByReason;
      inc(map, reason);
      pushLimited(failedDetails, { ts: e.ts ?? '', event: ev, match_key: e.match_key ?? '', path: e.path ?? '', reason }, 120);
    }

    if (ev === 'WON' || ev === 'LOST' || ev === 'CANCELED') {
      const stake = getNumber(e, ['amount_usd', 'stake', 'stake_usd']) ?? 0;
      const payout = getNumber(e, ['payout_usd', 'payout']) ?? 0;
      const pnl = ev === 'LOST' ? -stake : payout - stake;
      settledStakeSum += stake;
      settledPayoutSum += payout;
      settledPnlSum += pnl;
      if (ev === 'WON') settledWon++;
      if (ev === 'LOST') settledLost++;
      if (ev === 'CANCELED') settledCanceled++;
    }
  }

  // Pending/history delta
  const endPendingText = await safeReadText(cfg.pendingPath);
  const endPendingCount = endPendingText ? endPendingText.split(/\r?\n/).filter(Boolean).length : 0;
  const endHistText = await safeReadText(cfg.historyPath);
  const endHistCount = endHistText ? endHistText.split(/\r?\n/).filter(Boolean).length : 0;

  // Balance stats from samples
  const samplesText = await safeReadText(samplesPath);
  const sampleLines = samplesText.split(/\r?\n/).filter(Boolean);
  let okSamples = 0;
  let errSamples = 0;
  const balances = [];
  for (const ln of sampleLines) {
    const s = tryParseJson(ln);
    if (!s) continue;
    if (s.ok) okSamples++; else errSamples++;
    const bal = getNumber(s.executor, ['balanceUsd', 'balance']);
    if (bal !== null) balances.push(bal);
  }
  const balStart = balances.length ? balances[0] : NaN;
  const balEnd = balances.length ? balances[balances.length - 1] : NaN;
  const balMin = balances.length ? Math.min(...balances) : NaN;
  const balMax = balances.length ? Math.max(...balances) : NaN;

  // Report
  const report = [];
  report.push(`WATCH_START=${startTime.toISOString()}`);
  report.push(`WATCH_END=${finishTime.toISOString()}`);
  report.push(`DURATION_MIN_ACTUAL=${Math.round((finishTime.getTime() - startTime.getTime()) / 60000)}`);
  report.push(`INTERVAL_SEC=${cfg.intervalSec}`);
  report.push(`SAMPLES_TOTAL=${sampleCount}`);
  report.push(`SAMPLES_OK=${okSamples}`);
  report.push(`SAMPLES_ERR=${errSamples}`);
  report.push('');
  report.push('# balance_usd');
  report.push(`start=${fmtMoney(balStart)}`);
  report.push(`end=${fmtMoney(balEnd)}`);
  report.push(`min=${fmtMoney(balMin)}`);
  report.push(`max=${fmtMoney(balMax)}`);
  if (Number.isFinite(balStart) && Number.isFinite(balEnd)) report.push(`delta=${fmtMoney(balEnd - balStart)}`);
  report.push('');
  report.push('# ledger_delta');
  report.push(`START_LEDGER_SIZE=${startLedgerSize}`);
  report.push(`LEDGER_END_SIZE=${ledgerStats.endSize}`);
  report.push(`LEDGER_READ_FROM=${ledgerStats.readFrom}`);
  report.push(`LEDGER_DELTA_TRUNCATED=${ledgerStats.truncated ? 'true' : 'false'}`);
  report.push(`LEDGER_DELTA_BYTES_PLANNED=${ledgerStats.deltaBytesPlanned}`);
  report.push(`LEDGER_DELTA_BYTES_ACTUAL=${ledgerStats.deltaBytesActual}`);
  report.push(`LEDGER_DELTA_CANDIDATES=${ledgerStats.candidates}`);
  report.push(`LEDGER_DELTA_PARSED_EVENTS=${ledgerStats.parsed}`);
  report.push(`LEDGER_DELTA_PARSE_FAILED=${ledgerStats.parseFailed}`);
  report.push(`LEDGER_DELTA_EVENTS_DEDUPED=${ledgerStats.eventsDeduped}`);
  report.push('');
  report.push('# event_counts');
  for (const [k, v] of [...eventCounts.entries()].sort((a, b) => b[1] - a[1])) {
    report.push(`${k}=${v}`);
  }
  report.push('');
  report.push('# placed');
  report.push(`placed_count=${placedCount}`);
  report.push(`placed_stake_sum=${fmtMoney(stakePlacedSum)}`);
  report.push(`placed_avg_stake=${fmtMoney(placedCount ? stakePlacedSum / placedCount : NaN)}`);
  report.push('placed_by_path=');
  for (const [k, v] of [...placedByPath.entries()].sort((a, b) => b[1] - a[1])) {
    report.push(`  ${k}=${v}`);
  }
  report.push('');
  report.push('# settled_pnl');
  report.push(`won=${settledWon}`);
  report.push(`lost=${settledLost}`);
  report.push(`canceled=${settledCanceled}`);
  report.push(`stake_sum=${fmtMoney(settledStakeSum)}`);
  report.push(`payout_sum=${fmtMoney(settledPayoutSum)}`);
  report.push(`pnl_sum=${fmtMoney(settledPnlSum)}`);
  if (settledStakeSum > 0) report.push(`roi=${((settledPnlSum / settledStakeSum) * 100).toFixed(2)}%`);
  report.push('');
  report.push('# bet_failed_by_reason');
  for (const [k, v] of [...failedByReason.entries()].sort((a, b) => b[1] - a[1])) {
    report.push(`${k}=${v}`);
  }
  report.push('');
  report.push('# rejected_by_reason');
  for (const [k, v] of [...rejectedByReason.entries()].sort((a, b) => b[1] - a[1])) {
    report.push(`${k}=${v}`);
  }
  report.push('');
  report.push('# pending/history_delta');
  report.push(`START_PENDING=${startPendingCount}`);
  report.push(`END_PENDING=${endPendingCount}`);
  report.push(`NEW_PENDING=${Math.max(0, endPendingCount - startPendingCount)}`);
  report.push(`START_HISTORY=${startHistCount}`);
  report.push(`END_HISTORY=${endHistCount}`);
  report.push(`NEW_HISTORY=${Math.max(0, endHistCount - startHistCount)}`);
  report.push('');
  report.push('# placed_details_last_60');
  for (const p of placedDetails.slice(-60)) {
    report.push(`${p.ts} | ${p.match_key} | path=${p.path} | odds=${p.odds ?? 'n/a'} | stake=${p.stake} | state=${p.on_chain_state ?? 'n/a'}`);
  }
  report.push('');
  report.push('# bet_fail_details_last_60');
  for (const f of failedDetails.slice(-60)) {
    report.push(`${f.ts} | ${f.event} | ${f.match_key} | path=${f.path || 'n/a'} | reason=${f.reason}`);
  }
  report.push('');
  report.push(`META_PATH=${metaPath}`);
  report.push(`SAMPLES_PATH=${samplesPath}`);
  report.push(`REPORT_GENERATED_AT=${nowIso()}`);

  await fs.writeFile(reportPath, report.join('\n') + '\n', 'utf8');

  process.stdout.write(reportPath + '\n');
}

main().catch((e) => {
  process.stderr.write(`[watch_300m] fatal: ${e?.stack ?? e}\n`);
  process.exit(1);
});
