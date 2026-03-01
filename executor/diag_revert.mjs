import { createPublicClient, http, fallback, parseAbi, encodeFunctionData, decodeErrorResult } from 'viem';
import { polygon } from 'viem/chains';

const pc = createPublicClient({
  chain: polygon,
  transport: fallback(
    ['https://polygon-bor-rpc.publicnode.com','https://1rpc.io/matic','https://polygon.drpc.org'].map(u => http(u)),
    { rank: true }
  ),
});

const lp = '0x0FA7FB5407eA971694652E6E16C12A52625DE1b8';
const core = '0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7';

const abi = parseAbi([
  'function viewPayout(address core, uint256 tokenId) view returns (uint128)',
]);

// Known Azuro error selectors
const ERROR_SELECTORS = {
  '0x1b3c4eff': 'ConditionNotFinished',
  '0x646cf558': 'AlreadyPaid', 
  '0x5a4db784': 'BetNotExists',
  '0xb65f3a6a': 'CoreNotActive',
  '0x7b5369ad': 'LockedBetToken',
  '0x08c379a0': 'Error(string)',     // standard revert
  '0x4e487b71': 'Panic(uint256)',    // Panic
};

// Won bets from toolkit
const wonTokenIds = [221668, 221812, 221876, 221890];
// Canceled bets
const canceledTokenIds = [221711, 221833, 221837, 221849];
// Lost (should return 0)
const lostTokenIds = [221856, 221860];

async function checkPayout(label, tid) {
  try {
    const p = await pc.readContract({
      address: lp, abi,
      functionName: 'viewPayout',
      args: [core, BigInt(tid)],
    });
    console.log(`${label} tid=${tid} => PAYOUT=${(Number(p)/1e6).toFixed(4)} USDT`);
    return;
  } catch (e) {
    // Deep dig for raw revert data
    let rawHex = null;
    
    // Method 1: direct data
    if (e?.cause?.data?.data) rawHex = e.cause.data.data;
    else if (typeof e?.cause?.data === 'string') rawHex = e.cause.data;
    else if (e?.cause?.cause?.data?.data) rawHex = e.cause.cause.data.data;
    else if (typeof e?.cause?.cause?.data === 'string') rawHex = e.cause.cause.data;
    
    // Method 2: walk the error chain
    let current = e;
    while (current && !rawHex) {
      if (current.data && typeof current.data === 'string' && current.data.startsWith('0x')) {
        rawHex = current.data;
      }
      if (current.data?.data && typeof current.data.data === 'string') {
        rawHex = current.data.data;
      }
      current = current.cause;
    }

    // Method 3: try eth_call directly to get raw error
    if (!rawHex) {
      try {
        const callData = encodeFunctionData({ abi, functionName: 'viewPayout', args: [core, BigInt(tid)] });
        const result = await pc.call({ to: lp, data: callData });
        console.log(`${label} tid=${tid} => call OK: ${result.data}`);
        return;
      } catch (callErr) {
        // dig into call error
        let c = callErr;
        while (c) {
          if (c.data?.data && typeof c.data.data === 'string') { rawHex = c.data.data; break; }
          if (typeof c.data === 'string' && c.data.startsWith('0x')) { rawHex = c.data; break; }
          c = c.cause;
        }
        if (!rawHex) {
          // Last resort: look at message for hex  
          const msg = String(callErr?.cause?.cause?.message || callErr?.cause?.message || callErr?.message || '');
          const hexMatch = msg.match(/(0x[0-9a-fA-F]{8,})/);
          if (hexMatch) rawHex = hexMatch[1];
        }
      }
    }

    const selector = rawHex ? rawHex.slice(0, 10) : 'NO_SELECTOR';
    const errorName = ERROR_SELECTORS[selector] || 'UNKNOWN';
    
    // Try to decode Error(string)
    let decodedMsg = '';
    if (selector === '0x08c379a0' && rawHex) {
      try {
        const strAbi = parseAbi(['error Error(string)']);
        const decoded = decodeErrorResult({ abi: strAbi, data: rawHex });
        decodedMsg = ` msg="${decoded.args[0]}"`;
      } catch {}
    }
    
    console.log(`${label} tid=${tid} => REVERT selector=${selector} name=${errorName}${decodedMsg}`);
    if (rawHex) console.log(`  raw: ${rawHex.slice(0, 130)}`);
    else {
      // Print full error structure for debugging
      console.log(`  err.shortMessage: ${e?.shortMessage?.slice(0,150)}`);
      console.log(`  err.cause.data: ${JSON.stringify(e?.cause?.data)?.slice(0,150)}`);
    }
  }
}

console.log('=== WON BETS (should be claimable) ===');
for (const tid of wonTokenIds) await checkPayout('WON', tid);

console.log('\n=== CANCELED BETS (should return stake) ===');
for (const tid of canceledTokenIds) await checkPayout('CANCEL', tid);

console.log('\n=== LOST BETS (should return 0) ===');
for (const tid of lostTokenIds) await checkPayout('LOST', tid);
