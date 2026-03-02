// Fetch and decode Polygon USDT (ERC20) Approval logs for a given owner.
// Requires Etherscan API v2 key (works for Polygon via chainid=137).
// Usage:
//   $env:ETHERSCAN_API_KEY='...'
//   node tools/polygon_usdt_approvals.mjs --owner 0x...
// Optional:
//   --fromBlock 0 --toBlock 99999999 --usdt 0x... --apiKey ... --json

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (!a.startsWith('--')) continue;
    const key = a.slice(2);
    const next = argv[i + 1];
    if (next && !next.startsWith('--')) {
      args[key] = next;
      i += 1;
    } else {
      args[key] = true;
    }
  }
  return args;
}

function asAddress(addr) {
  if (!addr) return null;
  const s = String(addr).trim();
  if (!s.startsWith('0x')) return null;
  if (s.length !== 42) return null;
  return s;
}

function padTopicAddress(addr) {
  // topic is 32 bytes, right-aligned address.
  const a = addr.toLowerCase().replace(/^0x/, '');
  return '0x' + a.padStart(64, '0');
}

function topicToAddress(topic) {
  const t = String(topic).toLowerCase();
  if (!t.startsWith('0x')) return null;
  if (t.length !== 66) return null;
  return '0x' + t.slice(26);
}

function hexToBigInt(hex) {
  if (!hex) return 0n;
  const h = String(hex);
  if (!h.startsWith('0x')) return 0n;
  return BigInt(h);
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

async function etherscanGet(url) {
  const res = await fetch(url, { method: 'GET' });
  const text = await res.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    throw new Error(`Etherscan response is not JSON (HTTP ${res.status}): ${text.slice(0, 200)}`);
  }
  return json;
}

function makeUrl({ apiKey, usdt, ownerTopic, fromBlock, toBlock, page, offset }) {
  const base = new URL('https://api.etherscan.io/v2/api');
  base.searchParams.set('chainid', '137');
  base.searchParams.set('module', 'logs');
  base.searchParams.set('action', 'getLogs');
  base.searchParams.set('fromBlock', String(fromBlock));
  base.searchParams.set('toBlock', String(toBlock));
  base.searchParams.set('address', usdt);

  // Approval(address indexed owner, address indexed spender, uint256 value)
  base.searchParams.set('topic0', '0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925');
  base.searchParams.set('topic0_1_opr', 'and');
  base.searchParams.set('topic1', ownerTopic);

  base.searchParams.set('page', String(page));
  base.searchParams.set('offset', String(offset));
  base.searchParams.set('sort', 'asc');

  base.searchParams.set('apikey', apiKey);
  return base.toString();
}

function normalizeResult(result) {
  if (!result) return [];
  if (Array.isArray(result)) return result;
  if (typeof result === 'string') {
    const s = result.toLowerCase();
    if (s.includes('no records')) return [];
  }
  return [];
}

function formatUSDT(value, decimals) {
  const base = 10n ** BigInt(decimals);
  const whole = value / base;
  const frac = value % base;
  return `${whole.toString()}.${frac.toString().padStart(decimals, '0')}`;
}

function formatValue(value) {
  // USDT is 6 decimals, but allowance is raw uint256; show both raw and exact decimal.
  const raw = value.toString();
  const exact = formatUSDT(value, 6);
  return { raw, approxUSDT: exact };
}

async function main() {
  const args = parseArgs(process.argv.slice(2));

  const owner = asAddress(args.owner);
  if (!owner) {
    console.error('Chybí nebo je neplatný --owner 0x...');
    process.exit(2);
  }

  const apiKey = String(args.apiKey || process.env.ETHERSCAN_API_KEY || '').trim();
  if (!apiKey) {
    console.error('Chybí API key: dej --apiKey ... nebo env ETHERSCAN_API_KEY');
    process.exit(2);
  }

  const usdt = asAddress(args.usdt) || '0xc2132D05D31c914a87C6611C10748AEb04B58e8F';
  const fromBlock = args.fromBlock ? BigInt(args.fromBlock) : 0n;
  const toBlock = args.toBlock ? BigInt(args.toBlock) : 999999999n;

  const ownerTopic = padTopicAddress(owner);

  const offset = args.offset ? Number(args.offset) : 1000;
  const maxPages = args.maxPages ? Number(args.maxPages) : 200;

  const logs = [];
  let page = 1;
  let rateSleeps = 0;

  while (page <= maxPages) {
    const url = makeUrl({ apiKey, usdt, ownerTopic, fromBlock, toBlock, page, offset });
    const r = await etherscanGet(url);

    const status = String(r.status || '');
    const message = String(r.message || '');

    if (status === '0') {
      const lower = (message + ' ' + String(r.result || '')).toLowerCase();
      if (lower.includes('rate limit')) {
        rateSleeps += 1;
        const ms = Math.min(1500 * rateSleeps, 15000);
        await sleep(ms);
        continue;
      }

      const normalized = normalizeResult(r.result);
      if (normalized.length === 0) break;
    }

    const pageLogs = normalizeResult(r.result);
    if (pageLogs.length === 0) break;

    for (let i = 0; i < pageLogs.length; i += 1) {
      logs.push(pageLogs[i]);
    }

    if (pageLogs.length < offset) break;
    page += 1;

    // small pacing to be gentle
    await sleep(150);
  }

  const decoded = [];
  for (let i = 0; i < logs.length; i += 1) {
    const l = logs[i];
    const topics = l.topics || [];
    const spender = topics[2] ? topicToAddress(topics[2]) : null;
    const value = hexToBigInt(l.data);
    decoded.push({
      blockNumber: Number(l.blockNumber),
      timeStamp: Number(l.timeStamp),
      txHash: l.transactionHash,
      spender,
      value,
    });
  }

  decoded.sort((a, b) => (a.blockNumber < b.blockNumber ? -1 : 1));

  const bySpender = new Map();
  for (let i = 0; i < decoded.length; i += 1) {
    const ev = decoded[i];
    const key = ev.spender || '0x(unknown)';
    const row = bySpender.get(key) || { count: 0, first: ev, last: ev };
    row.count += 1;
    if (!row.first || ev.blockNumber < row.first.blockNumber) row.first = ev;
    if (!row.last || ev.blockNumber >= row.last.blockNumber) row.last = ev;
    bySpender.set(key, row);
  }

  const spenders = Array.from(bySpender.entries()).map(([spender, row]) => {
    const lastFmt = formatValue(row.last.value);
    return {
      spender,
      approvals: row.count,
      firstBlock: row.first.blockNumber,
      lastBlock: row.last.blockNumber,
      lastTx: row.last.txHash,
      lastValueRaw: lastFmt.raw,
      lastValueApproxUSDT: lastFmt.approxUSDT,
      lastIsZero: row.last.value === 0n,
    };
  });

  spenders.sort((a, b) => (a.lastBlock < b.lastBlock ? 1 : -1));

  const out = {
    ts: new Date().toISOString(),
    chain: 'polygon',
    token: { symbol: 'USDT', address: usdt },
    owner,
    query: {
      fromBlock: fromBlock.toString(),
      toBlock: toBlock.toString(),
      pagesFetched: page,
      offset,
      events: decoded.length,
      uniqueSpenders: spenders.length,
    },
    spenders,
  };

  const wantJson = Boolean(args.json);
  if (wantJson) {
    console.log(JSON.stringify(out, null, 2));
    return;
  }

  console.log(`owner=${owner}`);
  console.log(`usdt=${usdt}`);
  console.log(`events=${decoded.length} uniqueSpenders=${spenders.length}`);
  console.log('---');

  for (let i = 0; i < spenders.length; i += 1) {
    const s = spenders[i];
    const flag = s.lastIsZero ? 'REVOKED(0)' : 'ACTIVE';
    console.log(`${flag} spender=${s.spender} approvals=${s.approvals} lastBlock=${s.lastBlock} lastValueRaw=${s.lastValueRaw} ~USDT=${s.lastValueApproxUSDT}`);
    console.log(`  lastTx=${s.lastTx}`);
  }

  console.log('---');
  console.log('Tip: přidej --json pro plný JSON výstup.');
}

main().catch((e) => {
  console.error('ERROR:', e && e.message ? e.message : String(e));
  process.exit(1);
});
