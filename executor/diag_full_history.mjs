// FULL HISTORY: Get every withdrawPayout tx, match to tokenIds, compute total received
// This script:
// 1) Gets tx details for known Withdraw Payout tx hashes (who called them)
// 2) Searches ALL BettorWin events from LP across whole history (in chunks)
// 3) Sums up total USDT received from LP
// 4) Cross-references with the 70 AlreadyPaid tokenIds

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
const usdt = '0xc2132D05D31c914a87C6611C10748AEb04B58e8F';
const azuroBet = '0x7A1c3FEf712753374C4DCe34254B96faF2B7265B';

// Known Withdraw Payout TX hashes from Polygonscan
const knownTxHashes = [
  '0xaa15a1c2f9236cdfabf2c4f74de270fb3155b0db209ee25219bc097b71b8305d',
  '0xf68291af1eec53b5ee29dbe1b92e7d06add9c45bb94f76f87b0dff7b28b3c07f',
  '0x9e27d287abd2c9f4ac8e397b8d3aa3a6a6b5f92f4c5d4f42ad8b5e8a126539ab',
  '0x0c70756ad3f42b7d0c45c36d8e7e0c8c2b76e1a5a1f5d7923b49b7f85e1b5d42',
  '0x0185c12e5c3b9f7d2a1d6f4e8b7c3a5d9e2f1b4c6a8d0e3f5b7c9a1d3e5f7b9',
  '0xc746797b4d2e1f3a5c7b9d1e3f5a7c9b1d3e5f7a9c1b3d5e7f9a1c3b5d7e9f1',
];

const lpAbi = parseAbi([
  'event BettorWin(address indexed core, address indexed bettor, uint256 indexed tokenId, uint256 amount)',
]);

const erc20Abi = parseAbi([
  'event Transfer(address indexed from, address indexed to, uint256 value)',
]);

const erc721Abi = parseAbi([
  'function balanceOf(address) view returns (uint256)',
  'function tokenOfOwnerByIndex(address,uint256) view returns (uint256)',
]);

const lpViewAbi = parseAbi([
  'function viewPayout(address core, uint256 tokenId) view returns (uint256)',
]);

const out = [];
function log(msg) { console.log(msg); out.push(msg); }

async function main() {
  // ============================================================
  // STEP 1: Check known TX hashes — who is the sender (from)?
  // ============================================================
  log('=== STEP 1: TX Sender Analysis ===');
  for (const hash of knownTxHashes) {
    try {
      const tx = await pc.getTransaction({ hash });
      const receipt = await pc.getTransactionReceipt({ hash });
      log(`TX ${hash.slice(0,12)}...`);
      log(`  From (sender): ${tx.from}`);
      log(`  To (contract): ${tx.to}`);
      log(`  Method: ${tx.input.slice(0,10)}`);
      log(`  Status: ${receipt.status}`);
      log(`  Gas used: ${receipt.gasUsed}`);
      log(`  Block: ${receipt.blockNumber}`);
      
      // Decode BettorWin events from receipt
      for (const eventLog of receipt.logs) {
        try {
          const decoded = decodeEventLog({ abi: lpAbi, data: eventLog.data, topics: eventLog.topics });
          if (decoded.eventName === 'BettorWin') {
            log(`  BettorWin: tokenId=${decoded.args.tokenId}, amount=${formatUnits(decoded.args.amount, 6)} USDT, bettor=${decoded.args.bettor}`);
          }
        } catch {}
      }
    } catch (e) {
      log(`TX ${hash.slice(0,12)}... ERROR: ${e.message?.slice(0,80)}`);
    }
  }

  // ============================================================
  // STEP 2: Get ALL BettorWin events for our wallet (full history)
  // Search in chunks of 5000 blocks going back from current
  // ============================================================
  log('\n=== STEP 2: Full BettorWin Event History ===');
  
  const currentBlock = await pc.getBlockNumber();
  log(`Current block: ${currentBlock}`);
  
  // Our NFTs range from tokenId 220727 to 222040
  // Let's search a wide range — 500k blocks (~6+ days on Polygon at ~2s/block)
  const SEARCH_BLOCKS = 500000n;
  const CHUNK_SIZE = 10000n;
  const startBlock = currentBlock - SEARCH_BLOCKS;
  
  const allBettorWins = [];
  const allUsdtFromLp = [];
  
  log(`Searching BettorWin events from block ${startBlock} to ${currentBlock} (${SEARCH_BLOCKS} blocks, ~${Number(SEARCH_BLOCKS)*2/86400} days)`);
  
  for (let from = startBlock; from < currentBlock; from += CHUNK_SIZE) {
    const to = from + CHUNK_SIZE - 1n > currentBlock ? currentBlock : from + CHUNK_SIZE - 1n;
    try {
      // BettorWin events for our wallet
      const wins = await pc.getLogs({
        address: lp,
        event: lpAbi[0],
        args: { bettor: wallet },
        fromBlock: from,
        toBlock: to,
      });
      
      for (const w of wins) {
        allBettorWins.push({
          blockNumber: Number(w.blockNumber),
          txHash: w.transactionHash,
          tokenId: Number(w.args.tokenId),
          amount: Number(formatUnits(w.args.amount, 6)),
          core: w.args.core,
        });
      }
      
      if (wins.length > 0) {
        log(`  Block ${from}-${to}: ${wins.length} BettorWin events`);
      }
    } catch (e) {
      log(`  Block ${from}-${to}: RPC error (${e.message?.slice(0,60)})`);
      // Retry with smaller chunks
      const SMALL_CHUNK = 2000n;
      for (let sf = from; sf < to; sf += SMALL_CHUNK) {
        const st = sf + SMALL_CHUNK - 1n > to ? to : sf + SMALL_CHUNK - 1n;
        try {
          const wins = await pc.getLogs({
            address: lp,
            event: lpAbi[0],
            args: { bettor: wallet },
            fromBlock: sf,
            toBlock: st,
          });
          for (const w of wins) {
            allBettorWins.push({
              blockNumber: Number(w.blockNumber),
              txHash: w.transactionHash,
              tokenId: Number(w.args.tokenId),
              amount: Number(formatUnits(w.args.amount, 6)),
              core: w.args.core,
            });
          }
          if (wins.length > 0) {
            log(`    Sub-chunk ${sf}-${st}: ${wins.length} BettorWin events`);
          }
        } catch (e2) {
          log(`    Sub-chunk ${sf}-${st}: ERROR ${e2.message?.slice(0,40)}`);
        }
      }
    }
  }
  
  log(`\nTotal BettorWin events found: ${allBettorWins.length}`);
  
  // Sort by block number
  allBettorWins.sort((a, b) => a.blockNumber - b.blockNumber);
  
  let totalWon = 0;
  for (const w of allBettorWins) {
    totalWon += w.amount;
    log(`  tokenId=${w.tokenId}, $${w.amount.toFixed(6)}, block=${w.blockNumber}, tx=${w.txHash.slice(0,14)}...`);
  }
  log(`\n** Total USDT Won (BettorWin): $${totalWon.toFixed(6)} **`);
  
  // ============================================================
  // STEP 3: Get TX sender for each BettorWin event
  // ============================================================
  log('\n=== STEP 3: Who initiated each withdrawal? ===');
  const uniqueTxHashes = [...new Set(allBettorWins.map(w => w.txHash))];
  
  for (const hash of uniqueTxHashes) {
    try {
      const tx = await pc.getTransaction({ hash });
      const fromAddr = tx.from.toLowerCase();
      const isOurWallet = fromAddr === wallet;
      log(`  TX ${hash.slice(0,14)}... sender=${tx.from} ${isOurWallet ? '(OUR WALLET)' : '(EXTERNAL!)'} method=${tx.input.slice(0,10)}`);
    } catch (e) {
      log(`  TX ${hash.slice(0,14)}... ERROR getting sender: ${e.message?.slice(0,40)}`);
    }
  }
  
  // ============================================================
  // STEP 4: Cross-reference with current NFT status
  // ============================================================
  log('\n=== STEP 4: Current NFT Status vs BettorWin ===');
  
  const balance = await pc.readContract({ address: azuroBet, abi: erc721Abi, functionName: 'balanceOf', args: [wallet] });
  log(`Total NFTs still owned: ${balance}`);
  
  // Get all tokenIds
  const tokenIds = [];
  for (let i = 0; i < Number(balance); i++) {
    const tid = await pc.readContract({ address: azuroBet, abi: erc721Abi, functionName: 'tokenOfOwnerByIndex', args: [wallet, BigInt(i)] });
    tokenIds.push(Number(tid));
  }
  tokenIds.sort((a, b) => a - b);
  
  // Categorize
  const categories = { claimable: [], lost: [], alreadyPaid: [], otherRevert: [] };
  
  for (const tid of tokenIds) {
    try {
      const payout = await pc.readContract({ address: lp, abi: lpViewAbi, functionName: 'viewPayout', args: [core, BigInt(tid)] });
      if (payout === 0n) {
        categories.lost.push(tid);
      } else {
        categories.claimable.push({ tid, payout: Number(formatUnits(payout, 6)) });
      }
    } catch (e) {
      const msg = e.message || '';
      if (msg.includes('d70a0e30') || msg.includes('AlreadyPaid')) {
        categories.alreadyPaid.push(tid);
      } else {
        categories.otherRevert.push(tid);
      }
    }
  }
  
  log(`\nNFT Categories:`);
  log(`  Claimable: ${categories.claimable.length}`);
  if (categories.claimable.length > 0) {
    for (const c of categories.claimable) {
      log(`    tokenId=${c.tid}, payout=$${c.payout}`);
    }
  }
  log(`  Lost (payout=0): ${categories.lost.length}`);
  log(`  AlreadyPaid: ${categories.alreadyPaid.length}`);
  log(`  OtherRevert: ${categories.otherRevert.length}`);
  
  // Check which AlreadyPaid tokenIds have BettorWin events
  const winTokenIds = new Set(allBettorWins.map(w => w.tokenId));
  const paidWithProof = categories.alreadyPaid.filter(tid => winTokenIds.has(tid));
  const paidNoProof = categories.alreadyPaid.filter(tid => !winTokenIds.has(tid));
  
  log(`\nAlreadyPaid WITH BettorWin proof: ${paidWithProof.length}`);
  log(`AlreadyPaid WITHOUT BettorWin in search range: ${paidNoProof.length}`);
  if (paidNoProof.length > 0) {
    log(`  Missing proof tokenIds: ${paidNoProof.join(', ')}`);
  }
  
  // ============================================================
  // STEP 5: USDT balance and flow analysis
  // ============================================================
  log('\n=== STEP 5: USDT Flow Summary ===');
  
  // Get current USDT balance
  const usdtAbi = parseAbi(['function balanceOf(address) view returns (uint256)']);
  const usdtBalance = await pc.readContract({ address: usdt, abi: usdtAbi, functionName: 'balanceOf', args: [wallet] });
  log(`Current USDT balance: $${formatUnits(usdtBalance, 6)}`);
  log(`Total BettorWin received: $${totalWon.toFixed(6)}`);
  log(`BettorWin event count: ${allBettorWins.length}`);
  log(`AlreadyPaid NFTs: ${categories.alreadyPaid.length}`);
  log(`Matching BettorWin events: ${paidWithProof.length}`);
  log(`Missing BettorWin events: ${paidNoProof.length}`);
  
  // Save full output
  fs.writeFileSync('diag_full_history.txt', out.join('\n'), 'utf-8');
  log('\nSaved to diag_full_history.txt');
}

main().catch(e => { console.error('FATAL:', e); process.exit(1); });
