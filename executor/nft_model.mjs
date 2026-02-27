// On-chain NFT performance model (REAL DATA)
// - Enumerates all AzuroBet NFTs owned by WALLET
// - Classifies each bet as WON / LOST / CANCELED / PENDING using LiveCore
// - Joins to ../data/bet_history.txt by conditionId + closest timestamp
// - Aggregates PnL/ROI by sport, market (map_winner vs match_winner), and odds buckets

import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';
import { createPublicClient, fallback, http, parseAbi } from 'viem';
import { polygon } from 'viem/chains';

const t = await import('@azuro-org/toolkit');

const WALLET = (process.env.WALLET || '0x8226D38e5c69c2f0a77FBa80e466082B410a8F00').toLowerCase();
const NFT = (process.env.AZURO_NFT || '0x7A1c3FEf712753374C4DCe34254B96faF2B7265B');
const CORE = (process.env.AZURO_CORE || '0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7');

const rpcUrls = [
  process.env.RPC_URL || 'https://polygon-bor-rpc.publicnode.com',
  'https://polygon-rpc.com',
  'https://rpc.ankr.com/polygon',
  'https://polygon.drpc.org',
];

const client = createPublicClient({
  chain: polygon,
  transport: fallback(rpcUrls.map((u) => http(u)), { rank: true }),
  batch: { multicall: true },
});

const nftAbi = parseAbi([
  'function balanceOf(address) view returns (uint256)',
  'function tokenOfOwnerByIndex(address owner, uint256 index) view returns (uint256)',
]);

function parseBetHistory(filePath) {
  if (!fs.existsSync(filePath)) return { byCondition: new Map(), totalLines: 0 };
  const lines = fs.readFileSync(filePath, 'utf8').split(/\r?\n/).filter(Boolean);

  // Expected format per line:
  // sport::matchSlug|conditionId|team|odds|timestamp
  const byCondition = new Map();
  for (const line of lines) {
    const parts = line.split('|');
    if (parts.length < 5) continue;

    const slug = parts[0];
    const conditionId = parts[1];
    const team = parts[2];
    const odds = Number(parts[3]);
    const ts = Date.parse(parts[4]);

    const sport = slug.includes('::') ? slug.split('::')[0] : 'unknown';
    const market = slug.includes('::map') ? 'map_winner' : 'match_winner';

    const rec = { slug, sport, market, team, odds, ts: Number.isFinite(ts) ? ts : null, raw: line };
    if (!byCondition.has(conditionId)) byCondition.set(conditionId, []);
    byCondition.get(conditionId).push(rec);
  }

  for (const arr of byCondition.values()) {
    arr.sort((a, b) => (a.ts ?? 0) - (b.ts ?? 0));
  }

  return { byCondition, totalLines: lines.length };
}

function closestHistoryRec(recs, betTsMs) {
  if (!recs || recs.length === 0) return null;
  if (!Number.isFinite(betTsMs)) return recs[0];

  let best = recs[0];
  let bestDist = Math.abs((best.ts ?? 0) - betTsMs);
  for (const r of recs) {
    const dist = Math.abs((r.ts ?? 0) - betTsMs);
    if (dist < bestDist) {
      best = r;
      bestDist = dist;
    }
  }
  return best;
}

function oddsBucket(odds) {
  if (!Number.isFinite(odds)) return 'unknown';
  if (odds < 1.5) return '<1.5';
  if (odds < 2.0) return '1.5-2.0';
  if (odds < 3.0) return '2.0-3.0';
  return '>=3.0';
}

async function enumerateTokenIds() {
  const balance = await client.readContract({ address: NFT, abi: nftAbi, functionName: 'balanceOf', args: [WALLET] });
  const count = Number(balance);
  const tokenIds = [];

  for (let i = 0; i < count; i += 25) {
    const batch = [];
    for (let j = i; j < Math.min(i + 25, count); j++) batch.push(j);
    const ids = await Promise.all(
      batch.map((idx) => client.readContract({ address: NFT, abi: nftAbi, functionName: 'tokenOfOwnerByIndex', args: [WALLET, BigInt(idx)] }))
    );
    tokenIds.push(...ids.map((x) => Number(x)));
  }

  tokenIds.sort((a, b) => a - b);
  return tokenIds;
}

async function run() {
  const __filename = fileURLToPath(import.meta.url);
  const __dirname = path.dirname(__filename);
  const root = path.resolve(__dirname, '..');

  const betHistoryPath = path.join(root, 'data', 'bet_history.txt');
  const { byCondition, totalLines } = parseBetHistory(betHistoryPath);

  const tokenIds = await enumerateTokenIds();

  const rows = [];
  const conditionCache = new Map();

  const concurrency = 6;
  let cursor = 0;
  async function worker() {
    while (cursor < tokenIds.length) {
      const idx = cursor++;
      const tokenId = tokenIds[idx];

      try {
        const bet = await client.readContract({ address: CORE, abi: t.coreAbi, functionName: 'bets', args: [BigInt(tokenId)] });
        const conditionId = bet[0].toString();
        const amount = Number(bet[1]) / 1e6;
        const payout = Number(bet[2]) / 1e6;
        const outcomeId = bet[3];
        const timestampSec = Number(bet[4]);
        const isPaid = Boolean(bet[5]);

        let condition = conditionCache.get(conditionId);
        if (!condition) {
          const cond = await client.readContract({ address: CORE, abi: t.coreAbi, functionName: 'conditions', args: [BigInt(conditionId)] });
          condition = {
            settledAt: Number(cond[1]),
            winningOutcomesCount: Number(cond[3]),
            state: Number(cond[4]),
            oracle: cond[5],
          };
          conditionCache.set(conditionId, condition);
        }

        let result = 'UNKNOWN';
        let isWin = null;
        if (condition.state === 2) {
          result = 'CANCELED';
        } else if (condition.state === 1) {
          try {
            isWin = await client.readContract({
              address: CORE,
              abi: t.coreAbi,
              functionName: 'isOutcomeWinning',
              args: [BigInt(conditionId), outcomeId],
            });
            result = isWin ? 'WON' : 'LOST';
          } catch {
            result = 'RESOLVED_UNKNOWN';
          }
        } else if (condition.state === 0) {
          result = 'PENDING_CREATED';
        } else if (condition.state === 3) {
          result = 'PAUSED';
        }

        let returned = 0;
        if (result === 'WON') returned = payout;
        else if (result === 'CANCELED') returned = amount;
        else returned = 0;
        const net = returned - amount;

        const betTsMs = Number.isFinite(timestampSec) ? timestampSec * 1000 : null;
        const historyRecs = byCondition.get(conditionId);
        const h = closestHistoryRec(historyRecs, betTsMs);

        const sport = h?.sport ?? 'unknown';
        const market = h?.market ?? 'unknown';
        const odds = Number.isFinite(h?.odds) ? h.odds : (amount > 0 ? payout / amount : NaN);

        rows.push({
          tokenId,
          conditionId,
          amount,
          payout,
          outcomeId: Number(outcomeId),
          timestampSec,
          isPaid,
          conditionState: condition.state,
          oracle: condition.oracle,
          result,
          isWin: isWin === null ? null : Boolean(isWin),
          returned,
          net,
          sport,
          market,
          odds,
          oddsBucket: oddsBucket(odds),
          slug: h?.slug ?? null,
          team: h?.team ?? null,
        });
      } catch (e) {
        rows.push({ tokenId, error: e?.shortMessage ?? e?.message ?? String(e) });
      }
    }
  }

  await Promise.all(Array.from({ length: concurrency }, () => worker()));

  const totals = {
    n: rows.filter((r) => !r.error).length,
    errors: rows.filter((r) => r.error).length,
    wagered: 0,
    returned: 0,
    net: 0,
    won: 0,
    lost: 0,
    canceled: 0,
    pending: 0,
  };

  function addAgg(map, key, r) {
    if (!map.has(key)) map.set(key, { n: 0, wagered: 0, returned: 0, net: 0, won: 0, lost: 0, canceled: 0, pending: 0 });
    const a = map.get(key);
    a.n++;
    a.wagered += r.amount;
    a.returned += r.returned;
    a.net += r.net;
    if (r.result === 'WON') a.won++;
    else if (r.result === 'LOST') a.lost++;
    else if (r.result === 'CANCELED') a.canceled++;
    else if (String(r.result).startsWith('PENDING')) a.pending++;
  }

  const bySport = new Map();
  const byMarket = new Map();
  const byOdds = new Map();

  for (const r of rows) {
    if (r.error) continue;
    totals.wagered += r.amount;
    totals.returned += r.returned;
    totals.net += r.net;
    if (r.result === 'WON') totals.won++;
    else if (r.result === 'LOST') totals.lost++;
    else if (r.result === 'CANCELED') totals.canceled++;
    else if (String(r.result).startsWith('PENDING')) totals.pending++;

    addAgg(bySport, r.sport, r);
    addAgg(byMarket, r.market, r);
    addAgg(byOdds, r.oddsBucket, r);
  }

  function toSortedArr(map) {
    return [...map.entries()]
      .map(([k, v]) => ({ key: k, ...v, roi: v.wagered > 0 ? v.net / v.wagered : 0 }))
      .sort((a, b) => b.net - a.net);
  }

  const out = {
    meta: {
      wallet: WALLET,
      nft: NFT,
      core: CORE,
      betHistoryLines: totalLines,
      tokenCount: tokenIds.length,
      ts: new Date().toISOString(),
    },
    totals: { ...totals, roi: totals.wagered > 0 ? totals.net / totals.wagered : 0 },
    bySport: toSortedArr(bySport),
    byMarket: toSortedArr(byMarket),
    byOddsBucket: toSortedArr(byOdds),
    rows,
  };

  console.log('=== NFT REAL-DATA MODEL ===');
  console.log(`wallet=${out.meta.wallet}`);
  console.log(`NFTs=${out.meta.tokenCount}  bet_history_lines=${out.meta.betHistoryLines}  errors=${totals.errors}`);
  console.log('');
  console.log(`TOTAL wagered=$${out.totals.wagered.toFixed(2)} returned=$${out.totals.returned.toFixed(2)} net=$${out.totals.net.toFixed(2)} ROI=${(out.totals.roi * 100).toFixed(2)}%`);
  console.log(`RESULTS won=${out.totals.won} lost=${out.totals.lost} canceled=${out.totals.canceled} pending=${out.totals.pending}`);

  function printTop(title, arr, limit = 20) {
    console.log('');
    console.log(title);
    console.log('key                        n  won lost can  wagered   net     ROI');
    for (const x of arr.slice(0, limit)) {
      const key = String(x.key).padEnd(26);
      const n = String(x.n).padStart(3);
      const won = String(x.won).padStart(3);
      const lost = String(x.lost).padStart(4);
      const can = String(x.canceled).padStart(4);
      const w = ('$' + x.wagered.toFixed(2)).padStart(9);
      const net = ('$' + x.net.toFixed(2)).padStart(8);
      const roi = (x.roi * 100).toFixed(1).padStart(6) + '%';
      console.log(`${key} ${n} ${won} ${lost} ${can} ${w} ${net} ${roi}`);
    }
  }

  printTop('--- BY SPORT (sorted by net) ---', out.bySport, 30);
  printTop('--- BY MARKET (sorted by net) ---', out.byMarket, 10);
  printTop('--- BY ODDS BUCKET (sorted by net) ---', out.byOddsBucket, 10);

  const outPath = path.join(root, 'data', 'nft_model.json');
  fs.writeFileSync(outPath, JSON.stringify(out, null, 2), 'utf8');
  console.log('');
  console.log(`WROTE ${outPath}`);
}

await run();
