// FULL investigation script:
// 1) Categorize ALL 119 NFTs (AlreadyPaid vs Lost vs Claimable vs OtherRevert)
// 2) For AlreadyPaid bets, search for BettorWin events (proof of withdrawal)
// 3) Log exact core/lp/tokenId triplet for each

import { createPublicClient, http, fallback, parseAbi, formatUnits, keccak256, toBytes, decodeEventLog } from 'viem';
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
const azuroBet = '0x7A1c3FEf712753374C4DCe34254B96faF2B7265B';
const lp = '0x0FA7FB5407eA971694652E6E16C12A52625DE1b8';
const core = '0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7';  // ClientCore V3
const usdt = '0xc2132D05D31c914a87C6611C10748AEb04B58e8F';

const erc721Abi = parseAbi([
  'function balanceOf(address) view returns (uint256)',
  'function tokenOfOwnerByIndex(address,uint256) view returns (uint256)',
  'function ownerOf(uint256) view returns (address)',
]);

const lpAbi = parseAbi([
  'function viewPayout(address,uint256) view returns (uint128)',
]);

const erc20Abi = parseAbi([
  'function balanceOf(address) view returns (uint256)',
]);

const output = [];
function log(msg) { output.push(msg); console.log(msg); }

// ========== STEP 1: Get current wallet state ==========
const nftCount = Number(await pc.readContract({
  address: azuroBet, abi: erc721Abi,
  functionName: 'balanceOf', args: [wallet],
}));
const usdtBal = await pc.readContract({
  address: usdt, abi: erc20Abi,
  functionName: 'balanceOf', args: [wallet],
});
log(`WALLET_STATE: nfts=${nftCount} usdt=${formatUnits(usdtBal, 6)}`);
log(`CONTRACTS: lp=${lp} core=${core} azuroBet=${azuroBet}`);
log(`CALL: viewPayout(core=${core}, tokenId=X) on LP=${lp}`);
log('');

// ========== STEP 2: Enumerate ALL tokenIds ==========
const allTokenIds = [];
for (let i = 0; i < nftCount; i += 10) {
  const batch = [];
  for (let j = i; j < Math.min(i + 10, nftCount); j++) {
    batch.push(pc.readContract({
      address: azuroBet, abi: erc721Abi,
      functionName: 'tokenOfOwnerByIndex', args: [wallet, BigInt(j)],
    }));
  }
  allTokenIds.push(...(await Promise.all(batch)));
}
log(`ENUMERATED: ${allTokenIds.length} tokenIds`);
log(`RANGE: min=${allTokenIds[0]} max=${allTokenIds[allTokenIds.length-1]}`);
log('');

// ========== STEP 3: Check viewPayout for each ==========
const categories = { claimable: [], lost: [], already_paid: [], other_revert: [] };

for (let i = 0; i < allTokenIds.length; i += 10) {
  const batch = allTokenIds.slice(i, i + 10);
  const results = await Promise.all(batch.map(tid =>
    pc.readContract({
      address: lp, abi: lpAbi,
      functionName: 'viewPayout', args: [core, tid],
    }).then(p => {
      const usd = Number(p) / 1e6;
      return { tid: tid.toString(), usd, status: usd > 0 ? 'claimable' : 'lost' };
    }).catch(e => {
      const msg = e?.shortMessage || e?.message || '';
      if (msg.includes('0xd70a0e30')) return { tid: tid.toString(), usd: 0, status: 'already_paid' };
      // Extract selector from message
      const selMatch = msg.match(/0x[0-9a-f]{8}/i);
      return { tid: tid.toString(), usd: 0, status: 'other_revert', selector: selMatch?.[0] || 'unknown', msg: msg.slice(0, 100) };
    })
  ));
  for (const r of results) {
    categories[r.status].push(r);
  }
}

log('=== FULL NFT SCAN RESULTS ===');
log(`Total:       ${allTokenIds.length}`);
log(`Claimable:   ${categories.claimable.length} (${categories.claimable.reduce((s,r) => s+r.usd, 0).toFixed(2)} USDT)`);
log(`Lost:        ${categories.lost.length}`);
log(`AlreadyPaid: ${categories.already_paid.length}`);
log(`OtherRevert: ${categories.other_revert.length}`);
log('');

if (categories.claimable.length > 0) {
  log('=== CLAIMABLE BETS ===');
  for (const c of categories.claimable) log(`  tid=${c.tid} $${c.usd.toFixed(4)}`);
  log('');
}

if (categories.other_revert.length > 0) {
  log('=== OTHER REVERTS ===');
  for (const c of categories.other_revert) log(`  tid=${c.tid} selector=${c.selector} msg=${c.msg}`);
  log('');
}

// ========== STEP 4: Sample AlreadyPaid bets — check ownerOf ==========
log('=== ALREADYPAID SAMPLE — OWNERSHIP CHECK ===');
const sampleAP = categories.already_paid.slice(0, 5);
for (const ap of sampleAP) {
  try {
    const owner = await pc.readContract({
      address: azuroBet, abi: erc721Abi,
      functionName: 'ownerOf', args: [BigInt(ap.tid)],
    });
    const isOurs = owner.toLowerCase() === wallet.toLowerCase();
    log(`  tid=${ap.tid} owner=${owner} ${isOurs ? 'OUR_WALLET' : 'NOT_OURS'}`);
  } catch (e) {
    log(`  tid=${ap.tid} ownerOf REVERTED (burned?): ${e?.shortMessage?.slice(0,80)}`);
  }
}
log('');

// ========== STEP 5: Search for BettorWin events from LP for our wallet ==========
// BettorWin(address indexed core, address indexed bettor, uint256 tokenId, uint256 amount)
log('=== SEARCHING FOR BettorWin EVENTS (last 50000 blocks) ===');
const bettorWinAbi = parseAbi([
  'event BettorWin(address indexed core, address indexed bettor, uint256 tokenId, uint256 amount)',
]);
const currentBlock = await pc.getBlockNumber();
log(`Current block: ${currentBlock}`);

// Search last 50000 blocks (~28 hours at 2s/block for Polygon)
const fromBlock = currentBlock - 50000n;
try {
  const logs = await pc.getLogs({
    address: lp,
    event: bettorWinAbi[0],
    args: { bettor: wallet },
    fromBlock,
    toBlock: currentBlock,
  });
  log(`BettorWin events found: ${logs.length}`);
  for (const l of logs) {
    const { core: evCore, bettor, tokenId, amount } = l.args;
    const usd = Number(amount) / 1e6;
    log(`  block=${l.blockNumber} tx=${l.transactionHash.slice(0,18)}... core=${evCore} tokenId=${tokenId} amount=$${usd.toFixed(2)}`);
  }
} catch (e) {
  log(`BettorWin getLogs error: ${e?.shortMessage?.slice(0,200) || e?.message?.slice(0,200)}`);
  // Try smaller range
  try {
    log('Trying smaller range (10000 blocks)...');
    const logs = await pc.getLogs({
      address: lp,
      event: bettorWinAbi[0],
      args: { bettor: wallet },
      fromBlock: currentBlock - 10000n,
      toBlock: currentBlock,
    });
    log(`BettorWin events (10k blocks): ${logs.length}`);
    for (const l of logs) {
      const { core: evCore, tokenId, amount } = l.args;
      const usd = Number(amount) / 1e6;
      log(`  block=${l.blockNumber} tx=${l.transactionHash.slice(0,18)}... core=${evCore} tokenId=${tokenId} amount=$${usd.toFixed(2)}`);
    }
  } catch (e2) {
    log(`Smaller range also failed: ${e2?.message?.slice(0,150)}`);
  }
}
log('');

// ========== STEP 6: Search for USDT Transfer events TO our wallet (last 10k blocks) ==========
log('=== USDT TRANSFERS TO OUR WALLET (last 10000 blocks) ===');
const transferAbi = parseAbi([
  'event Transfer(address indexed from, address indexed to, uint256 value)',
]);
try {
  const transfers = await pc.getLogs({
    address: usdt,
    event: transferAbi[0],
    args: { to: wallet },
    fromBlock: currentBlock - 10000n,
    toBlock: currentBlock,
  });
  log(`USDT Transfer(to=wallet) events: ${transfers.length}`);
  // Filter only from LP (claim payouts would come from LP)
  const fromLP = transfers.filter(t => t.args.from.toLowerCase() === lp.toLowerCase());
  log(`  From LP (${lp}): ${fromLP.length}`);
  for (const t of fromLP) {
    const usd = Number(t.args.value) / 1e6;
    log(`    block=${t.blockNumber} tx=${t.transactionHash.slice(0,18)}... $${usd.toFixed(2)}`);
  }
  // Filter from Relayer (bet placement refunds / relayer)  
  const fromRelayer = transfers.filter(t => t.args.from.toLowerCase() === '0x8dA05c0021e6b35865FDC959c54dCeF3A4AbBa9d'.toLowerCase());
  log(`  From Relayer: ${fromRelayer.length}`);
  for (const t of fromRelayer) {
    const usd = Number(t.args.value) / 1e6;
    log(`    block=${t.blockNumber} tx=${t.transactionHash.slice(0,18)}... $${usd.toFixed(2)}`);
  }
} catch (e) {
  log(`USDT Transfer getLogs error: ${e?.message?.slice(0,200)}`);
}

// ========== STEP 7: 3 detailed AlreadyPaid cases for GPT ==========
log('');
log('=== 3 DETAILED ALREADYPAID CASES FOR GPT ===');
const detailedTokenIds = categories.already_paid.slice(0, 3).map(x => x.tid);
for (const tidStr of detailedTokenIds) {
  log(`--- tokenId=${tidStr} ---`);
  log(`  call: LP(${lp}).viewPayout(core=${core}, tokenId=${tidStr})`);
  log(`  result: REVERT selector=0xd70a0e30 (AlreadyPaid())`);
  
  // Check ownerOf
  try {
    const owner = await pc.readContract({
      address: azuroBet, abi: erc721Abi,
      functionName: 'ownerOf', args: [BigInt(tidStr)],
    });
    log(`  ownerOf(${tidStr}): ${owner}`);
  } catch (e) {
    log(`  ownerOf(${tidStr}): REVERTED — ${e?.shortMessage?.slice(0,80)}`);
  }
}

// Write to file
fs.writeFileSync('diag_output.txt', output.join('\n'));
log('\nDone. Output saved to diag_output.txt');
