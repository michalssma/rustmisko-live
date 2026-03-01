// Check if AlreadyPaid bets still have NFTs - verify on-chain state
import { createPublicClient, http, fallback, parseAbi, formatUnits } from 'viem';
import { polygon } from 'viem/chains';

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
const core = '0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7';
const usdt = '0xc2132D05D31c914a87C6611C10748AEb04B58e8F';

const erc721Abi = parseAbi([
  'function balanceOf(address) view returns (uint256)',
  'function ownerOf(uint256) view returns (address)',
  'function tokenOfOwnerByIndex(address,uint256) view returns (uint256)',
]);

const lpAbi = parseAbi([
  'function viewPayout(address,uint256) view returns (uint128)',
]);

const erc20Abi = parseAbi([
  'function balanceOf(address) view returns (uint256)',
]);

// AlreadyPaid bets (from previous diagnostic)
const alreadyPaidTokenIds = [221668, 221812, 221876, 221890, 221711, 221833];
// Lost bets (viewPayout returns 0)
const lostTokenIds = [221837, 221849, 221856, 221860];

console.log('=== CHECKING OWNERSHIP OF ALREADYPAID BETS ===');
for (const tid of alreadyPaidTokenIds) {
  try {
    const owner = await pc.readContract({
      address: azuroBet, abi: erc721Abi,
      functionName: 'ownerOf', args: [BigInt(tid)],
    });
    console.log(`tid=${tid} owner=${owner} ${owner.toLowerCase() === wallet.toLowerCase() ? 'OUR_WALLET' : 'OTHER'}`);
  } catch (e) {
    console.log(`tid=${tid} ownerOf REVERTED (burned?) => ${e?.shortMessage?.slice(0,100)}`);
  }
}

console.log('\n=== CURRENT WALLET STATE ===');
const nftCount = Number(await pc.readContract({
  address: azuroBet, abi: erc721Abi,
  functionName: 'balanceOf', args: [wallet],
}));
console.log(`NFT count: ${nftCount}`);

const usdtBal = await pc.readContract({
  address: usdt, abi: erc20Abi,
  functionName: 'balanceOf', args: [wallet],
});
console.log(`USDT balance: ${formatUnits(usdtBal, 6)}`);

// Check 5 newest tokenIds
console.log('\n=== NEWEST 5 TOKENIDS ===');
for (let i = Math.max(0, nftCount - 5); i < nftCount; i++) {
  const tid = await pc.readContract({
    address: azuroBet, abi: erc721Abi,
    functionName: 'tokenOfOwnerByIndex', args: [wallet, BigInt(i)],
  });
  // Check payout
  let payoutStr = '';
  try {
    const p = await pc.readContract({
      address: lp, abi: lpAbi,
      functionName: 'viewPayout', args: [core, tid],
    });
    payoutStr = `payout=${(Number(p)/1e6).toFixed(4)}`;
  } catch (e) {
    const msg = e?.shortMessage || '';
    if (msg.includes('0xd70a0e30')) payoutStr = 'AlreadyPaid';
    else payoutStr = `revert=${msg.slice(0,80)}`;
  }
  console.log(`  idx=${i} tid=${tid} ${payoutStr}`);
}

// Check oldest 5 tokenIds
console.log('\n=== OLDEST 5 TOKENIDS ===');
for (let i = 0; i < Math.min(5, nftCount); i++) {
  const tid = await pc.readContract({
    address: azuroBet, abi: erc721Abi,
    functionName: 'tokenOfOwnerByIndex', args: [wallet, BigInt(i)],
  });
  let payoutStr = '';
  try {
    const p = await pc.readContract({
      address: lp, abi: lpAbi,
      functionName: 'viewPayout', args: [core, tid],
    });
    payoutStr = `payout=${(Number(p)/1e6).toFixed(4)}`;
  } catch (e) {
    const msg = e?.shortMessage || '';
    if (msg.includes('0xd70a0e30')) payoutStr = 'AlreadyPaid';
    else payoutStr = `revert=${msg.slice(0,80)}`;
  }
  console.log(`  idx=${i} tid=${tid} ${payoutStr}`);
}

// FULL SCAN: categorize ALL NFTs
console.log('\n=== FULL SCAN: CATEGORIZING ALL ' + nftCount + ' NFTs ===');
let claimable = 0, claimableUsd = 0;
let lost = 0;
let alreadyPaid = 0;
let otherRevert = 0;
const claimableList = [];

for (let i = 0; i < nftCount; i += 10) {
  const batch = [];
  for (let j = i; j < Math.min(i + 10, nftCount); j++) {
    batch.push(pc.readContract({
      address: azuroBet, abi: erc721Abi,
      functionName: 'tokenOfOwnerByIndex', args: [wallet, BigInt(j)],
    }));
  }
  const tids = await Promise.all(batch);
  
  const results = await Promise.all(tids.map(tid =>
    pc.readContract({
      address: lp, abi: lpAbi,
      functionName: 'viewPayout', args: [core, tid],
    }).then(p => {
      const usd = Number(p) / 1e6;
      return { tid: tid.toString(), usd, status: usd > 0 ? 'claimable' : 'lost' };
    }).catch(e => {
      const msg = e?.shortMessage || '';
      if (msg.includes('0xd70a0e30')) return { tid: tid.toString(), usd: 0, status: 'already_paid' };
      return { tid: tid.toString(), usd: 0, status: 'other_revert', msg: msg.slice(0,60) };
    })
  ));
  
  for (const r of results) {
    if (r.status === 'claimable') { claimable++; claimableUsd += r.usd; claimableList.push(r); }
    else if (r.status === 'lost') lost++;
    else if (r.status === 'already_paid') alreadyPaid++;
    else { otherRevert++; console.log(`  OTHER_REVERT tid=${r.tid}: ${r.msg}`); }
  }
}

console.log(`\n=== SUMMARY ===`);
console.log(`Total NFTs: ${nftCount}`);
console.log(`Claimable:  ${claimable} ($${claimableUsd.toFixed(2)})`);
console.log(`Lost:       ${lost}`);
console.log(`AlreadyPaid: ${alreadyPaid}`);
console.log(`OtherRevert: ${otherRevert}`);
if (claimableList.length > 0) {
  console.log(`\n=== CLAIMABLE BETS ===`);
  for (const c of claimableList) {
    console.log(`  tid=${c.tid} $${c.usd.toFixed(4)}`);
  }
}
