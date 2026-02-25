/**
 * Azuro Executor Sidecar — HTTP API pro bet placement + cashout
 * 
 * Používá oficiální @azuro-org/toolkit pro garantovanou kompatibilitu
 * s Azuro V3 Relayer API.
 * 
 * Env vars:
 *   PRIVATE_KEY    — hex private key (bez 0x prefixu i s ním)
 *   CHAIN_ID       — 137 (Polygon), 100 (Gnosis), 8453 (Base) — default: 137
 *   EXECUTOR_PORT  — HTTP port — default: 3030
 *   RPC_URL        — Polygon RPC — default: https://polygon-rpc.com
 * 
 * Endpoints:
 *   POST /bet      — place bet
 *   POST /cashout  — execute cashout
 *   GET  /bet/:id  — check bet status
 *   GET  /cashout/:id — check cashout status
 *   GET  /balance  — USDT balance
 *   GET  /health   — health check
 */

import express from 'express';
import { createWalletClient, createPublicClient, http, parseAbi, formatUnits } from 'viem';
import { privateKeyToAccount } from 'viem/accounts';
import { polygon, gnosis, base } from 'viem/chains';

// ============================================================
// Config
// ============================================================

const PORT = parseInt(process.env.EXECUTOR_PORT || '3030');
const CHAIN_ID = parseInt(process.env.CHAIN_ID || '137');
const RPC_URL = process.env.RPC_URL || 'https://polygon-bor-rpc.publicnode.com';

// Private key — optional, dry-run mode if not set
const RAW_KEY = process.env.PRIVATE_KEY;
const DRY_RUN = !RAW_KEY;

if (DRY_RUN) {
  console.warn('⚠️  DRY-RUN MODE — žádný PRIVATE_KEY');
  console.warn('   Bety budou simulovány, NE odesílány on-chain.');
  console.warn('   Pro živé bety nastav: $env:PRIVATE_KEY="0x..."');
}
const PRIVATE_KEY = RAW_KEY ? (RAW_KEY.startsWith('0x') ? RAW_KEY : `0x${RAW_KEY}`) : null;

// ============================================================
// Azuro V3 Contract Addresses (Production)
// ============================================================

const CONTRACTS = {
  137: { // Polygon
    lp: '0x0FA7FB5407eA971694652E6E16C12A52625DE1b8',
    core: '0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7',
    relayer: '0x8dA05c0021e6b35865FDC959c54dCeF3A4AbBa9d',
    azuroBet: '0x7A1c3FEf712753374C4DCe34254B96faF2B7265B',
    cashout: '0x4a2BB4211cCF9b9eA6eF01D0a61448154ED19095',
    betToken: '0xc2132D05D31c914a87C6611C10748AEb04B58e8F', // USDT
    betTokenDecimals: 6,
  },
  100: { // Gnosis
    lp: '0x0FA7FB5407eA971694652E6E16C12A52625DE1b8',
    core: '0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7',
    relayer: '0x8dA05c0021e6b35865FDC959c54dCeF3A4AbBa9d',
    azuroBet: '0x7A1c3FEf712753374C4DCe34254B96faF2B7265B',
    cashout: '0x4a2BB4211cCF9b9eA6eF01D0a61448154ED19095',
    betToken: '0xe91D153E0b41518A2Ce8Dd3D7944Fa863463a97d',
    betTokenDecimals: 18,
  },
};

const contracts = CONTRACTS[CHAIN_ID];
if (!contracts) {
  console.error(`❌ Unsupported chain ID: ${CHAIN_ID}. Use 137 (Polygon).`);
  process.exit(1);
}

// ============================================================
// Wallet + Client Setup
// ============================================================

const account = DRY_RUN ? null : privateKeyToAccount(PRIVATE_KEY);
const chain = CHAIN_ID === 137 ? polygon : CHAIN_ID === 100 ? gnosis : base;

const walletClient = DRY_RUN ? null : createWalletClient({
  account,
  chain,
  transport: http(RPC_URL),
});

const publicClient = createPublicClient({
  chain,
  transport: http(RPC_URL),
});

if (DRY_RUN) {
  console.log(`🧪 DRY-RUN: simulace na ${chain.name} (${CHAIN_ID})`);
} else {
  console.log(`🔐 Wallet: ${account.address}`);
  console.log(`⛓️  Chain: ${chain.name} (${CHAIN_ID})`);
  console.log(`📄 Bet Token (USDT): ${contracts.betToken}`);
}

// ============================================================
// Azuro Toolkit — dynamic import (ESM)
// ============================================================

let toolkit = null;
try {
  toolkit = await import('@azuro-org/toolkit');
  console.log('✅ @azuro-org/toolkit loaded');
} catch (e) {
  console.warn(`⚠️ @azuro-org/toolkit not available: ${e.message}`);
  console.warn('   Falling back to direct contract interaction');
}

// ============================================================
// ERC20 ABI for token operations
// ============================================================

const ERC20_ABI = parseAbi([
  'function balanceOf(address) view returns (uint256)',
  'function allowance(address,address) view returns (uint256)',
  'function approve(address,uint256) returns (bool)',
  'function decimals() view returns (uint8)',
]);

// ============================================================
// Express App
// ============================================================

const app = express();
app.use(express.json());

// Track active bets for auto-cashout
const activeBets = new Map();

// ============================================================
// GET /health
// ============================================================

app.get('/health', async (req, res) => {
  if (DRY_RUN) {
    return res.json({
      status: 'dry-run',
      mode: 'DRY-RUN (simulace)',
      wallet: 'none — nastav PRIVATE_KEY pro živé bety',
      chain: chain.name,
      chainId: CHAIN_ID,
      balance: '0.00',
      relayerAllowance: '0',
      activeBets: activeBets.size,
      toolkitAvailable: toolkit !== null,
    });
  }
  try {
    const balance = await publicClient.readContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: 'balanceOf',
      args: [account.address],
    });
    const formattedBalance = formatUnits(balance, contracts.betTokenDecimals);
    
    const allowance = await publicClient.readContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: 'allowance',
      args: [account.address, contracts.relayer],
    });
    const formattedAllowance = formatUnits(allowance, contracts.betTokenDecimals);

    res.json({
      status: 'ok',
      wallet: account.address,
      chain: chain.name,
      chainId: CHAIN_ID,
      betToken: contracts.betToken,
      balance: formattedBalance,
      relayerAllowance: formattedAllowance,
      activeBets: activeBets.size,
      toolkitAvailable: toolkit !== null,
    });
  } catch (e) {
    res.json({
      status: 'error',
      error: e.message,
      wallet: account.address,
    });
  }
});

// ============================================================
// GET /balance
// ============================================================

app.get('/balance', async (req, res) => {
  if (DRY_RUN) {
    return res.json({
      betToken: '0.00',
      native: '0.00',
      wallet: 'DRY-RUN',
      mode: 'Simulace — nastav PRIVATE_KEY',
    });
  }
  try {
    const balance = await publicClient.readContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: 'balanceOf',
      args: [account.address],
    });
    const nativeBalance = await publicClient.getBalance({ address: account.address });
    
    res.json({
      betToken: formatUnits(balance, contracts.betTokenDecimals),
      native: formatUnits(nativeBalance, 18),
      wallet: account.address,
    });
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// ============================================================
// POST /approve — one-time token approval for Relayer
// ============================================================

app.post('/approve', async (req, res) => {
  if (DRY_RUN) {
    return res.json({ status: 'dry-run', message: 'Simulace — approve neodesláno' });
  }
  try {
    const maxUint256 = 2n ** 256n - 1n;
    
    // Check current allowance first
    const currentAllowance = await publicClient.readContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: 'allowance',
      args: [account.address, contracts.relayer],
    });
    
    if (currentAllowance > 0n) {
      res.json({
        status: 'already_approved',
        allowance: formatUnits(currentAllowance, contracts.betTokenDecimals),
      });
      return;
    }

    const hash = await walletClient.writeContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: 'approve',
      args: [contracts.relayer, maxUint256],
    });

    const receipt = await publicClient.waitForTransactionReceipt({ hash });
    
    res.json({
      status: 'approved',
      txHash: hash,
      blockNumber: receipt.blockNumber.toString(),
    });
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// ============================================================
// POST /bet — place bet via Azuro Relayer
// ============================================================

app.post('/bet', async (req, res) => {
  const { conditionId, outcomeId, amount, minOdds, gameId, team1, team2 } = req.body;
  
  if (!conditionId || !outcomeId || !amount) {
    return res.status(400).json({ error: 'Missing: conditionId, outcomeId, amount' });
  }

  // === DRY-RUN: simulate bet ===
  if (DRY_RUN) {
    const fakeId = `dry-${Date.now()}`;
    const amountUsd = parseFloat(amount) / (10 ** contracts.betTokenDecimals);
    console.log(`🧪 DRY-RUN BET: condition=${conditionId} outcome=${outcomeId} amount=$${amountUsd.toFixed(2)}`);
    
    activeBets.set(fakeId, {
      id: fakeId,
      conditionId,
      outcomeId,
      amount: amountUsd,
      gameId,
      team1: team1 || '?',
      team2: team2 || '?',
      placedAt: new Date().toISOString(),
      state: 'DRY-RUN',
    });

    return res.json({
      status: 'ok',
      betId: fakeId,
      state: 'DRY-RUN',
      mode: 'SIMULACE — bet NEBYL odeslán on-chain',
      details: `Tady by proběl: EIP-712 sign → Azuro Relayer → on-chain bet za $${amountUsd.toFixed(2)}`,
    });
  }

  try {
    // amount and minOdds arrive ALREADY in raw format from alert_bot
    // amount: USDT with 6 decimals (e.g. "1000000" = $1)
    // minOdds: odds × 1e12 (e.g. "1054500000000" = 1.0545 odds)
    const amountRaw = BigInt(amount);
    const minOddsRaw = minOdds ? BigInt(minOdds) : 0n;
    const nonce = BigInt(Date.now());
    const expiresAt = Math.floor(Date.now() / 1000) + 300; // 5 min expiry

    // Safety: strip conditionId_ prefix from outcomeId if present (subgraph format)
    let cleanOutcomeId = outcomeId;
    if (typeof cleanOutcomeId === 'string' && cleanOutcomeId.includes('_')) {
      cleanOutcomeId = cleanOutcomeId.split('_').pop();
      console.log(`🔧 Stripped outcomeId prefix: ${outcomeId} → ${cleanOutcomeId}`);
    }
    console.log(`🎰 Placing bet: condition=${conditionId} outcome=${cleanOutcomeId} amount=$${amount} minOdds=${minOdds || 'any'}`);

    if (toolkit) {
      // === Official toolkit path ===
      const clientData = {
        attention: 'RustMisko CS2 Bot',
        affiliate: account.address,
        core: contracts.core,
        expiresAt,
        chainId: CHAIN_ID,
        relayerFeeAmount: '0',
        isBetSponsored: false,
        isFeeSponsored: false,
        isSponsoredBetReturnable: false,
      };

      const bet = {
        conditionId: conditionId.toString(),
        outcomeId: cleanOutcomeId.toString(),
        minOdds: minOddsRaw.toString(),
        amount: amountRaw.toString(),
        nonce: nonce.toString(),
      };

      const typedData = toolkit.getBetTypedData({
        account: account.address,
        clientData,
        bet,
      });

      const signature = await account.signTypedData(typedData);

      const result = await toolkit.createBet({
        account: account.address,
        clientData,
        bet,
        signature,
      });

      console.log(`✅ Bet placed: id=${result.id} state=${result.state}`);

      // Track for auto-cashout
      if (result.state === 'Accepted' || result.state === 'Created' || result.state === 'Pending') {
        activeBets.set(result.id, {
          id: result.id,
          conditionId,
          outcomeId,
          amount: parseFloat(amount),
          minOdds: parseFloat(minOdds || '0'),
          gameId,
          team1,
          team2,
          placedAt: new Date().toISOString(),
          state: result.state,
        });
      }

      res.json({
        status: 'ok',
        betId: result.id,
        state: result.state,
        error: result.errorMessage || result.error,
      });
    } else {
      // === Direct contract interaction fallback ===
      // This is a simplified path — for full reliability, use toolkit
      res.status(501).json({
        error: 'Toolkit not available. Install: cd executor && npm install',
        hint: 'npm install @azuro-org/toolkit viem express',
      });
    }
  } catch (e) {
    console.error(`❌ Bet error: ${e.message}`);
    res.status(500).json({ error: e.message, details: e.stack });
  }
});

// ============================================================
// GET /bet/:id — check bet status
// ============================================================

app.get('/bet/:id', async (req, res) => {
  try {
    if (!toolkit) {
      return res.status(501).json({ error: 'Toolkit not available' });
    }

    const result = await toolkit.getBet({
      chainId: CHAIN_ID,
      orderId: req.params.id,
    });

    // Update active bet state
    if (activeBets.has(req.params.id)) {
      activeBets.get(req.params.id).state = result.state;
      if (result.state === 'Rejected') {
        activeBets.delete(req.params.id);
      }
    }

    res.json(result);
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// ============================================================
// POST /cashout — execute cashout for a bet
// ============================================================

app.post('/cashout', async (req, res) => {
  const { betId, graphBetId, tokenId } = req.body;
  
  if (!graphBetId && !tokenId) {
    return res.status(400).json({ error: 'Missing: graphBetId or tokenId' });
  }

  if (DRY_RUN) {
    console.log(`\uD83E\uDDEA DRY-RUN CASHOUT: graphBetId=${graphBetId}`);
    if (betId) activeBets.delete(betId);
    return res.json({
      status: 'ok',
      cashoutId: `dry-cashout-${Date.now()}`,
      state: 'DRY-RUN',
      cashoutOdds: '1.50',
      mode: 'SIMULACE — cashout NEBYL odeslán',
    });
  }

  try {
    if (!toolkit) {
      return res.status(501).json({ error: 'Toolkit not available' });
    }

    console.log(`💰 Calculating cashout for bet: graphBetId=${graphBetId}`);

    // Step 1: Get cashout calculation
    const calculation = await toolkit.getCalculatedCashout({
      chainId: CHAIN_ID,
      account: account.address,
      graphBetId: graphBetId,
    });

    if (!calculation || !calculation.calculationId) {
      return res.status(400).json({
        error: 'Cashout not available for this bet',
        calculation,
      });
    }

    console.log(`📊 Cashout calculation: odds=${calculation.cashoutOdds} expires=${calculation.expiredAt}`);

    // Step 2: Sign typed data
    const typedData = toolkit.getCashoutTypedData({
      chainId: CHAIN_ID,
      account: account.address,
      attention: 'RustMisko auto-cashout',
      tokenId: tokenId || graphBetId,
      cashoutOdds: calculation.cashoutOdds,
      expiredAt: calculation.expiredAt,
    });

    const signature = await walletClient.signTypedData(typedData);

    // Step 3: Submit cashout
    const result = await toolkit.createCashout({
      chainId: CHAIN_ID,
      calculationId: calculation.calculationId,
      attention: 'RustMisko auto-cashout',
      signature,
    });

    console.log(`✅ Cashout submitted: id=${result.id} state=${result.state}`);

    // Remove from active bets
    if (betId) {
      activeBets.delete(betId);
    }

    res.json({
      status: 'ok',
      cashoutId: result.id,
      state: result.state,
      cashoutOdds: calculation.cashoutOdds,
      error: result.errorMessage,
    });
  } catch (e) {
    console.error(`❌ Cashout error: ${e.message}`);
    res.status(500).json({ error: e.message, details: e.stack });
  }
});

// ============================================================
// GET /cashout/:id — check cashout status
// ============================================================

app.get('/cashout/:id', async (req, res) => {
  try {
    if (!toolkit) {
      return res.status(501).json({ error: 'Toolkit not available' });
    }

    const result = await toolkit.getCashout({
      chainId: CHAIN_ID,
      orderId: req.params.id,
    });

    res.json(result);
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// ============================================================
// GET /active-bets — list all active bets
// ============================================================

app.get('/active-bets', (req, res) => {
  res.json({
    count: activeBets.size,
    bets: Array.from(activeBets.values()),
  });
});

// ============================================================
// POST /check-cashout — check if cashout is profitable
// ============================================================

app.post('/check-cashout', async (req, res) => {
  const { graphBetId } = req.body;
  
  if (!graphBetId) {
    return res.status(400).json({ error: 'Missing: graphBetId' });
  }

  if (DRY_RUN) {
    return res.json({
      available: false,
      mode: 'DRY-RUN — cashout check simulován',
    });
  }

  try {
    if (!toolkit) {
      return res.status(501).json({ error: 'Toolkit not available' });
    }

    const calculation = await toolkit.getCalculatedCashout({
      chainId: CHAIN_ID,
      account: account.address,
      graphBetId,
    });

    res.json({
      available: !!calculation?.calculationId,
      cashoutOdds: calculation?.cashoutOdds,
      expiredAt: calculation?.expiredAt,
      calculationId: calculation?.calculationId,
    });
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// ============================================================
// POST /check-payout — check claimable payout for a token ID
// ============================================================

app.post('/check-payout', async (req, res) => {
  const { tokenId } = req.body;
  if (!tokenId) {
    return res.status(400).json({ error: 'Missing: tokenId' });
  }
  if (DRY_RUN) {
    return res.json({ tokenId, payout: '0', payoutUsd: 0, claimable: false, mode: 'DRY-RUN' });
  }
  try {
    const payout = await publicClient.readContract({
      address: contracts.lp,
      abi: toolkit ? toolkit.lpAbi : parseAbi(['function viewPayout(address,uint256) view returns (uint128)']),
      functionName: 'viewPayout',
      args: [contracts.core, BigInt(tokenId)],
    });
    const payoutUsd = Number(payout) / (10 ** contracts.betTokenDecimals);
    res.json({
      tokenId,
      payout: payout.toString(),
      payoutUsd,
      claimable: payoutUsd > 0,
    });
  } catch (e) {
    res.status(500).json({ error: e.message, tokenId });
  }
});

// ============================================================
// POST /claim — withdraw payouts for settled bets
// ============================================================

app.post('/claim', async (req, res) => {
  const { tokenIds } = req.body;
  if (!tokenIds || !Array.isArray(tokenIds) || tokenIds.length === 0) {
    return res.status(400).json({ error: 'Missing: tokenIds (array of token IDs)' });
  }
  if (DRY_RUN) {
    return res.json({ status: 'dry-run', message: 'Simulace — claim neodesláno', tokenIds });
  }
  try {
    const abi = toolkit ? toolkit.lpAbi : parseAbi([
      'function withdrawPayout(address,uint256)',
      'function withdrawPayouts(address,uint256[])',
      'function viewPayout(address,uint256) view returns (uint128)',
    ]);

    // First check which tokens have claimable payouts
    const claimable = [];
    let totalPayout = 0;
    for (const tid of tokenIds) {
      try {
        const p = await publicClient.readContract({
          address: contracts.lp, abi,
          functionName: 'viewPayout',
          args: [contracts.core, BigInt(tid)],
        });
        const usd = Number(p) / (10 ** contracts.betTokenDecimals);
        if (usd > 0) {
          claimable.push(BigInt(tid));
          totalPayout += usd;
        }
      } catch (_) { /* skip */ }
    }

    if (claimable.length === 0) {
      return res.json({ status: 'nothing', message: 'No claimable payouts', tokenIds });
    }

    console.log(`💰 Claiming ${claimable.length} bets, total ~$${totalPayout.toFixed(2)}`);

    // Batch withdraw
    const { request } = await publicClient.simulateContract({
      account,
      address: contracts.lp,
      abi,
      functionName: claimable.length === 1 ? 'withdrawPayout' : 'withdrawPayouts',
      args: claimable.length === 1
        ? [contracts.core, claimable[0]]
        : [contracts.core, claimable],
    });

    const hash = await walletClient.writeContract(request);
    console.log(`📤 Claim TX: ${hash}`);

    const receipt = await publicClient.waitForTransactionReceipt({ hash });
    console.log(`✅ Claim confirmed: status=${receipt.status} gas=${receipt.gasUsed}`);

    // Get updated balance
    const balance = await publicClient.readContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: 'balanceOf',
      args: [account.address],
    });
    const balanceUsd = formatUnits(balance, contracts.betTokenDecimals);

    res.json({
      status: 'ok',
      txHash: hash,
      claimed: claimable.length,
      totalPayoutUsd: totalPayout,
      newBalanceUsd: balanceUsd,
      blockNumber: receipt.blockNumber.toString(),
    });
  } catch (e) {
    console.error(`❌ Claim error: ${e.message}`);
    res.status(500).json({ error: e.message, details: e.stack });
  }
});

// ============================================================
// Start
// ============================================================

app.listen(PORT, '127.0.0.1', () => {
  console.log(`\n🚀 Azuro Executor sidecar running on http://127.0.0.1:${PORT}`);
  console.log(`   Mode: ${DRY_RUN ? '\uD83E\uDDEA DRY-RUN (simulace)' : '\uD83D\uDD25 LIVE (on-chain)'}`);
  console.log(`   Chain: ${chain.name} (${CHAIN_ID})`);
  if (!DRY_RUN) console.log(`   Wallet: ${account.address}`);
  console.log(`   Endpoints: /health /bet /cashout /check-cashout /check-payout /claim /active-bets\n`);
});
