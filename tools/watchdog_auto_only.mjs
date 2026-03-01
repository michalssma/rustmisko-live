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

function scanAutoStats(ledgerPath) {
  if (!fs.existsSync(ledgerPath)) {
    return { placed: 0, attempts: 0, failed: 0, skipped: 0, rejected: 0 };
  }
  const lines = fs.readFileSync(ledgerPath, 'utf8').split(/\r?\n/).filter(Boolean);
  let placed = 0;
  let failed = 0;
  let skipped = 0;
  let rejected = 0;
  for (const line of lines) {
    let row;
    try {
      row = JSON.parse(line);
    } catch {
      continue;
    }
    const event = String(row.event || row.type || '');
    const p = String(row.path || '').toLowerCase();
    if (p !== 'bet_command') {
      if (event === 'PLACED') placed += 1;
      else if (event === 'BET_FAILED') failed += 1;
      else if (event === 'AUTO_BET_SKIPPED') skipped += 1;
      else if (event === 'REJECTED') rejected += 1;
    }
  }
  return {
    placed,
    failed,
    skipped,
    rejected,
    attempts: placed + failed + skipped + rejected,
  };
}

async function main() {
  const targetDelta = Number(getArg('target', '3'));
  const intervalSec = Number(getArg('interval', '5'));
  const maxTicks = Number(getArg('ticks', '2160'));
  const stopOnReach = String(getArg('stop', 'true')).toLowerCase() === 'true';
  const ledgerPath = path.resolve(getArg('ledger', 'data/ledger.jsonl'));
  const runRoot = path.resolve(getArg('out', `temp/prod_autoonly_${new Date().toISOString().replace(/[:.]/g, '').slice(0, 15)}`));

  fs.mkdirSync(runRoot, { recursive: true });
  const logPath = path.join(runRoot, 'watchdog.txt');

  const initial = scanAutoStats(ledgerPath);
  fs.writeFileSync(
    logPath,
    `START=${new Date().toISOString()} initial_auto_placed=${initial.placed} initial_attempts=${initial.attempts} target_delta=${targetDelta}\n`,
    'utf8'
  );

  for (let i = 1; i <= maxTicks; i++) {
    await sleep(intervalSec * 1000);
    const current = scanAutoStats(ledgerPath);
    const delta = current.placed - initial.placed;
    const attemptsDelta = current.attempts - initial.attempts;
    fs.appendFileSync(
      logPath,
      `t=${i} ts=${new Date().toISOString()} auto_placed_now=${current.placed} placed_delta=${delta} attempts_now=${current.attempts} attempts_delta=${attemptsDelta} failed=${current.failed} skipped=${current.skipped} rejected=${current.rejected}\n`,
      'utf8'
    );

    if (delta >= targetDelta) {
      fs.appendFileSync(logPath, `TARGET_REACHED ts=${new Date().toISOString()}\n`, 'utf8');
      if (stopOnReach) {
        try {
          const out = execSync('powershell -ExecutionPolicy Bypass -File .\\start_system.ps1 -Stop', { encoding: 'utf8' });
          fs.writeFileSync(path.join(runRoot, 'stop_output.txt'), out || '', 'utf8');
          fs.appendFileSync(logPath, `SYSTEM_STOPPED ts=${new Date().toISOString()}\n`, 'utf8');
        } catch (e) {
          const msg = e?.stdout?.toString?.() || e?.message || String(e);
          fs.writeFileSync(path.join(runRoot, 'stop_output.txt'), msg, 'utf8');
          fs.appendFileSync(logPath, `SYSTEM_STOP_FAILED ts=${new Date().toISOString()}\n`, 'utf8');
          process.exitCode = 2;
        }
      }
      return;
    }
  }

  fs.appendFileSync(logPath, `TIMEOUT ts=${new Date().toISOString()}\n`, 'utf8');
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
