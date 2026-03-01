// Search for ALL BettorWin events for our wallet in chunks of 3000 blocks
// covering last ~48 hours
import { createPublicClient, http, fallback, parseAbi, formatUnits } from 'viem';
import { polygon } from 'viem/chains';
import fs from 'fs';

const pc = createPublicClient({
  chain: polygon,
  transport: fallback(
    ['https://polygon-bor-rpc.publicnode.com','https://1rpc.io/matic','https://polygon.drpc.org'].map(u => http(u)),
    { rank: true }
  ),
});

const wallet = '0x8226D38e5c69c2f0a77FBa80e466082B410a8F00';
const lp = '0x0FA7FB5407eA971694652E6E16C12A52625DE1b8';
const usdt = '0xc2132D05D31c914a87C6611C10748AEb04B58e8F';

const bettorWinAbi = parseAbi([
  'event BettorWin(address indexed core, address indexed bettor, uint256 tokenId, uint256 amount)',
]);
const transferAbi = parseAbi([
  'event Transfer(address indexed from, address indexed to, uint256 value)',
]);

const currentBlock = await pc.getBlockNumber();
const CHUNK = 3000n;
// 48 hours = ~86400 blocks at 2s/block
const totalBlocks = 86400n;
const startBlock = currentBlock - totalBlocks;

const output = [];
function log(msg) { output.push(msg); console.log(msg); }

log(`Current block: ${currentBlock}`);
log(`Searching from block ${startBlock} to ${currentBlock} (${totalBlocks} blocks, ~48h)`);
log('');

// ========= BettorWin events =========
log('=== ALL BettorWin EVENTS ===');
const allWinEvents = [];
for (let from = startBlock; from < currentBlock; from += CHUNK) {
  const to = from + CHUNK > currentBlock ? currentBlock : from + CHUNK;
  try {
    const logs = await pc.getLogs({
      address: lp,
      event: bettorWinAbi[0],
      args: { bettor: wallet },
      fromBlock: from,
      toBlock: to,
    });
    allWinEvents.push(...logs);
  } catch (e) {
    log(`  chunk ${from}-${to} error: ${e?.message?.slice(0,80)}`);
  }
}

log(`Total BettorWin events found: ${allWinEvents.length}`);
let totalWinUsd = 0;
for (const l of allWinEvents) {
  const { core, tokenId, amount } = l.args;
  const usd = Number(amount) / 1e6;
  totalWinUsd += usd;
  log(`  block=${l.blockNumber} tx=${l.transactionHash} core=${core} tokenId=${tokenId} amount=$${usd.toFixed(2)}`);
}
log(`Total BettorWin amount: $${totalWinUsd.toFixed(2)}`);
log('');

// ========= USDT transfers from LP to our wallet =========
log('=== ALL USDT TRANSFERS FROM LP TO WALLET ===');
const allLPTransfers = [];
for (let from = startBlock; from < currentBlock; from += CHUNK) {
  const to = from + CHUNK > currentBlock ? currentBlock : from + CHUNK;
  try {
    const logs = await pc.getLogs({
      address: usdt,
      event: transferAbi[0],
      args: { from: lp, to: wallet },
      fromBlock: from,
      toBlock: to,
    });
    allLPTransfers.push(...logs);
  } catch (e) {
    // ignore
  }
}
log(`USDT transfers from LP: ${allLPTransfers.length}`);
let totalFromLP = 0;
for (const t of allLPTransfers) {
  const usd = Number(t.args.value) / 1e6;
  totalFromLP += usd;
  log(`  block=${t.blockNumber} tx=${t.transactionHash} $${usd.toFixed(2)}`);
}
log(`Total USDT from LP: $${totalFromLP.toFixed(2)}`);
log('');

// ========= ALL USDT transfers TO wallet (any sender) =========
log('=== ALL USDT TRANSFERS TO WALLET (any sender) ===');
const allInTransfers = [];
for (let from = startBlock; from < currentBlock; from += CHUNK) {
  const to = from + CHUNK > currentBlock ? currentBlock : from + CHUNK;
  try {
    const logs = await pc.getLogs({
      address: usdt,
      event: transferAbi[0],
      args: { to: wallet },
      fromBlock: from,
      toBlock: to,
    });
    allInTransfers.push(...logs);
  } catch (e) {
    // ignore
  }
}
log(`All USDT in-transfers: ${allInTransfers.length}`);
let totalIn = 0;
for (const t of allInTransfers) {
  const usd = Number(t.args.value) / 1e6;
  totalIn += usd;
  log(`  block=${t.blockNumber} from=${t.args.from} $${usd.toFixed(2)}`);
}
log(`Total USDT received (48h): $${totalIn.toFixed(2)}`);
log('');

// ========= ALL USDT transfers FROM wallet (any receiver) =========
log('=== ALL USDT TRANSFERS FROM WALLET (outgoing) ===');
const allOutTransfers = [];
for (let from = startBlock; from < currentBlock; from += CHUNK) {
  const to = from + CHUNK > currentBlock ? currentBlock : from + CHUNK;
  try {
    const logs = await pc.getLogs({
      address: usdt,
      event: transferAbi[0],
      args: { from: wallet },
      fromBlock: from,
      toBlock: to,
    });
    allOutTransfers.push(...logs);
  } catch (e) {
    // ignore
  }
}
log(`All USDT out-transfers: ${allOutTransfers.length}`);
let totalOut = 0;
for (const t of allOutTransfers) {
  const usd = Number(t.args.value) / 1e6;
  totalOut += usd;
  log(`  block=${t.blockNumber} to=${t.args.to} $${usd.toFixed(2)}`);
}
log(`Total USDT spent (48h): $${totalOut.toFixed(2)}`);
log('');
log(`NET FLOW (48h): +$${totalIn.toFixed(2)} - $${totalOut.toFixed(2)} = $${(totalIn-totalOut).toFixed(2)}`);

fs.writeFileSync('diag_events.txt', output.join('\n'));
