const { createPublicClient, createWalletClient, http } = require('viem');
const { polygon } = require('viem/chains');
const { privateKeyToAccount } = require('viem/accounts');
const tk = require('@azuro-org/toolkit');

const LP = '0x0FA7FB5407eA971694652E6E16C12A52625DE1b8';
const CORE = '0xf9548be470a4e130c90cea8b179fcd66d2972ac7';
const PRIVATE_KEY = '0x34fb468df8e14a223595b824c1515f0477d2f06b3f6509f25c2f9e9e02ce3f7c';
const RPC = 'https://polygon-bor-rpc.publicnode.com';

const account = privateKeyToAccount(PRIVATE_KEY);
const pc = createPublicClient({ chain: polygon, transport: http(RPC) });
const wc = createWalletClient({ account, chain: polygon, transport: http(RPC) });

const TOKEN_IDS = [220727, 220777, 220789, 220795, 220797];

async function main() {
  console.log('=== Checking payouts ===');
  let totalClaimable = 0;
  
  for (const id of TOKEN_IDS) {
    try {
      const p = await pc.readContract({
        address: LP,
        abi: tk.lpAbi,
        functionName: 'viewPayout',
        args: [CORE, BigInt(id)],
      });
      const usd = Number(p) / 1e6;
      console.log(`Token ${id}: ${usd.toFixed(6)} USDT`);
      totalClaimable += usd;
    } catch(e) {
      console.log(`Token ${id}: ERROR - ${e.shortMessage || e.message.slice(0,120)}`);
    }
  }
  
  console.log(`\nTotal claimable: ${totalClaimable.toFixed(6)} USDT`);
  
  if (totalClaimable > 0) {
    console.log('\n=== Withdrawing payouts ===');
    
    // Use withdrawPayouts (batch) if available, otherwise one by one
    try {
      const claimableIds = [];
      for (const id of TOKEN_IDS) {
        try {
          const p = await pc.readContract({
            address: LP,
            abi: tk.lpAbi,
            functionName: 'viewPayout',
            args: [CORE, BigInt(id)],
          });
          if (Number(p) > 0) claimableIds.push(BigInt(id));
        } catch(e) { /* skip */ }
      }
      
      if (claimableIds.length > 0) {
        console.log(`Withdrawing ${claimableIds.length} bets: ${claimableIds.join(', ')}`);
        
        // Try batch withdrawal
        const { request } = await pc.simulateContract({
          account,
          address: LP,
          abi: tk.lpAbi,
          functionName: 'withdrawPayouts',
          args: [CORE, claimableIds],
        });
        
        const hash = await wc.writeContract(request);
        console.log(`TX hash: ${hash}`);
        
        const receipt = await pc.waitForTransactionReceipt({ hash });
        console.log(`Status: ${receipt.status} | Gas: ${receipt.gasUsed}`);
      }
    } catch(e) {
      console.log(`Batch error: ${e.shortMessage || e.message.slice(0,200)}`);
      
      // Fallback: withdraw one by one
      console.log('\nFallback: withdrawing one by one...');
      for (const id of TOKEN_IDS) {
        try {
          const p = await pc.readContract({
            address: LP,
            abi: tk.lpAbi,
            functionName: 'viewPayout',
            args: [CORE, BigInt(id)],
          });
          if (Number(p) === 0) {
            console.log(`Token ${id}: nothing to claim, skip`);
            continue;
          }
          
          const { request } = await pc.simulateContract({
            account,
            address: LP,
            abi: tk.lpAbi,
            functionName: 'withdrawPayout',
            args: [CORE, BigInt(id)],
          });
          
          const hash = await wc.writeContract(request);
          console.log(`Token ${id}: TX ${hash}`);
          
          const receipt = await pc.waitForTransactionReceipt({ hash });
          console.log(`  Status: ${receipt.status}`);
        } catch(e) {
          console.log(`Token ${id}: ERROR - ${e.shortMessage || e.message.slice(0,120)}`);
        }
      }
    }
    
    // Final balance check
    const ERC20_ABI = [{ inputs: [{ name: 'account', type: 'address' }], name: 'balanceOf', outputs: [{ name: '', type: 'uint256' }], stateMutability: 'view', type: 'function' }];
    const bal = await pc.readContract({
      address: '0xc2132D05D31c914a87C6611C10748AEb04B58e8F',
      abi: ERC20_ABI,
      functionName: 'balanceOf',
      args: [account.address],
    });
    console.log(`\nFinal balance: ${(Number(bal) / 1e6).toFixed(6)} USDT`);
  }
}

main().catch(e => console.error('FATAL:', e.message));
