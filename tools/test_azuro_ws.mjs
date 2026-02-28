/**
 * Quick test: Azuro WebSocket Conditions Stream
 * 
 * Per official docs (gem.azuro.org/hub/apps/guides/advanced/live/data-retrieval):
 *   Production: wss://streams.azuro.org/v1/streams/conditions
 *   Dev:        wss://dev-streams.azuro.org/v1/streams/conditions
 * 
 * Protocol:
 *   Subscribe:   { action: "subscribe", conditionIds: ["123456"] }
 *   Unsubscribe: { action: "unsubscribe", conditionIds: ["123456"] }
 *   Response:    Array<{ id, state?, margin, reinforcement, winningOutcomesCount, outcomes? }>
 * 
 * Steps:
 * 1. Fetch live conditionIDs from Azuro data-feed subgraph
 * 2. Connect to WS endpoint  
 * 3. Subscribe to those conditions
 * 4. Log all received messages for 30s
 */

import { createRequire } from "module";
const require = createRequire(import.meta.url);
const WebSocket = require("../executor/node_modules/ws");

// === All URL variants to try ===
const WS_URLS = [
  // Official docs say /conditions
  "wss://streams.azuro.org/v1/streams/conditions",
  // onchainfeed variant (connected before with /feed)
  "wss://streams.onchainfeed.org/v1/streams/feed",
  "wss://streams.onchainfeed.org/v1/streams/conditions",
  // azuro.org with /feed
  "wss://streams.azuro.org/v1/streams/feed",
  // preprod 
  "wss://preprod-streams.azuro.org/v1/streams/conditions",
  // Without /v1
  "wss://streams.azuro.org/streams/conditions",
  // V3 might have different path
  "wss://streams.azuro.org/v1/conditions",
];

const SUBGRAPH_URL = "https://thegraph-1.onchainfeed.org/subgraphs/name/azuro-protocol/azuro-data-feed-polygon";
const TEST_DURATION_MS = 30_000;

// Step 1: Fetch live conditionIDs from subgraph
async function fetchLiveConditionIds() {
  const now = Math.floor(Date.now() / 1000);
  const query = `{
    games(first: 5, where: { state_in: ["Live"], sport_: { slug: "cs2" }, startsAt_gte: "${now - 86400}" }) {
      id title state
      participants(orderBy: sortOrder) { name }
      conditions(first: 10, where: { state_in: ["Active"] }) {
        id state
        outcomes(orderBy: sortOrder) { id currentOdds }
      }
    }
  }`;

  console.log("[SUBGRAPH] Fetching live CS2 conditions...");
  const resp = await fetch(SUBGRAPH_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ query }),
  });
  const json = await resp.json();
  
  const condIds = [];
  const games = json?.data?.games || [];
  console.log(`[SUBGRAPH] Found ${games.length} live CS2 game(s)`);
  
  for (const g of games) {
    const teams = (g.participants || []).map(p => p.name).join(" vs ");
    console.log(`  Game: ${teams} (${g.id}) state=${g.state}`);
    for (const c of (g.conditions || [])) {
      condIds.push(c.id);
      const odds = (c.outcomes || []).map(o => o.currentOdds).join("/");
      console.log(`    Condition ${c.id}: odds=${odds}`);
    }
  }

  // Also try other sports if CS2 has nothing
  if (condIds.length === 0) {
    console.log("\n[SUBGRAPH] No CS2 live — trying all sports...");
    const q2 = `{
      games(first: 5, where: { state_in: ["Live"], startsAt_gte: "${now - 86400}" }) {
        id title state
        participants(orderBy: sortOrder) { name }
        conditions(first: 5, where: { state_in: ["Active"] }) {
          id state
          outcomes(orderBy: sortOrder) { id currentOdds }
        }
      }
    }`;
    const resp2 = await fetch(SUBGRAPH_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ query: q2 }),
    });
    const json2 = await resp2.json();
    const games2 = json2?.data?.games || [];
    console.log(`[SUBGRAPH] Found ${games2.length} live game(s) across all sports`);
    for (const g of games2) {
      const teams = (g.participants || []).map(p => p.name).join(" vs ");
      console.log(`  Game: ${teams} (${g.id})`);
      for (const c of (g.conditions || [])) {
        condIds.push(c.id);
      }
    }
  }

  return condIds;
}

// Step 2: Test each WS endpoint
async function testWsEndpoint(url, conditionIds) {
  return new Promise((resolve) => {
    console.log(`\n${"=".repeat(60)}`);
    console.log(`[WS TEST] ${url}`);
    console.log(`[WS TEST] Subscribing to ${conditionIds.length} condition(s)...`);
    console.log(`${"=".repeat(60)}`);
    
    let msgCount = 0;
    let firstMsgAt = null;
    const startTime = Date.now();
    
    let ws;
    try {
      ws = new WebSocket(url);
    } catch (e) {
      console.log(`[ERROR] Cannot connect: ${e.message}`);
      resolve({ url, connected: false, messages: 0 });
      return;
    }

    const timeout = setTimeout(() => {
      console.log(`[TIMEOUT] 10s — closing.`);
      ws.close();
      resolve({ url, connected: true, messages: msgCount, firstMsgMs: firstMsgAt ? firstMsgAt - startTime : null });
    }, 10_000);

    ws.on("open", () => {
      console.log(`[CONNECTED] ✅ Open!`);

      // Subscribe per official docs format
      if (conditionIds.length > 0) {
        const sub = { action: "subscribe", conditionIds: conditionIds.slice(0, 20) };
        console.log(`[SEND] ${JSON.stringify(sub).substring(0, 300)}`);
        ws.send(JSON.stringify(sub));
      } else {
        console.log(`[WARN] No conditionIds to subscribe to — testing connection only`);
      }
      
      // Also try ping
      ws.ping();
    });

    ws.on("message", (data) => {
      msgCount++;
      if (!firstMsgAt) firstMsgAt = Date.now();
      const str = data.toString().substring(0, 1500);
      console.log(`[MSG #${msgCount}] ${str}\n`);
    });

    ws.on("pong", () => {
      console.log(`[PONG] ✅ Server alive`);
    });

    ws.on("error", (err) => {
      console.log(`[ERROR] ${err.message}`);
      clearTimeout(timeout);
      resolve({ url, connected: false, messages: 0, error: err.message });
    });

    ws.on("close", (code, reason) => {
      console.log(`[CLOSED] Code=${code} Reason=${reason?.toString() || "none"}`);
      clearTimeout(timeout);
      resolve({ url, connected: true, messages: msgCount, firstMsgMs: firstMsgAt ? firstMsgAt - startTime : null });
    });
  });
}

// Main
async function main() {
  console.log("╔══════════════════════════════════════════════════════╗");
  console.log("║  AZURO WS STREAM TEST — Official Docs Protocol     ║");
  console.log("╚══════════════════════════════════════════════════════╝\n");

  const conditionIds = await fetchLiveConditionIds();
  console.log(`\n[READY] Got ${conditionIds.length} conditionId(s) to subscribe to.\n`);

  const results = [];
  for (const url of WS_URLS) {
    const r = await testWsEndpoint(url, conditionIds);
    results.push(r);
  }

  console.log("\n╔══════════════════════════════════════════════════════╗");
  console.log("║  RESULTS SUMMARY                                   ║");
  console.log("╚══════════════════════════════════════════════════════╝");
  for (const r of results) {
    const status = r.connected ? (r.messages > 0 ? "✅ WORKS" : "⚠️ Connected, 0 msgs") : "❌ Failed";
    console.log(`  ${status} | ${r.url}`);
    if (r.messages > 0) console.log(`    → ${r.messages} messages, first after ${r.firstMsgMs}ms`);
    if (r.error) console.log(`    → Error: ${r.error}`);
  }
}

main().catch(e => { console.error(e); process.exit(1); });
