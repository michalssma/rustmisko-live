import fs from 'node:fs';
import path from 'node:path';

const ledgerPath = path.join(process.cwd(), 'data', 'ledger.jsonl');
if (!fs.existsSync(ledgerPath)) {
  console.error(`Ledger not found: ${ledgerPath}`);
  process.exit(1);
}

const lines = fs.readFileSync(ledgerPath, 'utf8').split(/\r?\n/).filter(Boolean);

let anomalyPlaced = 0;
let anomalyFailed = 0;
let anomalyWon = 0;
let anomalyLost = 0;
let anomalyCanceled = 0;

for (const line of lines) {
  let row;
  try { row = JSON.parse(line); } catch { continue; }

  const ev = row.event || row.type || '';
  const p = (row.path || '').toString().toLowerCase();

  if (p === 'anomaly_odds') {
    if (ev === 'PLACED') anomalyPlaced += 1;
    if (ev === 'BET_FAILED') anomalyFailed += 1;
  }

  // Settlement events for anomaly path are typically tagged as path B
  if (p === 'b') {
    if (ev === 'WON') anomalyWon += 1;
    if (ev === 'LOST') anomalyLost += 1;
    if (ev === 'CANCELED') anomalyCanceled += 1;
  }
}

const attempts = anomalyPlaced + anomalyFailed;
const placementPrecision = attempts > 0 ? anomalyPlaced / attempts : 0;

const settled = anomalyWon + anomalyLost + anomalyCanceled;
const settlementPrecision = settled > 0 ? anomalyWon / settled : 0;

const out = {
  ts: new Date().toISOString(),
  metric: 'anomaly_precision',
  formulas: {
    placement_precision_proxy: 'anomaly_odds PLACED / (anomaly_odds PLACED + anomaly_odds BET_FAILED)',
    settlement_precision_proxy: 'B WON / (B WON + B LOST + B CANCELED)',
  },
  values: {
    anomaly_placed: anomalyPlaced,
    anomaly_bet_failed: anomalyFailed,
    anomaly_attempts: attempts,
    placement_precision_proxy: Number((placementPrecision * 100).toFixed(2)),
    path_b_won: anomalyWon,
    path_b_lost: anomalyLost,
    path_b_canceled: anomalyCanceled,
    path_b_settled: settled,
    settlement_precision_proxy: Number((settlementPrecision * 100).toFixed(2)),
  }
};

console.log(JSON.stringify(out, null, 2));
