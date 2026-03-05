import { createPublicClient, http, fallback, parseAbi } from 'viem';
import { polygon } from 'viem/chains';

const pc = createPublicClient({
  chain: polygon,
  transport: fallback(
    ['https://polygon-bor-rpc.publicnode.com','https://1rpc.io/matic','https://polygon.drpc.org'].map(u => http(u)),
    { rank: true }
  ),
});

// Prematch Core
const coreAddr = '0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7';
const conditionAbi = parseAbi([
  'function conditions(uint256) view returns (uint256,uint48,uint48,uint16,uint8,address)'
]);

const cid = '300610060000000000291604170000000000001744833926';

console.log('=== Checking condition for bet #126 (heroic_vs_ww::map3_winner) ===');
console.log('conditionId:', cid);
console.log('');

// Check prematch core
try {
  const res = await pc.readContract({
    address: coreAddr, abi: conditionAbi, functionName: 'conditions', args: [BigInt(cid)]
  });
  const stateMap = { 0: 'Created (open)', 1: 'Resolved', 2: 'Canceled', 3: 'Paused' };
  console.log('[PREMATCH CORE] Result:');
  console.log('  reinforcement:', res[0].toString());
  console.log('  lastDepositId:', res[1].toString());
  console.log('  wonOutcomeId:', res[2].toString());
  console.log('  margin:', res[3].toString());
  console.log('  state:', res[4].toString(), '->', stateMap[Number(res[4])] || 'UNKNOWN');
  console.log('  oracle:', res[5]);
} catch(e) {
  console.log('[PREMATCH CORE] Error:', e.shortMessage || e.message);
}

// Also try with a minimal LP ABI to see if condition exists in LP
const lpAddr = '0x7043E4e1c4045424858ECBCED80989FeAfC11B36';
const lpAbi = parseAbi([
  'function getCondition(uint256) view returns (uint256,uint48,uint48,uint16,uint8,address)'
]);
try {
  const res2 = await pc.readContract({
    address: lpAddr, abi: lpAbi, functionName: 'getCondition', args: [BigInt(cid)]
  });
  console.log('[LP] getCondition state:', res2[4].toString());
} catch(e) {
  console.log('[LP] Error:', e.shortMessage || e.message);
}

// Decode the condition ID structure to understand what it encodes
// Azuro conditionId = ((leafId << 128) | (gameId << 64) | timestamp)
// Actually in new format: large integer
// Let's just log its hex representation
const cidBig = BigInt(cid);
console.log('');
console.log('=== Condition ID Analysis ===');
console.log('hex:', cidBig.toString(16));
// The timestamp embedded in last part: 1744833926 
const ts = 1744833926;
console.log('embedded timestamp:', ts, '->', new Date(ts * 1000).toISOString());
console.log('NOTE: timestamp in future =', ts > Math.floor(Date.now()/1000), '(match starts in future)');
