import fs from 'node:fs/promises';
import path from 'node:path';

function nowIso() {
  return new Date().toISOString();
}

function parseArgs(argv) {
  const out = {
    seconds: 600,
    outPath: path.join(process.cwd(), 'logs', 'monitor_ledger_600s_report.txt'),
    ledgerPath: path.join(process.cwd(), 'data', 'ledger.jsonl'),
    pendingPath: path.join(process.cwd(), 'data', 'pending_claims.txt'),
    historyPath: path.join(process.cwd(), 'data', 'bet_history.txt'),
  };

  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--seconds' || a === '--secs' || a === '-s') {
      const v = Number(argv[i + 1]);
      if (Number.isFinite(v) && v > 0) out.seconds = Math.floor(v);
      i++;
    } else if (a === '--out') {
      const v = argv[i + 1];
      if (v) out.outPath = path.isAbsolute(v) ? v : path.join(process.cwd(), v);
      i++;
    } else if (a === '--ledger') {
      const v = argv[i + 1];
      if (v) out.ledgerPath = path.isAbsolute(v) ? v : path.join(process.cwd(), v);
      i++;
    }
  }
  return out;
}

async function safeStat(filePath) {
  try {
    return await fs.stat(filePath);
  } catch {
    return null;
  }
}

async function safeReadText(filePath) {
  try {
    return await fs.readFile(filePath, 'utf8');
  } catch {
    return '';
  }
}

function splitIntoJsonCandidates(text) {
  // Primary: JSONL by newline.
  // Fallback: some outputs may accidentally end up concatenated with spaces; split on "}{" boundary.
  const lines = text
    .replace(/\r\n/g, '\n')
    .split('\n')
    .flatMap((ln) => {
      const s = ln.trim();
      if (!s) return [];
      if (s.startsWith('{') && s.endsWith('}')) return [s];
      // Conservative: only split if it looks like multiple objects stuck together.
      if (s.includes('}{') || /}\s+{/.test(s)) {
        return s.replace(/}\s*{/g, '}\n{').split('\n').map((x) => x.trim()).filter(Boolean);
      }
      return [s];
    });
  return lines;
}

function tryParseEvent(jsonText) {
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
  }
  return null;
}

function addCount(map, key, inc = 1) {
  map.set(key, (map.get(key) ?? 0) + inc);
}

async function main() {
  const cfg = parseArgs(process.argv);
  const startTime = new Date();

  const startLedgerStat = await safeStat(cfg.ledgerPath);
  const startLedgerSize = startLedgerStat?.size ?? 0;

  const startPendingText = await safeReadText(cfg.pendingPath);
  const startPendingCount = startPendingText ? startPendingText.split(/\r?\n/).filter(Boolean).length : 0;

  const startHistText = await safeReadText(cfg.historyPath);
  const startHistCount = startHistText ? startHistText.split(/\r?\n/).filter(Boolean).length : 0;

  const header = [
    `MONITOR_START=${startTime.toISOString()}`,
    `LEDGER_PATH=${cfg.ledgerPath}`,
    `START_LEDGER_SIZE=${startLedgerSize}`,
    `START_PENDING=${startPendingCount}`,
    `START_HISTORY=${startHistCount}`,
    `DURATION_SEC=${cfg.seconds}`,
  ].join('\n');

  await fs.mkdir(path.dirname(cfg.outPath), { recursive: true });
  await fs.writeFile(cfg.outPath, header + '\n', 'utf8');

  // Sleep.
  await new Promise((r) => setTimeout(r, cfg.seconds * 1000));

  const endTime = new Date();

  // Read appended ledger bytes.
  let deltaText = '';
  try {
    const fh = await fs.open(cfg.ledgerPath, 'r');
    try {
      const stat = await fh.stat();
      const endSize = stat.size;
      const readFrom = Math.min(startLedgerSize, endSize);
      const bytesToRead = Math.max(0, endSize - readFrom);
      if (bytesToRead > 0) {
        const buf = Buffer.alloc(bytesToRead);
        await fh.read(buf, 0, bytesToRead, readFrom);
        deltaText = buf.toString('utf8');
      }
    } finally {
      await fh.close();
    }
  } catch {
    deltaText = '';
  }

  const candidates = splitIntoJsonCandidates(deltaText);
  const events = [];
  let parseFailed = 0;
  for (const c of candidates) {
    const obj = tryParseEvent(c);
    if (obj) events.push(obj);
    else parseFailed++;
  }

  const eventCounts = new Map();
  const placedByPath = new Map();
  const failedByReason = new Map();
  const rejectedByReason = new Map();
  const placedDetails = [];

  for (const e of events) {
    const ev = typeof e.event === 'string' ? e.event : 'UNKNOWN';
    addCount(eventCounts, ev);

    if (ev === 'PLACED') {
      const p = typeof e.path === 'string' ? e.path : 'unknown';
      addCount(placedByPath, p);
      const stake = getNumber(e, ['stake', 'amount_usd', 'stake_usd']) ?? 0;
      placedDetails.push({
        ts: e.ts ?? '',
        match_key: e.match_key ?? '',
        path: p,
        odds: getNumber(e, ['odds']) ?? null,
        stake,
        on_chain_state: e.on_chain_state ?? null,
        bet_id: e.bet_id ?? null,
      });
    }

    if (ev === 'BET_FAILED' || ev === 'ON_CHAIN_REJECTED' || ev === 'REJECTED') {
      const reason =
        (typeof e.reason_code === 'string' && e.reason_code) ||
        (typeof e.reason === 'string' && e.reason) ||
        (typeof e.error === 'string' && e.error) ||
        'unknown';
      const map = ev === 'BET_FAILED' ? failedByReason : rejectedByReason;
      addCount(map, reason);
    }
  }

  const endPendingText = await safeReadText(cfg.pendingPath);
  const endPendingCount = endPendingText ? endPendingText.split(/\r?\n/).filter(Boolean).length : 0;
  const newPending = Math.max(0, endPendingCount - startPendingCount);

  const endHistText = await safeReadText(cfg.historyPath);
  const endHistCount = endHistText ? endHistText.split(/\r?\n/).filter(Boolean).length : 0;
  const newHist = Math.max(0, endHistCount - startHistCount);

  const lines = [];
  lines.push(`MONITOR_END=${endTime.toISOString()}`);
  lines.push(`DURATION_SEC_ACTUAL=${Math.round((endTime.getTime() - startTime.getTime()) / 1000)}`);
  lines.push(`LEDGER_DELTA_BYTES=${Buffer.byteLength(deltaText, 'utf8')}`);
  lines.push(`LEDGER_DELTA_CANDIDATES=${candidates.length}`);
  lines.push(`LEDGER_DELTA_PARSED_EVENTS=${events.length}`);
  lines.push(`LEDGER_DELTA_PARSE_FAILED=${parseFailed}`);
  lines.push('');
  lines.push('# ledger_event_counts');
  for (const [k, v] of [...eventCounts.entries()].sort((a, b) => b[1] - a[1])) {
    lines.push(`${k}=${v}`);
  }
  lines.push('');
  lines.push('# placed_by_path');
  for (const [k, v] of [...placedByPath.entries()].sort((a, b) => b[1] - a[1])) {
    lines.push(`${k}=${v}`);
  }
  lines.push('');
  lines.push('# failed_by_reason (BET_FAILED)');
  for (const [k, v] of [...failedByReason.entries()].sort((a, b) => b[1] - a[1])) {
    lines.push(`${k}=${v}`);
  }
  lines.push('');
  lines.push('# rejected_by_reason (ON_CHAIN_REJECTED/REJECTED)');
  for (const [k, v] of [...rejectedByReason.entries()].sort((a, b) => b[1] - a[1])) {
    lines.push(`${k}=${v}`);
  }
  lines.push('');
  lines.push(`NEW_PENDING_COUNT=${newPending}`);
  lines.push(`NEW_HISTORY_COUNT=${newHist}`);
  lines.push('');
  lines.push('# placed_details (last 40)');
  for (const p of placedDetails.slice(-40)) {
    lines.push(`${p.ts} | ${p.match_key} | path=${p.path} | odds=${p.odds ?? 'n/a'} | stake=${p.stake} | state=${p.on_chain_state ?? 'n/a'}`);
  }
  lines.push('');
  lines.push(`REPORT_GENERATED_AT=${nowIso()}`);

  await fs.appendFile(cfg.outPath, lines.join('\n') + '\n', 'utf8');
  process.stdout.write(cfg.outPath + '\n');
}

main().catch((e) => {
  process.stderr.write(`[monitor_ledger_600s] fatal: ${e?.stack ?? e}\n`);
  process.exit(1);
});
