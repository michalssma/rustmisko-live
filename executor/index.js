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

import express from "express";
import {
  createWalletClient,
  createPublicClient,
  http,
  fallback,
  parseAbi,
  formatUnits,
} from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { polygon, gnosis, base } from "viem/chains";
import fs from "fs";
import path from "path";

// ============================================================
// Config
// ============================================================

const PORT = parseInt(process.env.EXECUTOR_PORT || "3030");
const CHAIN_ID = parseInt(process.env.CHAIN_ID || "137");
const RPC_URL = process.env.RPC_URL || "https://polygon-bor-rpc.publicnode.com";

// Multiple RPC endpoints for reliability (fallback chain)
// NOTE: Ankr + polygon-rpc.com removed — require API keys since ~Feb 2026
const RPC_URLS = [
  RPC_URL,
  "https://1rpc.io/matic",
  "https://polygon.drpc.org",
];

// Private key — optional, dry-run mode if not set
const RAW_KEY = process.env.PRIVATE_KEY;
const DRY_RUN = !RAW_KEY;

if (DRY_RUN) {
  console.warn("⚠️  DRY-RUN MODE — žádný PRIVATE_KEY");
  console.warn("   Bety budou simulovány, NE odesílány on-chain.");
  console.warn('   Pro živé bety nastav: $env:PRIVATE_KEY="0x..."');
}
const PRIVATE_KEY = RAW_KEY
  ? RAW_KEY.startsWith("0x")
    ? RAW_KEY
    : `0x${RAW_KEY}`
  : null;

// ============================================================
// Azuro V3 Contract Addresses (Production)
// ============================================================

const CONTRACTS = {
  137: {
    // Polygon
    lp: "0x0FA7FB5407eA971694652E6E16C12A52625DE1b8",
    core: "0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7",
    relayer: "0x8dA05c0021e6b35865FDC959c54dCeF3A4AbBa9d",
    azuroBet: "0x7A1c3FEf712753374C4DCe34254B96faF2B7265B",
    cashout: "0x4a2BB4211cCF9b9eA6eF01D0a61448154ED19095",
    betToken: "0xc2132D05D31c914a87C6611C10748AEb04B58e8F", // USDT
    betTokenDecimals: 6,
  },
  100: {
    // Gnosis
    lp: "0x0FA7FB5407eA971694652E6E16C12A52625DE1b8",
    core: "0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7",
    relayer: "0x8dA05c0021e6b35865FDC959c54dCeF3A4AbBa9d",
    azuroBet: "0x7A1c3FEf712753374C4DCe34254B96faF2B7265B",
    cashout: "0x4a2BB4211cCF9b9eA6eF01D0a61448154ED19095",
    betToken: "0xe91D153E0b41518A2Ce8Dd3D7944Fa863463a97d",
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

const rpcTransport = fallback(
  RPC_URLS.map((url) => http(url)),
  { rank: true },
);

const walletClient = DRY_RUN
  ? null
  : createWalletClient({
      account,
      chain,
      transport: rpcTransport,
    });

const publicClient = createPublicClient({
  chain,
  transport: rpcTransport,
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
  toolkit = await import("@azuro-org/toolkit");
  console.log("✅ @azuro-org/toolkit loaded");
} catch (e) {
  console.warn(`⚠️ @azuro-org/toolkit not available: ${e.message}`);
  console.warn("   Falling back to direct contract interaction");
}

// ============================================================
// ERC20 ABI for token operations
// ============================================================

const ERC20_ABI = parseAbi([
  "function balanceOf(address) view returns (uint256)",
  "function allowance(address,address) view returns (uint256)",
  "function approve(address,uint256) returns (bool)",
  "function decimals() view returns (uint8)",
]);

// ============================================================
// Express App
// ============================================================

const app = express();
app.use(express.json());

// Track active bets for auto-cashout
const activeBets = new Map();

// ============================================================
// Persistent Active Bets (data/active_bets.json)
// Survives executor restarts for auto-cashout continuity
// ============================================================

const ACTIVE_BETS_PATH = path.resolve(
  process.cwd(),
  "..",
  "data",
  "active_bets.json",
);

// Condition dedup: prevent duplicate on-chain bets
const bettedConditions = new Set();

function loadActiveBetsFromDisk() {
  try {
    if (!fs.existsSync(ACTIVE_BETS_PATH)) return [];
    const data = JSON.parse(fs.readFileSync(ACTIVE_BETS_PATH, "utf8"));
    return Array.isArray(data) ? data : [];
  } catch {
    return [];
  }
}

function saveActiveBetsToDisk() {
  const arr = Array.from(activeBets.values());
  try {
    fs.writeFileSync(ACTIVE_BETS_PATH, JSON.stringify(arr, null, 2));
  } catch (e) {
    console.error(`⚠️ Failed to save active_bets.json: ${e.message}`);
  }
}

// Load persisted bets on startup
{
  const loaded = loadActiveBetsFromDisk();
  for (const b of loaded) {
    activeBets.set(b.betId || b.tokenId, b);
    if (b.conditionId) bettedConditions.add(b.conditionId);
  }
  if (loaded.length > 0) {
    console.log(`📋 Loaded ${loaded.length} active bets from disk (${bettedConditions.size} conditions in dedup)`);
  }
}

// Auto-prune settled bets on startup (after toolkit becomes available)
// SAFETY: Only removes Lost/Rejected immediately.
// Won/Canceled are removed ONLY if on-chain viewPayout confirms already claimed.
async function autoPruneSettled() {
  if (!toolkit || activeBets.size === 0) return;
  const before = activeBets.size;
  let removed = 0;
  let kept_won = 0;
  const lpAbi = parseAbi(['function viewPayout(address,uint256) view returns (uint128)']);
  for (const [key, bet] of activeBets.entries()) {
    try {
      const result = await toolkit.getBet({ chainId: CHAIN_ID, orderId: bet.betId || key });
      const st = result?.state || "";
      const rs = result?.result || "";
      // Rejected: never on-chain, safe to remove
      if (st === "Rejected") { activeBets.delete(key); removed++; continue; }
      // Lost: no payout, safe to remove
      if (rs === "Lost") { activeBets.delete(key); removed++; continue; }
      // Won/Canceled: ONLY remove if on-chain payout already claimed
      if (rs === "Won" || rs === "Canceled" || st === "Canceled") {
        const tid = extractTokenIdFromUnknown(result);
        if (tid && !DRY_RUN) {
          try {
            const payout = await publicClient.readContract({
              address: contracts.lp, abi: lpAbi,
              functionName: 'viewPayout', args: [contracts.core, BigInt(tid)],
            });
            // viewPayout returns 0 after claim (NFT burned), safe to prune
            if (Number(payout) === 0) { activeBets.delete(key); removed++; continue; }
          } catch {
            // viewPayout reverted = not yet resolved on-chain, KEEP!
          }
        }
        kept_won++;
        continue;
      }
    } catch {}
  }
  if (removed > 0) {
    saveActiveBetsToDisk();
    console.log(`🧹 Auto-prune on startup: ${removed} removed (Lost/Rejected/Claimed), ${kept_won} Won/Canceled kept (awaiting claim). ${before} -> ${activeBets.size}`);
  }
}
// Schedule auto-prune 10s after startup (give toolkit time to init)
setTimeout(() => autoPruneSettled().catch(e => console.error(`Auto-prune error: ${e.message}`)), 10000);

function extractTokenIdFromUnknown(value) {
  if (value === null || value === undefined) return null;
  if (typeof value === "bigint") return value.toString();
  if (typeof value === "number" && Number.isFinite(value))
    return Math.trunc(value).toString();
  if (typeof value === "string") {
    if (/^\d+$/.test(value)) return value;
    return null;
  }
  if (Array.isArray(value)) {
    for (const item of value) {
      const found = extractTokenIdFromUnknown(item);
      if (found) return found;
    }
    return null;
  }
  if (typeof value === "object") {
    // Prefer explicit token keys
    for (const k of ["tokenId", "tokenID", "betTokenId"]) {
      if (Object.prototype.hasOwnProperty.call(value, k)) {
        const found = extractTokenIdFromUnknown(value[k]);
        if (found) return found;
      }
    }
    // Common Azuro key: betId (NFT token id)
    for (const k of ["betId"]) {
      if (Object.prototype.hasOwnProperty.call(value, k)) {
        const found = extractTokenIdFromUnknown(value[k]);
        if (found) return found;
      }
    }
    // Recursive search fallback
    for (const v of Object.values(value)) {
      const found = extractTokenIdFromUnknown(v);
      if (found) return found;
    }
  }
  return null;
}

function loadPendingClaimsRows() {
  if (!fs.existsSync(PENDING_CLAIMS_PATH)) return [];
  const lines = fs
    .readFileSync(PENDING_CLAIMS_PATH, "utf8")
    .split(/\r?\n/)
    .map((s) => s.trim())
    .filter(Boolean);

  const out = [];
  for (const line of lines) {
    const p = line.split("|");
    if (p.length < 6) continue;
    out.push({
      tokenId: p[0],
      betId: p[1],
      matchKey: p[2],
      team: p[3],
      stake: p[4],
      odds: p[5],
      ts: p[6] || null,
    });
  }
  return out;
}

// ============================================================
// GET /health
// ============================================================

app.get("/health", async (req, res) => {
  if (DRY_RUN) {
    return res.json({
      status: "dry-run",
      mode: "DRY-RUN (simulace)",
      wallet: "none — nastav PRIVATE_KEY pro živé bety",
      chain: chain.name,
      chainId: CHAIN_ID,
      balance: "0.00",
      relayerAllowance: "0",
      activeBets: activeBets.size,
      toolkitAvailable: toolkit !== null,
    });
  }
  try {
    const balance = await publicClient.readContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: "balanceOf",
      args: [account.address],
    });
    const formattedBalance = formatUnits(balance, contracts.betTokenDecimals);

    const allowance = await publicClient.readContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: "allowance",
      args: [account.address, contracts.relayer],
    });
    const formattedAllowance = formatUnits(
      allowance,
      contracts.betTokenDecimals,
    );

    res.json({
      status: "ok",
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
      status: "error",
      error: e.message,
      wallet: account.address,
    });
  }
});

// ============================================================
// GET /balance
// ============================================================

app.get("/balance", async (req, res) => {
  if (DRY_RUN) {
    return res.json({
      betToken: "0.00",
      native: "0.00",
      wallet: "DRY-RUN",
      mode: "Simulace — nastav PRIVATE_KEY",
    });
  }
  try {
    const balance = await publicClient.readContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: "balanceOf",
      args: [account.address],
    });
    const nativeBalance = await publicClient.getBalance({
      address: account.address,
    });

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

app.post("/approve", async (req, res) => {
  if (DRY_RUN) {
    return res.json({
      status: "dry-run",
      message: "Simulace — approve neodesláno",
    });
  }
  try {
    const maxUint256 = 2n ** 256n - 1n;

    // Check current allowance first
    const currentAllowance = await publicClient.readContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: "allowance",
      args: [account.address, contracts.relayer],
    });

    if (currentAllowance > 0n) {
      res.json({
        status: "already_approved",
        allowance: formatUnits(currentAllowance, contracts.betTokenDecimals),
      });
      return;
    }

    const hash = await walletClient.writeContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: "approve",
      args: [contracts.relayer, maxUint256],
    });

    const receipt = await publicClient.waitForTransactionReceipt({ hash });

    res.json({
      status: "approved",
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

app.post("/bet", async (req, res) => {
  const { conditionId, outcomeId, amount, minOdds, gameId, team1, team2 } =
    req.body;

  if (!conditionId || !outcomeId || !amount) {
    return res
      .status(400)
      .json({ error: "Missing: conditionId, outcomeId, amount" });
  }

  // DEDUP: prevent duplicate on-chain bets for same condition
  if (bettedConditions.has(conditionId)) {
    console.log(`🚫 DEDUP: condition ${conditionId} already bet — rejecting`);
    return res.status(409).json({
      error: "DEDUP: Already bet on this condition",
      conditionId,
    });
  }

  // === DRY-RUN: simulate bet ===
  if (DRY_RUN) {
    const fakeId = `dry-${Date.now()}`;
    const amountUsd = parseFloat(amount) / 10 ** contracts.betTokenDecimals;
    console.log(
      `🧪 DRY-RUN BET: condition=${conditionId} outcome=${outcomeId} amount=$${amountUsd.toFixed(2)}`,
    );

    activeBets.set(fakeId, {
      id: fakeId,
      conditionId,
      outcomeId,
      amount: amountUsd,
      gameId,
      team1: team1 || "?",
      team2: team2 || "?",
      placedAt: new Date().toISOString(),
      state: "DRY-RUN",
    });

    return res.json({
      status: "ok",
      betId: fakeId,
      state: "DRY-RUN",
      mode: "SIMULACE — bet NEBYL odeslán on-chain",
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
    if (typeof cleanOutcomeId === "string" && cleanOutcomeId.includes("_")) {
      cleanOutcomeId = cleanOutcomeId.split("_").pop();
      console.log(
        `🔧 Stripped outcomeId prefix: ${outcomeId} → ${cleanOutcomeId}`,
      );
    }
    console.log(
      `🎰 Placing bet: condition=${conditionId} outcome=${cleanOutcomeId} amount=$${amount} minOdds=${minOdds || "any"}`,
    );

    if (toolkit) {
      // === Official toolkit path ===
      const clientData = {
        attention: "RustMisko CS2 Bot",
        affiliate: account.address,
        core: contracts.core,
        expiresAt,
        chainId: CHAIN_ID,
        relayerFeeAmount: "0",
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

      const discoveredTokenId = extractTokenIdFromUnknown(result);
      const graphBetId = discoveredTokenId
        ? `${contracts.core.toLowerCase()}_${discoveredTokenId}`
        : null;

      console.log(`✅ Bet placed: id=${result.id} state=${result.state}`);

      // Track for auto-cashout
      if (
        result.state === "Accepted" ||
        result.state === "Created" ||
        result.state === "Pending"
      ) {
        const selectedTeam =
          req.body.valueTeam ||
          req.body.value_team ||
          req.body.team ||
          team1 ||
          "";

        const betData = {
          betId: result.id,
          tokenId: discoveredTokenId,
          graphBetId,
          conditionId,
          outcomeId: cleanOutcomeId,
          stake: parseFloat(amount) / 1e6,
          odds: parseFloat(minOdds || "0") / 1e12,
          sport: (req.body.matchKey || "").split("::")[0] || "?",
          matchKey: req.body.matchKey || "",
          team: selectedTeam,
          placedAt: new Date().toISOString(),
          status: "pending",
        };
        activeBets.set(result.id, betData);
        bettedConditions.add(conditionId);
        saveActiveBetsToDisk();
        console.log(`💾 Saved active bet: ${result.id} condition=${conditionId}`);
      }

      res.json({
        status: "ok",
        betId: result.id,
        tokenId: discoveredTokenId,
        graphBetId,
        state: result.state,
        error: result.errorMessage || result.error,
      });
    } else {
      // === Direct contract interaction fallback ===
      // This is a simplified path — for full reliability, use toolkit
      res.status(501).json({
        error: "Toolkit not available. Install: cd executor && npm install",
        hint: "npm install @azuro-org/toolkit viem express",
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

app.get("/bet/:id", async (req, res) => {
  try {
    if (!toolkit) {
      return res.status(501).json({ error: "Toolkit not available" });
    }

    const result = await toolkit.getBet({
      chainId: CHAIN_ID,
      orderId: req.params.id,
    });

    const discoveredTokenId = extractTokenIdFromUnknown(result);
    const graphBetId = discoveredTokenId
      ? `${contracts.core.toLowerCase()}_${discoveredTokenId}`
      : null;

    // Update active bet state
    if (activeBets.has(req.params.id)) {
      activeBets.get(req.params.id).state = result.state;
      if (result.state === "Rejected") {
        activeBets.delete(req.params.id);
      }
    }

    res.json({
      ...result,
      tokenId: discoveredTokenId,
      graphBetId,
    });
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// ============================================================
// POST /cashout — execute cashout for a bet
// ============================================================

app.post("/cashout", async (req, res) => {
  const { betId, graphBetId, tokenId } = req.body;
  // Construct graphBetId from tokenId if not provided
  const effectiveGraphBetId =
    graphBetId ||
    (tokenId ? `${contracts.core.toLowerCase()}_${tokenId}` : null);
  const effectiveTokenId =
    tokenId || (graphBetId ? graphBetId.split("_").pop() : null);

  if (!effectiveGraphBetId && !effectiveTokenId) {
    return res.status(400).json({ error: "Missing: graphBetId or tokenId" });
  }

  if (DRY_RUN) {
    console.log(
      `\uD83E\uDDEA DRY-RUN CASHOUT: graphBetId=${effectiveGraphBetId}`,
    );
    if (betId) activeBets.delete(betId);
    return res.json({
      status: "ok",
      cashoutId: `dry-cashout-${Date.now()}`,
      state: "DRY-RUN",
      cashoutOdds: "1.50",
      mode: "SIMULACE — cashout NEBYL odeslán",
    });
  }

  try {
    if (!toolkit) {
      return res.status(501).json({ error: "Toolkit not available" });
    }

    console.log(
      `💰 Calculating cashout for bet: graphBetId=${effectiveGraphBetId} tokenId=${effectiveTokenId}`,
    );

    // Step 1: Get cashout calculation
    const calculation = await toolkit.getCalculatedCashout({
      chainId: CHAIN_ID,
      account: account.address,
      graphBetId: effectiveGraphBetId,
    });

    if (!calculation || !calculation.calculationId) {
      return res.status(400).json({
        error: "Cashout not available for this bet",
        calculation,
      });
    }

    console.log(
      `📊 Cashout calculation: odds=${calculation.cashoutOdds} expires=${calculation.expiredAt}`,
    );

    // Step 2: Sign typed data
    const typedData = toolkit.getCashoutTypedData({
      chainId: CHAIN_ID,
      account: account.address,
      attention: "RustMisko auto-cashout",
      tokenId: effectiveTokenId || effectiveGraphBetId,
      cashoutOdds: calculation.cashoutOdds,
      expiredAt: calculation.expiredAt,
    });

    const signature = await walletClient.signTypedData(typedData);

    // Step 3: Submit cashout
    const result = await toolkit.createCashout({
      chainId: CHAIN_ID,
      calculationId: calculation.calculationId,
      attention: "RustMisko auto-cashout",
      signature,
    });

    console.log(`✅ Cashout submitted: id=${result.id} state=${result.state}`);

    // Remove from active bets
    if (betId) {
      activeBets.delete(betId);
    }

    res.json({
      status: "ok",
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

app.get("/cashout/:id", async (req, res) => {
  try {
    if (!toolkit) {
      return res.status(501).json({ error: "Toolkit not available" });
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

app.get("/active-bets", (req, res) => {
  res.json({
    count: activeBets.size,
    bets: Array.from(activeBets.values()),
  });
});

// ============================================================
// POST /prune-settled — remove settled/resolved bets from active tracking
// ============================================================

app.post("/prune-settled", async (req, res) => {
  if (!toolkit) {
    return res.status(501).json({ error: "Toolkit not available" });
  }
  const before = activeBets.size;
  const removed = [];
  const kept = [];
  const lpAbi = parseAbi(['function viewPayout(address,uint256) view returns (uint128)']);
  for (const [key, bet] of activeBets.entries()) {
    try {
      const result = await toolkit.getBet({ chainId: CHAIN_ID, orderId: bet.betId || key });
      const st = result?.state || "";
      const rs = result?.result || "";
      // Safe to remove: Rejected (never on-chain) and Lost (no payout)
      if (st === "Rejected" || rs === "Lost") {
        removed.push({ team: bet.team, state: st, result: rs, reason: "safe-dead" });
        activeBets.delete(key);
        continue;
      }
      // Won/Canceled: only prune if on-chain confirms already claimed
      if (rs === "Won" || rs === "Canceled" || st === "Canceled") {
        const tid = extractTokenIdFromUnknown(result);
        if (tid && !DRY_RUN) {
          try {
            const payout = await publicClient.readContract({
              address: contracts.lp, abi: lpAbi,
              functionName: 'viewPayout', args: [contracts.core, BigInt(tid)],
            });
            if (Number(payout) === 0) {
              removed.push({ team: bet.team, state: st, result: rs, reason: "claimed" });
              activeBets.delete(key);
              continue;
            } else {
              kept.push({ team: bet.team, state: st, result: rs, tokenId: tid, payoutUsd: Number(payout) / 10**contracts.betTokenDecimals, reason: "UNCLAIMED — DO NOT DELETE" });
              continue;
            }
          } catch {
            kept.push({ team: bet.team, state: st, result: rs, reason: "oracle-pending (viewPayout reverted)" });
            continue;
          }
        }
        kept.push({ team: bet.team, state: st, result: rs, reason: "no-tokenId-or-dry-run" });
        continue;
      }
    } catch {}
  }
  if (removed.length > 0) saveActiveBetsToDisk();
  console.log(`🧹 Prune: ${removed.length} removed, ${kept.length} kept (have money). ${before} -> ${activeBets.size}`);
  res.json({ before, after: activeBets.size, removed, kept });
});

// ============================================================
// POST /check-cashout — check if cashout is profitable
// ============================================================

app.post("/check-cashout", async (req, res) => {
  const { graphBetId, tokenId } = req.body;
  // Construct graphBetId from tokenId if not provided
  // Azuro subgraph format: {coreAddress_lowercase}_{tokenId}
  const effectiveGraphBetId =
    graphBetId ||
    (tokenId ? `${contracts.core.toLowerCase()}_${tokenId}` : null);

  if (!effectiveGraphBetId) {
    return res.status(400).json({ error: "Missing: graphBetId or tokenId" });
  }

  if (DRY_RUN) {
    return res.json({
      available: false,
      mode: "DRY-RUN — cashout check simulován",
    });
  }

  try {
    if (!toolkit) {
      return res.status(501).json({ error: "Toolkit not available" });
    }

    console.log(`🔍 Check-cashout: graphBetId=${effectiveGraphBetId}`);
    const calculation = await toolkit.getCalculatedCashout({
      chainId: CHAIN_ID,
      account: account.address,
      graphBetId: effectiveGraphBetId,
    });

    res.json({
      available: !!calculation?.calculationId,
      cashoutOdds: calculation?.cashoutOdds,
      expiredAt: calculation?.expiredAt,
      calculationId: calculation?.calculationId,
    });
  } catch (e) {
    // Cashout not available is normal (bet not in right state)
    if (
      e.message?.includes("not found") ||
      e.message?.includes("not available")
    ) {
      return res.json({ available: false, reason: e.message });
    }
    res.status(500).json({ error: e.message });
  }
});

// ============================================================
// POST /check-payout — check claimable payout for a token ID
// ============================================================

app.post("/check-payout", async (req, res) => {
  const { tokenId } = req.body;
  if (!tokenId) {
    return res.status(400).json({ error: "Missing: tokenId" });
  }
  if (DRY_RUN) {
    return res.json({
      tokenId,
      payout: "0",
      payoutUsd: 0,
      claimable: false,
      mode: "DRY-RUN",
    });
  }
  try {
    const payout = await publicClient.readContract({
      address: contracts.lp,
      abi: toolkit
        ? toolkit.lpAbi
        : parseAbi([
            "function viewPayout(address,uint256) view returns (uint128)",
          ]),
      functionName: "viewPayout",
      args: [contracts.core, BigInt(tokenId)],
    });
    const payoutUsd = Number(payout) / 10 ** contracts.betTokenDecimals;
    res.json({
      tokenId,
      payout: payout.toString(),
      payoutUsd,
      claimable: payoutUsd > 0,
    });
  } catch (e) {
    // viewPayout reverts for unresolved bets — return pending status
    res.json({
      tokenId,
      payout: "0",
      payoutUsd: 0,
      claimable: false,
      pending: true,
      reason: "Bet not yet resolved (viewPayout reverted)",
    });
  }
});

// ============================================================
// POST /claim — withdraw payouts for settled bets
// ============================================================

app.post("/claim", async (req, res) => {
  const { tokenIds } = req.body;
  if (!tokenIds || !Array.isArray(tokenIds) || tokenIds.length === 0) {
    return res
      .status(400)
      .json({ error: "Missing: tokenIds (array of token IDs)" });
  }
  if (DRY_RUN) {
    return res.json({
      status: "dry-run",
      message: "Simulace — claim neodesláno",
      tokenIds,
    });
  }
  try {
    const abi = toolkit
      ? toolkit.lpAbi
      : parseAbi([
          "function withdrawPayout(address,uint256)",
          "function withdrawPayouts(address,uint256[])",
          "function viewPayout(address,uint256) view returns (uint128)",
        ]);

    // First check which tokens have claimable payouts
    const claimable = [];
    let totalPayout = 0;
    for (const tid of tokenIds) {
      try {
        const p = await publicClient.readContract({
          address: contracts.lp,
          abi,
          functionName: "viewPayout",
          args: [contracts.core, BigInt(tid)],
        });
        const usd = Number(p) / 10 ** contracts.betTokenDecimals;
        if (usd > 0) {
          claimable.push(BigInt(tid));
          totalPayout += usd;
        }
      } catch (_) {
        /* skip */
      }
    }

    if (claimable.length === 0) {
      return res.json({
        status: "nothing",
        message: "No claimable payouts",
        tokenIds,
      });
    }

    console.log(
      `💰 Claiming ${claimable.length} bets, total ~$${totalPayout.toFixed(2)}`,
    );

    // Batch withdraw
    const { request } = await publicClient.simulateContract({
      account,
      address: contracts.lp,
      abi,
      functionName:
        claimable.length === 1 ? "withdrawPayout" : "withdrawPayouts",
      args:
        claimable.length === 1
          ? [contracts.core, claimable[0]]
          : [contracts.core, claimable],
    });

    const hash = await walletClient.writeContract(request);
    console.log(`📤 Claim TX: ${hash}`);

    const receipt = await publicClient.waitForTransactionReceipt({ hash });
    console.log(
      `✅ Claim confirmed: status=${receipt.status} gas=${receipt.gasUsed}`,
    );

    // Get updated balance
    const balance = await publicClient.readContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: "balanceOf",
      args: [account.address],
    });
    const balanceUsd = formatUnits(balance, contracts.betTokenDecimals);

    res.json({
      status: "ok",
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
// GET /my-bets — ON-CHAIN NFT enumeration (no subgraph!)
// Enumerates AzuroBet NFTs owned by wallet, checks payout status
// ============================================================

app.get('/my-bets', async (req, res) => {
  const walletAddr = DRY_RUN
    ? (process.env.WALLET_ADDRESS || null)
    : account?.address;
  if (!walletAddr) {
    return res.json({ bets: [], mode: 'DRY-RUN', warning: 'Nastav PRIVATE_KEY nebo WALLET_ADDRESS' });
  }

  try {
    const erc721Abi = parseAbi([
      'function balanceOf(address) view returns (uint256)',
      'function tokenOfOwnerByIndex(address,uint256) view returns (uint256)',
    ]);
    const lpAbi = parseAbi([
      'function viewPayout(address,uint256) view returns (uint128)',
    ]);

    // Step 1: Count NFTs
    const nftCount = Number(await publicClient.readContract({
      address: contracts.azuroBet, abi: erc721Abi,
      functionName: 'balanceOf', args: [walletAddr],
    }));

    if (nftCount === 0) {
      return res.json({ total: 0, claimable: 0, pending: 0, lost: 0, bets: [], source: 'on-chain' });
    }

    // Step 2: Enumerate tokenIds (batches of 10)
    const tokenIds = [];
    for (let i = 0; i < nftCount; i += 10) {
      const batch = [];
      for (let j = i; j < Math.min(i + 10, nftCount); j++) {
        batch.push(publicClient.readContract({
          address: contracts.azuroBet, abi: erc721Abi,
          functionName: 'tokenOfOwnerByIndex', args: [walletAddr, BigInt(j)],
        }));
      }
      tokenIds.push(...await Promise.all(batch));
    }

    // Step 3: Check viewPayout for each (parallel)
    const results = await Promise.all(tokenIds.map(tid =>
      publicClient.readContract({
        address: contracts.lp, abi: lpAbi,
        functionName: 'viewPayout', args: [contracts.core, tid],
      }).then(p => {
        const usd = Number(p) / (10 ** contracts.betTokenDecimals);
        return { tokenId: tid.toString(), payoutUsd: usd, status: usd > 0 ? 'claimable' : 'lost' };
      }).catch(() => ({ tokenId: tid.toString(), payoutUsd: 0, status: 'pending' }))
    ));

    const claimable = results.filter(r => r.status === 'claimable');
    const pending = results.filter(r => r.status === 'pending');
    const lost = results.filter(r => r.status === 'lost');
    const totalClaimableUsd = claimable.reduce((s, r) => s + r.payoutUsd, 0);

    // Get USDT balance
    const balance = await publicClient.readContract({
      address: contracts.betToken, abi: ERC20_ABI,
      functionName: 'balanceOf', args: [walletAddr],
    });

    console.log(`📊 My-bets on-chain: ${nftCount} NFTs, ${claimable.length} claimable ($${totalClaimableUsd.toFixed(2)}), ${pending.length} pending, ${lost.length} lost`);

    res.json({
      total: nftCount,
      claimable: claimable.length,
      claimableUsd: totalClaimableUsd,
      pending: pending.length,
      lost: lost.length,
      balanceUsd: formatUnits(balance, contracts.betTokenDecimals),
      source: 'on-chain',
      bets: results,
    });
  } catch (e) {
    console.error(`❌ My-bets error: ${e.message}`);
    res.status(500).json({ error: e.message });
  }
});

// ============================================================
// POST /auto-claim — find and claim ALL redeemable bets automatically
// ============================================================

app.post("/auto-claim", async (req, res) => {
  if (DRY_RUN) {
    return res.json({
      status: "dry-run",
      claimed: 0,
      warning: "Nastav PRIVATE_KEY pro live claim",
    });
  }

  try {
    // ============================================================
    // ON-CHAIN NFT ENUMERATION — no subgraph dependency!
    // AzuroBet.balanceOf → tokenOfOwnerByIndex → LP.viewPayout
    // ============================================================
    const erc721Abi = parseAbi([
      "function balanceOf(address) view returns (uint256)",
      "function tokenOfOwnerByIndex(address,uint256) view returns (uint256)",
    ]);
    const lpAbi = toolkit
      ? toolkit.lpAbi
      : parseAbi([
          "function withdrawPayout(address,uint256)",
          "function withdrawPayouts(address,uint256[])",
          "function viewPayout(address,uint256) view returns (uint128)",
        ]);

    // Step 1: Count NFTs owned
    const nftCount = Number(
      await publicClient.readContract({
        address: contracts.azuroBet,
        abi: erc721Abi,
        functionName: "balanceOf",
        args: [account.address],
      }),
    );

    if (nftCount === 0) {
      return res.json({
        status: "nothing",
        message: "No AzuroBet NFTs owned",
        nftCount: 0,
      });
    }

    console.log(`🔍 Auto-claim: scanning ${nftCount} AzuroBet NFTs...`);

    // Step 2: Enumerate all tokenIds (batch of 10)
    const tokenIds = [];
    for (let i = 0; i < nftCount; i += 10) {
      const batch = [];
      for (let j = i; j < Math.min(i + 10, nftCount); j++) {
        batch.push(
          publicClient.readContract({
            address: contracts.azuroBet,
            abi: erc721Abi,
            functionName: "tokenOfOwnerByIndex",
            args: [account.address, BigInt(j)],
          }),
        );
      }
      tokenIds.push(...(await Promise.all(batch)));
    }

    // Step 3: Check viewPayout for each (parallel)
    const payoutResults = await Promise.all(
      tokenIds.map((tid) =>
        publicClient
          .readContract({
            address: contracts.lp,
            abi: lpAbi,
            functionName: "viewPayout",
            args: [contracts.core, tid],
          })
          .then((p) => ({
            id: tid,
            usd: Number(p) / 10 ** contracts.betTokenDecimals,
            status: "resolved",
          }))
          .catch(() => ({ id: tid, usd: 0, status: "pending" })),
      ),
    );

    const claimable = [];
    let totalPayout = 0;
    let pendingCount = 0;
    for (const r of payoutResults) {
      if (r.status === "pending") {
        pendingCount++;
        continue;
      }
      if (r.usd > 0) {
        claimable.push(r.id);
        totalPayout += r.usd;
      }
    }

    console.log(
      `🔍 Scan: ${nftCount} NFTs, ${claimable.length} claimable ($${totalPayout.toFixed(2)}), ${pendingCount} pending`,
    );

    if (claimable.length === 0) {
      return res.json({
        status: "nothing",
        message: `No claimable payouts. ${pendingCount} bets pending, ${nftCount - pendingCount - claimable.length} lost/empty.`,
        nftCount,
        pendingCount,
      });
    }

    // Step 4: Withdraw payouts
    console.log(
      `💰 Claiming ${claimable.length} bets, total ~$${totalPayout.toFixed(2)}`,
    );

    const { request } = await publicClient.simulateContract({
      account,
      address: contracts.lp,
      abi: lpAbi,
      functionName:
        claimable.length === 1 ? "withdrawPayout" : "withdrawPayouts",
      args:
        claimable.length === 1
          ? [contracts.core, claimable[0]]
          : [contracts.core, claimable],
    });

    const hash = await walletClient.writeContract(request);
    const receipt = await publicClient.waitForTransactionReceipt({ hash });

    const balance = await publicClient.readContract({
      address: contracts.betToken,
      abi: ERC20_ABI,
      functionName: "balanceOf",
      args: [account.address],
    });

    console.log(
      `✅ Auto-claim: ${claimable.length} bets claimed, $${totalPayout.toFixed(2)}, tx=${hash}`,
    );

    res.json({
      status: "ok",
      txHash: hash,
      claimed: claimable.length,
      tokenIds: claimable.map((t) => t.toString()),
      totalPayoutUsd: totalPayout,
      newBalanceUsd: formatUnits(balance, contracts.betTokenDecimals),
      blockNumber: receipt.blockNumber.toString(),
      nftCount,
      pendingCount,
    });
  } catch (e) {
    console.error(`❌ Auto-claim error: ${e.message}`);
    res.status(500).json({ error: e.message, details: e.stack });
  }
});

// ============================================================
// Start
// ============================================================

app.listen(PORT, "127.0.0.1", () => {
  console.log(
    `\n🚀 Azuro Executor sidecar running on http://127.0.0.1:${PORT}`,
  );
  console.log(
    `   Mode: ${DRY_RUN ? "\uD83E\uDDEA DRY-RUN (simulace)" : "\uD83D\uDD25 LIVE (on-chain)"}`,
  );
  console.log(`   Chain: ${chain.name} (${CHAIN_ID})`);
  if (!DRY_RUN) console.log(`   Wallet: ${account.address}`);
  console.log(
    `   Endpoints: /health /bet /cashout /check-cashout /check-payout /claim /my-bets /auto-claim /active-bets\n`,
  );
});
