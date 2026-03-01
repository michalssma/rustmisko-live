// Focused: Get who initiated each BettorWin event, and cross-reference with AlreadyPaid
import { createPublicClient, http, fallback, parseAbi, formatUnits, decodeEventLog } from 'viem';
import { polygon } from 'viem/chains';
import fs from 'fs';

const pc = createPublicClient({
  chain: polygon,
  transport: fallback(
    ['https://polygon-bor-rpc.publicnode.com','https://1rpc.io/matic','https://polygon.drpc.org'].map(u => http(u)),
    { rank: true }
  ),
});

const wallet = '0x8226D38e5c69c2f0a77FBa80e466082B410a8F00'.toLowerCase();
const lp = '0x0FA7FB5407eA971694652E6E16C12A52625DE1b8';
const core = '0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7';
const azuroBet = '0x7A1c3FEf712753374C4DCe34254B96faF2B7265B';

const lpAbi = parseAbi([
  'event BettorWin(address indexed core, address indexed bettor, uint256 indexed tokenId, uint256 amount)',
]);
const lpViewAbi = parseAbi([
  'function viewPayout(address core, uint256 tokenId) view returns (uint256)',
]);
const erc721Abi = parseAbi([
  'function balanceOf(address) view returns (uint256)',
  'function tokenOfOwnerByIndex(address,uint256) view returns (uint256)',
]);

const out = [];
function log(msg) { console.log(msg); out.push(msg); }

async function main() {
  const currentBlock = await pc.getBlockNumber();
  log(`Current block: ${currentBlock}`);
  
  // Step 1: Get ALL BettorWin events (500k blocks ~ 11+ days)
  log('\n=== STEP 1: ALL BettorWin Events ===');
  const SEARCH_BLOCKS = 500000n;
  const CHUNK_SIZE = 10000n;
  const startBlock = currentBlock - SEARCH_BLOCKS;
  const allWins = [];
  
  for (let from = startBlock; from < currentBlock; from += CHUNK_SIZE) {
    const to = from + CHUNK_SIZE - 1n > currentBlock ? currentBlock : from + CHUNK_SIZE - 1n;
    try {
      const wins = await pc.getLogs({
        address: lp,
        event: lpAbi[0],
        args: { bettor: wallet },
        fromBlock: from,
        toBlock: to,
      });
      for (const w of wins) {
        allWins.push({
          blockNumber: Number(w.blockNumber),
          txHash: w.transactionHash,
          tokenId: Number(w.args.tokenId),
          amount: Number(formatUnits(w.args.amount, 6)),
          core: w.args.core,
        });
      }
    } catch (e) {
      // Retry smaller
      const SMALL = 2000n;
      for (let sf = from; sf <= to; sf += SMALL) {
        const st = sf + SMALL - 1n > to ? to : sf + SMALL - 1n;
        try {
          const wins = await pc.getLogs({ address: lp, event: lpAbi[0], args: { bettor: wallet }, fromBlock: sf, toBlock: st });
          for (const w of wins) {
            allWins.push({ blockNumber: Number(w.blockNumber), txHash: w.transactionHash, tokenId: Number(w.args.tokenId), amount: Number(formatUnits(w.args.amount, 6)), core: w.args.core });
          }
        } catch {}
      }
    }
  }
  
  allWins.sort((a, b) => a.blockNumber - b.blockNumber);
  log(`Found ${allWins.length} BettorWin events`);
  let totalWon = 0;
  for (const w of allWins) {
    totalWon += w.amount;
    log(`  tokenId=${w.tokenId} $${w.amount.toFixed(2)} block=${w.blockNumber} tx=${w.txHash.slice(0,14)}...`);
  }
  log(`TOTAL WON: $${totalWon.toFixed(2)}`);
  
  // Step 2: Who initiated each withdrawal TX?
  log('\n=== STEP 2: TX Initiators ===');
  const uniqueTxs = [...new Set(allWins.map(w => w.txHash))];
  let ourCount = 0, externalCount = 0;
  
  for (const hash of uniqueTxs) {
    try {
      const tx = await pc.getTransaction({ hash });
      const sender = tx.from.toLowerCase();
      const isOurs = sender === wallet;
      if (isOurs) ourCount++; else externalCount++;
      const winsInTx = allWins.filter(w => w.txHash === hash);
      const tokenIds = winsInTx.map(w => w.tokenId).join(',');
      log(`  ${hash.slice(0,14)}... sender=${tx.from} ${isOurs ? 'OUR_WALLET' : 'EXTERNAL!'} method=${tx.input.slice(0,10)} tokenIds=[${tokenIds}]`);
    } catch (e) {
      log(`  ${hash.slice(0,14)}... ERROR: ${e.message?.slice(0,50)}`);
    }
  }
  log(`Our wallet initiated: ${ourCount}/${uniqueTxs.length} TXs`);
  log(`External initiated: ${externalCount}/${uniqueTxs.length} TXs`);
  
  // Step 3: Current NFT categorization & cross-reference
  log('\n=== STEP 3: NFT Status & Cross-Reference ===');
  const balance = await pc.readContract({ address: azuroBet, abi: erc721Abi, functionName: 'balanceOf', args: [wallet] });
  log(`Total NFTs owned: ${balance}`);
  
  const tokenIds = [];
  for (let i = 0; i < Number(balance); i++) {
    const tid = await pc.readContract({ address: azuroBet, abi: erc721Abi, functionName: 'tokenOfOwnerByIndex', args: [wallet, BigInt(i)] });
    tokenIds.push(Number(tid));
  }
  tokenIds.sort((a, b) => a - b);
  
  const cats = { claimable: [], lost: [], alreadyPaid: [], otherRevert: [] };
  for (const tid of tokenIds) {
    try {
      const payout = await pc.readContract({ address: lp, abi: lpViewAbi, functionName: 'viewPayout', args: [core, BigInt(tid)] });
      if (payout === 0n) cats.lost.push(tid);
      else cats.claimable.push({ tid, payout: Number(formatUnits(payout, 6)) });
    } catch (e) {
      const msg = e.message || '';
      if (msg.includes('d70a0e30') || msg.includes('AlreadyPaid')) cats.alreadyPaid.push(tid);
      else cats.otherRevert.push(tid);
    }
  }
  
  log(`Claimable: ${cats.claimable.length}`);
  for (const c of cats.claimable) log(`  tokenId=${c.tid} payout=$${c.payout.toFixed(2)}`);
  log(`Lost (payout=0): ${cats.lost.length}`);
  log(`AlreadyPaid: ${cats.alreadyPaid.length}`);
  log(`OtherRevert: ${cats.otherRevert.length}`);
  
  // Cross-reference
  const winTokenSet = new Set(allWins.map(w => w.tokenId));
  const paidWithProof = cats.alreadyPaid.filter(t => winTokenSet.has(t));
  const paidNoProof = cats.alreadyPaid.filter(t => !winTokenSet.has(t));
  
  log(`\nAlreadyPaid WITH BettorWin event: ${paidWithProof.length}/${cats.alreadyPaid.length}`);
  log(`AlreadyPaid WITHOUT BettorWin event: ${paidNoProof.length}`);
  if (paidNoProof.length > 0) {
    log(`Missing proof tokenIds: ${paidNoProof.join(', ')}`);
  }
  
  // BettorWin events for tokenIds NOT in our NFT list (already burned?)
  const ownedSet = new Set(tokenIds);
  const winsNotOwned = allWins.filter(w => !ownedSet.has(w.tokenId));
  log(`\nBettorWin for tokenIds we DON'T own: ${winsNotOwned.length}`);
  for (const w of winsNotOwned) {
    log(`  tokenId=${w.tokenId} $${w.amount.toFixed(2)}`);
  }

  // Summary
  log('\n=== SUMMARY FOR GPT ===');
  log(`Total NFTs: ${tokenIds.length} (range: ${tokenIds[0]}-${tokenIds[tokenIds.length-1]})`);
  log(`  Claimable: ${cats.claimable.length}`);
  log(`  Lost: ${cats.lost.length}`);
  log(`  AlreadyPaid: ${cats.alreadyPaid.length}`);
  log(`  OtherRevert: ${cats.otherRevert.length}`);
  log(`BettorWin events found: ${allWins.length} (total: $${totalWon.toFixed(2)})`);
  log(`  Matched to AlreadyPaid: ${paidWithProof.length}`);
  log(`  Unmatched AlreadyPaid: ${paidNoProof.length}`);
  log(`TX initiators: ${ourCount} our wallet, ${externalCount} external`);
  log(`Core used in events: ${[...new Set(allWins.map(w=>w.core))].join(', ')}`);
  
  fs.writeFileSync('diag_summary.txt', out.join('\n'), 'utf-8');
  log('\nSaved to diag_summary.txt');
}

main().catch(e => { console.error('FATAL:', e); process.exit(1); });
