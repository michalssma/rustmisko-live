#!/usr/bin/env node
/*
  Today audit for MiskoLive
  - Reads data/ledger.jsonl and logs/alert_bot.log
  - Focus: how many score/edge bets vs anomaly bets happened today, list each bet, and compute PnL.

  Usage:
    node tools/today_audit.mjs
    node tools/today_audit.mjs 2026-03-03

  Notes:
    - Uses the PLACED event's `path` for classification.
    - Settlement (WON/LOST) is linked by bet_id.
*/

import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';

function isoDateUtc(d = new Date()) {
  return d.toISOString().slice(0, 10);
}

function nowStamp() {
  // YYYYMMDD_HHMMSS
  const d = new Date();
  const pad = (n) => String(n).padStart(2, '0');
  return (
    String(d.getUTCFullYear()) +
    pad(d.getUTCMonth() + 1) +
    pad(d.getUTCDate()) +
    '_' +
    pad(d.getUTCHours()) +
    pad(d.getUTCMinutes()) +
    pad(d.getUTCSeconds())
  );
}

function safeJsonParse(line) {
  try {
    return JSON.parse(line);
  } catch {
    return null;
  }
}

function getTs(row) {
  return row?.ts ?? row?.decision_ts ?? row?.response_ts ?? row?.send_ts ?? null;
}

function isSameIsoDatePrefix(ts, datePrefix) {
  if (!ts || typeof ts !== 'string') return false;
  // Ledger/log timestamps are ISO-like with leading YYYY-MM-DD
  return ts.startsWith(datePrefix);
}

function classifyFamily(placedPath) {
  const p = String(placedPath ?? '').toLowerCase();
  if (p === 'anomaly_odds') return 'anomaly';
  if (p === 'score_edge') return 'score';
  // legacy/internal label used in some rows
  if (p === 'edge') return 'score';
  return 'other';
}

function num(v) {
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

function fmtUsd(v) {
  if (!Number.isFinite(v)) return '';
  return `$${v.toFixed(2)}`;
}

function fmtPct(v) {
  if (!Number.isFinite(v)) return '';
  return `${(v * 100).toFixed(2)}%`;
}

function csvEscape(s) {
  const v = String(s ?? '');
  if (/[",\n]/.test(v)) return `"${v.replaceAll('"', '""')}"`;
  return v;
}

async function readLedger({ repoRoot, datePrefix }) {
  const ledgerPath = path.join(repoRoot, 'data', 'ledger.jsonl');

  const placedByBetId = new Map();
  const acceptedByBetId = new Map();
  const settledByBetId = new Map();
  const betFailedRows = [];

  let totalLines = 0;
  let parsedLines = 0;
  let todayRows = 0;

  const rl = readline.createInterface({
    input: fs.createReadStream(ledgerPath, { encoding: 'utf8' }),
    crlfDelay: Infinity,
  });

  for await (const line of rl) {
    totalLines++;
    if (!line || line[0] !== '{') continue;
    const row = safeJsonParse(line);
    if (!row) continue;
    parsedLines++;

    const ts = getTs(row);
    if (!isSameIsoDatePrefix(ts, datePrefix)) continue;
    todayRows++;

    const event = row.event ?? row.type;
    if (event === 'PLACED') {
      const betId = row.bet_id;
      if (!betId) continue;

      // `amount_usd` is the common stake field on PLACED; sometimes `stake` exists too.
      const stakeUsd = num(row.amount_usd) ?? num(row.stake) ?? null;
      placedByBetId.set(betId, {
        bet_id: betId,
        family: classifyFamily(row.path),
        placed_path: row.path,
        ts,
        match_key: row.match_key,
        value_team: row.value_team,
        odds: num(row.odds),
        stake_usd: stakeUsd,
        condition_id: row.condition_id,
        outcome_id: row.outcome_id,
        alert_id: row.alert_id,
      });
    } else if (event === 'ON_CHAIN_ACCEPTED') {
      const betId = row.bet_id;
      if (!betId) continue;
      acceptedByBetId.set(betId, {
        bet_id: betId,
        ts,
        on_chain_state: row.on_chain_state,
        token_id: row.token_id,
        stake: num(row.stake),
        odds: num(row.odds),
        path: row.path,
      });
    } else if (event === 'WON' || event === 'LOST') {
      const betId = row.bet_id;
      if (!betId) continue;
      settledByBetId.set(betId, {
        bet_id: betId,
        ts,
        result: event,
        amount_usd: num(row.amount_usd) ?? num(row.stake),
        payout_usd: num(row.payout_usd),
        odds: num(row.odds),
        path: row.path,
        match_key: row.match_key,
        value_team: row.value_team,
        alert_id: row.alert_id,
      });
    } else if (event === 'BET_FAILED') {
      betFailedRows.push({
        ts,
        family: classifyFamily(row.path),
        path: row.path,
        match_key: row.match_key,
        outcome_id: row.outcome_id,
        condition_id: row.condition_id,
        requested_odds: num(row.requested_odds),
        min_odds: num(row.min_odds),
        stake: num(row.stake),
        reason_code: row.reason_code,
        error: row.error,
        alert_id: row.alert_id,
      });
    }
  }

  return {
    ledgerPath,
    totalLines,
    parsedLines,
    todayRows,
    placedByBetId,
    acceptedByBetId,
    settledByBetId,
    betFailedRows,
  };
}

function computePlacedStats(placedRows) {
  const stats = {
    placed_count: 0,
    settled_count: 0,
    won_count: 0,
    lost_count: 0,
    open_count: 0,
    stake_usd: 0,
    payout_usd: 0,
    pnl_usd: 0,
    avg_odds: null,
  };

  let oddsSum = 0;
  let oddsN = 0;

  for (const r of placedRows) {
    stats.placed_count++;
    if (Number.isFinite(r.stake_usd)) stats.stake_usd += r.stake_usd;
    if (Number.isFinite(r.odds)) {
      oddsSum += r.odds;
      oddsN++;
    }

    if (r.settlement?.result === 'WON') {
      stats.settled_count++;
      stats.won_count++;
      if (Number.isFinite(r.settlement.payout_usd)) stats.payout_usd += r.settlement.payout_usd;
      // PnL: payout - stake
      const stake = Number.isFinite(r.stake_usd) ? r.stake_usd : (Number.isFinite(r.settlement.amount_usd) ? r.settlement.amount_usd : 0);
      const payout = Number.isFinite(r.settlement.payout_usd) ? r.settlement.payout_usd : 0;
      stats.pnl_usd += payout - stake;
    } else if (r.settlement?.result === 'LOST') {
      stats.settled_count++;
      stats.lost_count++;
      const stake = Number.isFinite(r.stake_usd) ? r.stake_usd : (Number.isFinite(r.settlement.amount_usd) ? r.settlement.amount_usd : 0);
      stats.pnl_usd += -stake;
    } else {
      stats.open_count++;
    }
  }

  stats.avg_odds = oddsN > 0 ? oddsSum / oddsN : null;
  stats.roi = stats.stake_usd > 0 ? stats.pnl_usd / stats.stake_usd : null;
  stats.winrate = stats.settled_count > 0 ? stats.won_count / stats.settled_count : null;

  return stats;
}

async function readBotLog({ repoRoot, datePrefix }) {
  const logPath = path.join(repoRoot, 'logs', 'alert_bot.log');
  if (!fs.existsSync(logPath)) {
    return { logPath, exists: false };
  }

  let totalTodayLines = 0;
  let pollLines = 0;
  let sumScoreEdges = 0;
  let sumOddsAnoms = 0;
  let sumSent = 0;

  let scoreNotActionable = 0;
  let oddsSkippedAutoBet = 0;
  let pendingCapBlocks = 0;

  const pollRe = /Poll: (\d+) score edges, (\d+) odds anomalies, (\d+) sent/;
  const lineDateRe = /^\s*\uFEFF?(\d{4}-\d{2}-\d{2})/;
  const ansiRe = /\x1B\[[0-9;]*m/g;

  const rl = readline.createInterface({
    input: fs.createReadStream(logPath, { encoding: 'utf8' }),
    crlfDelay: Infinity,
  });

  for await (const line of rl) {
    const clean = line.replace(ansiRe, '');
    const mDate = clean.match(lineDateRe);
    if (!mDate || mDate[1] !== datePrefix) continue;
    totalTodayLines++;

    const m = clean.match(pollRe);
    if (m) {
      pollLines++;
      sumScoreEdges += Number(m[1]);
      sumOddsAnoms += Number(m[2]);
      sumSent += Number(m[3]);
    }

    if (clean.includes('score not actionable')) scoreNotActionable++;
    if (clean.includes('ODDS ANOMALY') && clean.includes('skipped for auto-bet')) oddsSkippedAutoBet++;
    if (clean.toLowerCase().includes('pending') && clean.toLowerCase().includes('cap')) pendingCapBlocks++;
  }

  return {
    logPath,
    exists: true,
    totalTodayLines,
    pollLines,
    sumScoreEdges,
    sumOddsAnoms,
    sumSent,
    scoreNotActionable,
    oddsSkippedAutoBet,
    pendingCapBlocks,
  };
}

function toMarkdownReport({ datePrefix, ledger, placedRows, byFamilyStats, betFailedRows, logStats }) {
  const lines = [];
  lines.push(`# TODAY AUDIT — ${datePrefix} (UTC)`);
  lines.push('');

  lines.push('## Summary (PLACED bets)');
  lines.push('');
  lines.push('| family | placed | settled | won | lost | open | stake | payout | pnl | roi | avg_odds |');
  lines.push('|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|');

  const families = ['score', 'anomaly', 'other'];
  for (const fam of families) {
    const s = byFamilyStats[fam] ?? computePlacedStats([]);
    lines.push(
      `| ${fam} | ${s.placed_count} | ${s.settled_count} | ${s.won_count} | ${s.lost_count} | ${s.open_count} | ${fmtUsd(s.stake_usd)} | ${fmtUsd(s.payout_usd)} | ${fmtUsd(s.pnl_usd)} | ${fmtPct(s.roi)} | ${s.avg_odds ? s.avg_odds.toFixed(2) : ''} |`
    );
  }

  lines.push('');
  lines.push('## Failed bet attempts (BET_FAILED)');
  lines.push('');
  const failedByFamily = betFailedRows.reduce((acc, r) => {
    acc[r.family] = (acc[r.family] ?? 0) + 1;
    return acc;
  }, {});
  lines.push(`Total BET_FAILED rows: ${betFailedRows.length}`);
  lines.push(`- score: ${failedByFamily.score ?? 0}`);
  lines.push(`- anomaly: ${failedByFamily.anomaly ?? 0}`);
  lines.push(`- other: ${failedByFamily.other ?? 0}`);

  const reasonCounts = new Map();
  for (const r of betFailedRows) {
    const key = `${r.family}::${r.reason_code ?? 'Unknown'}`;
    reasonCounts.set(key, (reasonCounts.get(key) ?? 0) + 1);
  }
  const topReasons = [...reasonCounts.entries()].sort((a, b) => b[1] - a[1]).slice(0, 10);
  if (topReasons.length) {
    lines.push('');
    lines.push('Top BET_FAILED reasons:');
    for (const [k, c] of topReasons) lines.push(`- ${k}: ${c}`);
  }

  if (logStats?.exists) {
    lines.push('');
    lines.push('## Log signals (alert_bot.log)');
    lines.push('');
    lines.push(`- Poll lines: ${logStats.pollLines} (today log lines: ${logStats.totalTodayLines})`);
    lines.push(`- Detected: score_edges=${logStats.sumScoreEdges}, odds_anomalies=${logStats.sumOddsAnoms}, sent=${logStats.sumSent}`);
    lines.push(`- "score not actionable" lines: ${logStats.scoreNotActionable}`);
    lines.push(`- "ODDS ANOMALY ... skipped for auto-bet" lines: ${logStats.oddsSkippedAutoBet}`);
    lines.push(`- Lines mentioning "pending" + "cap": ${logStats.pendingCapBlocks}`);
  }

  lines.push('');
  lines.push('## Placed bets (detailed)');
  lines.push('');
  lines.push('| ts | family | path | match_key | odds | stake | result | payout | pnl | value_team | bet_id |');
  lines.push('|---|---|---|---|---:|---:|---|---:|---:|---|---|');

  const sorted = [...placedRows].sort((a, b) => String(a.ts).localeCompare(String(b.ts)));
  for (const r of sorted) {
    const stake = Number.isFinite(r.stake_usd) ? r.stake_usd : null;
    const result = r.settlement?.result ?? '';
    const payout = r.settlement?.payout_usd ?? null;
    let pnl = null;
    if (result === 'WON') pnl = (Number.isFinite(payout) ? payout : 0) - (Number.isFinite(stake) ? stake : 0);
    if (result === 'LOST') pnl = -(Number.isFinite(stake) ? stake : 0);

    lines.push(
      `| ${r.ts ?? ''} | ${r.family} | ${r.placed_path ?? ''} | ${r.match_key ?? ''} | ${r.odds ?? ''} | ${stake ?? ''} | ${result} | ${payout ?? ''} | ${pnl ?? ''} | ${r.value_team ?? ''} | ${r.bet_id} |`
    );
  }

  lines.push('');
  lines.push('## Failed attempts (detailed, first 200)');
  lines.push('');
  lines.push('| ts | family | path | match_key | requested_odds | min_odds | stake | reason_code | error |');
  lines.push('|---|---|---|---|---:|---:|---:|---|---|');

  for (const r of betFailedRows.slice(0, 200)) {
    lines.push(
      `| ${r.ts ?? ''} | ${r.family} | ${r.path ?? ''} | ${r.match_key ?? ''} | ${r.requested_odds ?? ''} | ${r.min_odds ?? ''} | ${r.stake ?? ''} | ${r.reason_code ?? ''} | ${String(r.error ?? '').slice(0, 120)} |`
    );
  }

  lines.push('');
  lines.push('---');
  lines.push(`Source ledger: ${ledger.ledgerPath}`);
  if (logStats?.logPath) lines.push(`Source log: ${logStats.logPath}`);
  return lines.join('\n');
}

async function main() {
  const repoRoot = process.cwd();
  const datePrefix = process.argv[2] || isoDateUtc();

  const stamp = nowStamp();
  const outDir = path.join(repoRoot, 'temp', `today_audit_${datePrefix}_${stamp}`);
  fs.mkdirSync(outDir, { recursive: true });

  const ledger = await readLedger({ repoRoot, datePrefix });

  // Attach settlements to placed bets (classification follows PLACED.path)
  const placedRows = [];
  for (const p of ledger.placedByBetId.values()) {
    const settlement = ledger.settledByBetId.get(p.bet_id) ?? null;
    const accepted = ledger.acceptedByBetId.get(p.bet_id) ?? null;
    placedRows.push({ ...p, accepted, settlement });
  }

  const byFamily = { score: [], anomaly: [], other: [] };
  for (const r of placedRows) {
    (byFamily[r.family] ?? byFamily.other).push(r);
  }

  const byFamilyStats = {
    score: computePlacedStats(byFamily.score),
    anomaly: computePlacedStats(byFamily.anomaly),
    other: computePlacedStats(byFamily.other),
  };

  const logStats = await readBotLog({ repoRoot, datePrefix });

  const md = toMarkdownReport({
    datePrefix,
    ledger,
    placedRows,
    byFamilyStats,
    betFailedRows: ledger.betFailedRows,
    logStats,
  });

  const reportPath = path.join(outDir, 'report.md');
  fs.writeFileSync(reportPath, md, 'utf8');

  // Also write machine-readable summaries
  const summary = {
    datePrefix,
    ledger: {
      path: ledger.ledgerPath,
      totalLines: ledger.totalLines,
      parsedLines: ledger.parsedLines,
      todayRows: ledger.todayRows,
      placedCount: placedRows.length,
      betFailedCount: ledger.betFailedRows.length,
    },
    placed: {
      score: byFamilyStats.score,
      anomaly: byFamilyStats.anomaly,
      other: byFamilyStats.other,
    },
    log: logStats,
  };

  fs.writeFileSync(path.join(outDir, 'summary.json'), JSON.stringify(summary, null, 2), 'utf8');

  // CSV for quick slicing
  const csvHeader = [
    'ts',
    'family',
    'path',
    'match_key',
    'odds',
    'stake_usd',
    'result',
    'payout_usd',
    'pnl_usd',
    'value_team',
    'bet_id',
  ];
  const csvLines = [csvHeader.join(',')];
  const sorted = [...placedRows].sort((a, b) => String(a.ts).localeCompare(String(b.ts)));
  for (const r of sorted) {
    const stake = Number.isFinite(r.stake_usd) ? r.stake_usd : '';
    const result = r.settlement?.result ?? '';
    const payout = Number.isFinite(r.settlement?.payout_usd) ? r.settlement.payout_usd : '';
    let pnl = '';
    if (result === 'WON') pnl = (Number(payout) || 0) - (Number(stake) || 0);
    if (result === 'LOST') pnl = -(Number(stake) || 0);

    const row = [
      r.ts ?? '',
      r.family,
      r.placed_path ?? '',
      r.match_key ?? '',
      r.odds ?? '',
      stake,
      result,
      payout,
      pnl,
      r.value_team ?? '',
      r.bet_id,
    ].map(csvEscape);
    csvLines.push(row.join(','));
  }
  fs.writeFileSync(path.join(outDir, 'placed_bets.csv'), csvLines.join('\n'), 'utf8');

  // Minimal console output
  console.log(`Wrote report: ${reportPath}`);
  console.log(`Placed today: score=${byFamilyStats.score.placed_count}, anomaly=${byFamilyStats.anomaly.placed_count}, other=${byFamilyStats.other.placed_count}`);
  console.log(`Settled PnL: score=${fmtUsd(byFamilyStats.score.pnl_usd)}, anomaly=${fmtUsd(byFamilyStats.anomaly.pnl_usd)}, other=${fmtUsd(byFamilyStats.other.pnl_usd)}`);
  console.log(`BET_FAILED today: ${ledger.betFailedRows.length} (score=${(ledger.betFailedRows.filter(r=>r.family==='score')).length}, anomaly=${(ledger.betFailedRows.filter(r=>r.family==='anomaly')).length})`);
}

main().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
