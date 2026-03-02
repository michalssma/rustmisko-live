import fs from 'node:fs';
import path from 'node:path';
import { execSync } from 'node:child_process';

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function getArg(name, fallback) {
  const key = `--${name}`;
  const idx = process.argv.indexOf(key);
  if (idx >= 0 && idx + 1 < process.argv.length) {
    return process.argv[idx + 1];
  }
  return fallback;
}

function safeJsonParse(line) {
  try {
    return JSON.parse(line);
  } catch {
    return null;
  }
}

function getEventType(row) {
  return String(row?.event || row?.type || '').trim();
}

function getPathLower(row) {
  return String(row?.path || '').toLowerCase();
}

function isAutoPlaced(row) {
  const eventType = getEventType(row);
  if (eventType !== 'PLACED') return false;
  const p = getPathLower(row);
  // Manual command path is "bet_command"; everything else is auto / system-driven
  return p !== 'bet_command';
}

function sportFromMatchKey(matchKey) {
  const mk = String(matchKey || '');
  return mk.includes('::') ? mk.split('::')[0] : '';
}

function summarizeRow(row) {
  const eventType = getEventType(row);
  const mk = row?.match_key || row?.matchKey || '';
  const sport = sportFromMatchKey(mk);
  const reason = row?.reason_code || '';
  const err = row?.error || '';
  const odds = row?.odds ?? row?.requested_odds ?? '';
  const stake = row?.amount_usd ?? row?.stake ?? '';
  const cid = row?.condition_id || '';
  const out = row?.outcome_id || '';
  const pathV = row?.path || '';
  const aid = row?.alert_id ?? '';

  if (eventType === 'PLACED') {
    const betId = row?.bet_id || '';
    return `${eventType} aid=${aid} sport=${sport} mk=${mk} odds=${odds} stake=${stake} path=${pathV} bet_id=${betId}`;
  }
  return `${eventType} aid=${aid} sport=${sport} mk=${mk} path=${pathV} reason=${reason} error=${String(err).slice(0, 140)} cid=${String(cid).slice(0, 18)} out=${String(out).slice(0, 10)}`;
}

function countAutoPlacedBaseline(ledgerPath) {
  if (!fs.existsSync(ledgerPath)) return 0;
  const lines = fs.readFileSync(ledgerPath, 'utf8').split(/\r?\n/).filter(Boolean);
  let count = 0;
  for (const line of lines) {
    const row = safeJsonParse(line);
    if (!row) continue;
    if (isAutoPlaced(row)) count += 1;
  }
  return count;
}

function writeJson(filePath, obj) {
  fs.writeFileSync(filePath, JSON.stringify(obj, null, 2) + '\n', 'utf8');
}

function incMap(map, key) {
  const k = String(key || '');
  map.set(k, (map.get(k) || 0) + 1);
}

function topEntries(map, n) {
  return [...map.entries()].sort((a, b) => b[1] - a[1]).slice(0, n);
}

async function main() {
  const mode = String(getArg('mode', 'stop-on-target'));
  const stopOnTarget = mode !== 'timer';

  const targetDelta = Number(getArg('target', '3'));
  const pollMs = Number(getArg('pollMs', '300'));
  const maxMinutes = Number(getArg('maxMinutes', '120'));
  const drainAfterStopMs = Number(getArg('drainMs', '5000'));

  const ledgerPath = path.resolve(getArg('ledger', 'data/ledger.jsonl'));
  const outRootDefault = `temp/prod_live_autobuy_${new Date().toISOString().replace(/[:.]/g, '').slice(0, 15)}`;
  const runRoot = path.resolve(getArg('out', outRootDefault));
  const stopCmd = String(getArg('stopCmd', 'powershell -ExecutionPolicy Bypass -File .\\start_system.ps1 -Stop'));

  fs.mkdirSync(runRoot, { recursive: true });

  const timelinePath = path.join(runRoot, 'timeline.txt');
  const eventsPath = path.join(runRoot, 'events_captured.jsonl');
  const auditJsonPath = path.join(runRoot, 'audit.json');
  const auditMdPath = path.join(runRoot, 'audit.md');
  const stopOutPath = path.join(runRoot, 'stop_output.txt');

  fs.writeFileSync(timelinePath, `RUN_START=${new Date().toISOString()}\nledger=${ledgerPath}\nstopCmd=${stopCmd}\ntargetAutoPlacedDelta=${targetDelta}\n\n`, 'utf8');
  fs.appendFileSync(timelinePath, `mode=${mode} stopOnTarget=${stopOnTarget}\n`, 'utf8');

  const baseline = countAutoPlacedBaseline(ledgerPath);
  fs.appendFileSync(timelinePath, `baseline_auto_placed=${baseline}\n`, 'utf8');

  // Tail from end-of-file to capture only new events from this run
  let offset = 0;
  try {
    offset = fs.statSync(ledgerPath).size;
  } catch {
    offset = 0;
  }
  let partial = '';

  const placed = [];
  const failed = [];
  const skipped = [];
  const rejected = [];
  const onChainAccepted = [];
  const onChainRejected = [];

  const reasonCounts = new Map();
  const errorCounts = new Map();
  const reasonErrorCounts = new Map();

  function writeAudit(stopTs) {
    const audit = {
      runRoot,
      mode,
      start_ts: new Date(startTs).toISOString(),
      stop_ts: stopTs,
      ledgerPath,
      baseline_auto_placed: baseline,
      new_auto_placed: newAutoPlaced,
      captured: {
        placed: placed.length,
        bet_failed: failed.length,
        auto_bet_skipped: skipped.length,
        rejected: rejected.length,
        on_chain_accepted: onChainAccepted.length,
        on_chain_rejected: onChainRejected.length,
      },
      top_reason_codes: topEntries(reasonCounts, 20),
      top_reason_error: topEntries(reasonErrorCounts, 30),
      sample_placed_auto: placed.filter(isAutoPlaced).slice(0, 10),
      sample_on_chain_accepted: onChainAccepted.slice(0, 10),
      sample_on_chain_rejected: onChainRejected.slice(0, 10),
      sample_failures: failed.slice(0, 10),
      sample_skipped: skipped.slice(0, 10),
    };

    writeJson(auditJsonPath, audit);

    const placedAuto = placed.filter(isAutoPlaced);
    const placedAutoSummary = placedAuto.map((r) => ({
      alert_id: r.alert_id,
      bet_id: r.bet_id,
      match_key: r.match_key,
      sport: sportFromMatchKey(r.match_key),
      path: r.path,
      odds: r.odds,
      amount_usd: r.amount_usd,
      min_odds: r.min_odds,
      condition_id: r.condition_id,
      outcome_id: r.outcome_id,
      condition_age_ms: r.condition_age_ms,
      pipeline_ms: r.pipeline_ms,
      rtt_ms: r.rtt_ms,
      decision_ts: r.decision_ts,
    }));

    const failureSummary = failed.map((r) => ({
      alert_id: r.alert_id,
      match_key: r.match_key,
      sport: sportFromMatchKey(r.match_key),
      path: r.path,
      reason_code: r.reason_code,
      error: r.error,
      requested_odds: r.requested_odds,
      min_odds: r.min_odds,
      stake: r.stake,
      condition_age_ms: r.condition_age_ms,
      pipeline_ms: r.pipeline_ms,
      rtt_ms: r.rtt_ms,
      retries: r.retries,
      condition_id: r.condition_id,
      outcome_id: r.outcome_id,
    }));

    const skippedSummary = skipped.map((r) => ({
      alert_id: r.alert_id,
      match_key: r.match_key,
      sport: sportFromMatchKey(r.match_key),
      path: r.path,
      reason_code: r.reason_code,
      error: r.error,
      requested_odds: r.requested_odds,
      stake: r.stake,
      ws_state: r.ws_state,
      ws_age_ms: r.ws_age_ms,
      condition_age_ms: r.condition_age_ms,
      condition_id: r.condition_id,
      outcome_id: r.outcome_id,
    }));

    writeJson(path.join(runRoot, 'placed_auto.json'), placedAutoSummary);
    writeJson(path.join(runRoot, 'bet_failed.json'), failureSummary);
    writeJson(path.join(runRoot, 'auto_bet_skipped.json'), skippedSummary);

    const md = [];
    md.push(`# AUTO-BUY AUDIT`);
    md.push('');
    md.push(`- start: ${new Date(startTs).toISOString()}`);
    md.push(`- stop: ${stopTs || ''}`);
    md.push(`- mode: ${mode}`);
    md.push(`- baseline_auto_placed: ${baseline}`);
    md.push(`- new_auto_placed: ${newAutoPlaced}`);
    md.push(`- captured: PLACED=${placed.length}, BET_FAILED=${failed.length}, AUTO_BET_SKIPPED=${skipped.length}, REJECTED=${rejected.length}, ON_CHAIN_ACCEPTED=${onChainAccepted.length}, ON_CHAIN_REJECTED=${onChainRejected.length}`);
    md.push('');
    md.push(`## On-chain (Accepted/Rejected)`);
    if (onChainAccepted.length === 0 && onChainRejected.length === 0) {
      md.push('- (žádné)');
    } else {
      md.push(`- accepted: ${onChainAccepted.length}`);
      md.push(`- rejected: ${onChainRejected.length}`);
    }
    md.push('');
    md.push(`## Prošly (AUTO PLACED)`);
    if (placedAutoSummary.length === 0) {
      md.push('- (žádné)');
    } else {
      for (const pRow of placedAutoSummary.slice(0, 20)) {
        md.push(`- aid=${pRow.alert_id} sport=${pRow.sport} mk=${pRow.match_key} odds=${pRow.odds} stake=${pRow.amount_usd} min_odds=${pRow.min_odds} age_ms=${pRow.condition_age_ms} rtt=${pRow.rtt_ms}ms pipe=${pRow.pipeline_ms}ms bet_id=${pRow.bet_id}`);
      }
    }
    md.push('');
    md.push(`## Neprošly (BET_FAILED)`);
    if (failureSummary.length === 0) {
      md.push('- (žádné)');
    } else {
      for (const fRow of failureSummary.slice(0, 30)) {
        md.push(`- aid=${fRow.alert_id} sport=${fRow.sport} mk=${fRow.match_key} reason=${fRow.reason_code} odds=${fRow.requested_odds} min_odds=${fRow.min_odds} stake=${fRow.stake} retries=${fRow.retries} age_ms=${fRow.condition_age_ms} err=${String(fRow.error).slice(0, 160)}`);
      }
    }
    md.push('');
    md.push(`## Skipped (AUTO_BET_SKIPPED)`);
    if (skippedSummary.length === 0) {
      md.push('- (žádné)');
    } else {
      for (const sRow of skippedSummary.slice(0, 30)) {
        md.push(`- aid=${sRow.alert_id} sport=${sRow.sport} mk=${sRow.match_key} reason=${sRow.reason_code} ws_age_ms=${sRow.ws_age_ms ?? ''} gql_age_ms=${sRow.condition_age_ms ?? ''} err=${String(sRow.error).slice(0, 160)}`);
      }
    }
    md.push('');
    md.push('## Top důvody (reason_code)');
    for (const [k, v] of topEntries(reasonCounts, 20)) {
      md.push(`- ${k}: ${v}`);
    }
    md.push('');
    md.push('## Top kombinace (reason_code | error)');
    for (const [k, v] of topEntries(reasonErrorCounts, 20)) {
      md.push(`- ${k}: ${v}`);
    }
    md.push('');

    fs.writeFileSync(auditMdPath, md.join('\n') + '\n', 'utf8');
    fs.appendFileSync(timelinePath, `AUDIT_WRITTEN ts=${new Date().toISOString()}\n`, 'utf8');
    console.log(`[watch] audit written: ${auditMdPath}`);
  }

  let newAutoPlaced = 0;
  let stopTriggered = false;
  let stopTriggeredAt = null;

  let lastHeartbeat = Date.now();
  const startTs = Date.now();
  const deadlineTs = startTs + maxMinutes * 60_000;

  console.log(`[watch] runRoot=${runRoot}`);
  console.log(`[watch] mode=${mode} stopOnTarget=${stopOnTarget}`);
  console.log(`[watch] baseline_auto_placed=${baseline} tail_offset=${offset}`);

  while (Date.now() < deadlineTs) {
    // heartbeat every 30s
    if (Date.now() - lastHeartbeat > 30_000) {
      lastHeartbeat = Date.now();
      console.log(`[watch] ts=${new Date().toISOString()} new_auto_placed=${newAutoPlaced}/${targetDelta} captured placed=${placed.length} failed=${failed.length} skipped=${skipped.length} rejected=${rejected.length} on_chain_accepted=${onChainAccepted.length} on_chain_rejected=${onChainRejected.length}`);
    }

    let size;
    try {
      size = fs.statSync(ledgerPath).size;
    } catch {
      await sleep(pollMs);
      continue;
    }

    if (size > offset) {
      const chunk = fs.readFileSync(ledgerPath).slice(offset, size).toString('utf8');
      offset = size;
      const text = partial + chunk;
      const lines = text.split(/\r?\n/);
      partial = lines.pop() || '';

      for (const line of lines) {
        if (!line.trim()) continue;
        const row = safeJsonParse(line);
        if (!row) continue;

        fs.appendFileSync(eventsPath, line + '\n', 'utf8');

        const eventType = getEventType(row);
        const p = getPathLower(row);

        if (eventType === 'PLACED') {
          placed.push(row);
          if (isAutoPlaced(row)) {
            newAutoPlaced += 1;
            console.log(`[PLACED][AUTO] ${summarizeRow(row)}`);
          } else {
            console.log(`[PLACED][MANUAL] ${summarizeRow(row)}`);
          }
        } else if (eventType === 'BET_FAILED') {
          failed.push(row);
          const reason = row?.reason_code || 'Unknown';
          const err = row?.error || '';
          incMap(reasonCounts, reason);
          incMap(errorCounts, err);
          incMap(reasonErrorCounts, `${reason} | ${err}`);
          console.log(`[BET_FAILED] ${summarizeRow(row)}`);
        } else if (eventType === 'AUTO_BET_SKIPPED') {
          skipped.push(row);
          const reason = row?.reason_code || 'Unknown';
          const err = row?.error || '';
          incMap(reasonCounts, reason);
          incMap(errorCounts, err);
          incMap(reasonErrorCounts, `${reason} | ${err}`);
          console.log(`[SKIPPED] ${summarizeRow(row)}`);
        } else if (eventType === 'REJECTED') {
          rejected.push(row);
          const reason = row?.reason_code || 'Unknown';
          const err = row?.error || '';
          incMap(reasonCounts, reason);
          incMap(errorCounts, err);
          incMap(reasonErrorCounts, `${reason} | ${err}`);
          console.log(`[REJECTED] ${summarizeRow(row)}`);
        } else if (eventType === 'ON_CHAIN_ACCEPTED') {
          onChainAccepted.push(row);
          console.log(`[ON_CHAIN_ACCEPTED] ${summarizeRow(row)}`);
        } else if (eventType === 'ON_CHAIN_REJECTED') {
          onChainRejected.push(row);
          const err = row?.error || row?.errorMessage || '';
          incMap(errorCounts, err);
          incMap(reasonErrorCounts, `ON_CHAIN_REJECTED | ${err}`);
          console.log(`[ON_CHAIN_REJECTED] ${summarizeRow(row)}`);
        }

        if (stopOnTarget && !stopTriggered && newAutoPlaced >= targetDelta) {
          stopTriggered = true;
          stopTriggeredAt = new Date().toISOString();
          fs.appendFileSync(timelinePath, `TARGET_REACHED ts=${stopTriggeredAt} new_auto_placed=${newAutoPlaced}\n`, 'utf8');
          console.log(`[watch] TARGET_REACHED ts=${stopTriggeredAt} -> stopping system...`);

          try {
            const out = execSync(stopCmd, { encoding: 'utf8' });
            fs.writeFileSync(stopOutPath, out || '', 'utf8');
            fs.appendFileSync(timelinePath, `SYSTEM_STOPPED ts=${new Date().toISOString()}\n`, 'utf8');
          } catch (e) {
            const msg = e?.stdout?.toString?.() || e?.message || String(e);
            fs.writeFileSync(stopOutPath, msg, 'utf8');
            fs.appendFileSync(timelinePath, `SYSTEM_STOP_FAILED ts=${new Date().toISOString()}\n`, 'utf8');
          }

          // Drain any trailing ledger writes after stop
          await sleep(drainAfterStopMs);

          // Final read any remaining bytes
          try {
            const finalSize = fs.statSync(ledgerPath).size;
            if (finalSize > offset) {
              const finalChunk = fs.readFileSync(ledgerPath).slice(offset, finalSize).toString('utf8');
              offset = finalSize;
              const finalText = partial + finalChunk;
              const finalLines = finalText.split(/\r?\n/);
              partial = finalLines.pop() || '';
              for (const fl of finalLines) {
                if (!fl.trim()) continue;
                const r = safeJsonParse(fl);
                if (!r) continue;
                fs.appendFileSync(eventsPath, fl + '\n', 'utf8');
                const et = getEventType(r);
                if (et === 'PLACED') placed.push(r);
                else if (et === 'BET_FAILED') failed.push(r);
                else if (et === 'AUTO_BET_SKIPPED') skipped.push(r);
                else if (et === 'REJECTED') rejected.push(r);
                else if (et === 'ON_CHAIN_ACCEPTED') onChainAccepted.push(r);
                else if (et === 'ON_CHAIN_REJECTED') onChainRejected.push(r);
              }
            }
          } catch {
            // ignore
          }

          writeAudit(stopTriggeredAt);
          return;
        }
      }
    }

    await sleep(pollMs);
  }

  const timeoutAt = new Date().toISOString();
  fs.appendFileSync(timelinePath, `TIMEOUT ts=${timeoutAt} new_auto_placed=${newAutoPlaced}\n`, 'utf8');
  console.log(`[watch] TIMEOUT after ${maxMinutes} minutes (new_auto_placed=${newAutoPlaced}/${targetDelta})`);
  writeAudit(null);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
