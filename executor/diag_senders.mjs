// Quick: Check TX senders for known BettorWin transactions
import { createPublicClient, http, fallback } from 'viem';
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

// TX hashes from BettorWin events (from diag_events.mjs output)
const txHashes = [
  '0xaa15a1c2f9236cdfabf2c4f74de270fb3155b0db209ee25219bc097b71b8305d',  // tokenId=222040 $3.60
  '0xf68291af13295787d3912e6a6c78866db4b1f53177ae4c807e2bf6e23d6ec429',  // tokenId=221983 $2.69
  '0x9e27d2875b4a5fb1c47283aa9705f63cc36a4b41906bc7ed48c937f42e93f1c5',  // tokenId=221995 $1.44
  '0x0c70756a66fc8e53001ed2e551ad046b110d41d07e54446f43b14381f9a02d8d',  // tokenId=221980 $2.98
  '0x0185c12e0f1fdf27663f556a516bc0446682b8c0404bcc47d9ef10aa1305961f',  // tokenId=221939 $2.83
  '0xc746797bd3fd649ca1114dd582fc78f020686caf9c7863d1153177207a864a9f',  // tokenId=221976 $3.11
  '0x5bbf0515b641c0c7510478e28e6521714ffd5734a34c5c1822583bb907ba01dc',  // tokenId=221949 $3.12
  '0xc9ade4707f2125d167d943da34868285894edbe55becdb6ec866423db6ec8ac3',  // tokenId=? block=83574267
  '0xbc91ee7449672e5475e9ca0402ec3aa308495217641d0ce137f157ba99985ba3',  // block=83574116
  '0x06933ec76bafd1336aa3f336358a706d94cd78612d05cfea1ecff40a6e1f317a',  // block=83572167
  '0x42dc044bca85235c15c61c7e8e14c1d2a61d06b3c534b03208dbdcfe02ba30af',  // block=83567299
  '0x383b76f5eeff927efdb0a09972bb40688e6f59385ae78a658666ad544b2dc227',  // block=83563716
  '0x807b0ca106465ff333558dbe32b226eab3b883f9221a7836acba130a9c9c4ec9',  // first LP transfer block=83504975
  '0x24c3fe27390561738e2d6c6134f8f3df2f8a4fa6c22443304a4e44f377f89473',  // block=83507484
  '0x94fec5794d3e4f7f0236ebd50bc8c6d29b0b632cdade80eecfc55bf4b105cb3b',  // block=83508215
  '0xc7dfaa8cd5e962ac1a17b15ddd1a17d76df2f51294f3e8b4b7bdb25c679c646c',  // block=83514058
  '0xe14a173571db2cde2d7607614170a697d2501bda75450c2cc9248064b4bceaf0',  // block=83519619
  '0x76f5c1e2143054193830a5d8d2ea78b5d47c9d89949a13591b06d52243787808',  // block=83549720
];

async function main() {
  const out = [];
  const log = (msg) => { console.log(msg); out.push(msg); };
  
  let ourCount = 0, extCount = 0, errCount = 0;
  
  for (const hash of txHashes) {
    try {
      const tx = await pc.getTransaction({ hash });
      const sender = tx.from.toLowerCase();
      const isOurs = sender === wallet;
      if (isOurs) ourCount++; else extCount++;
      log(`${hash.slice(0,14)}... sender=${tx.from} ${isOurs ? '✓ OUR' : '✗ EXTERNAL'} to=${tx.to} method=${tx.input.slice(0,10)}`);
    } catch (e) {
      errCount++;
      log(`${hash.slice(0,14)}... ERROR: ${e.message?.slice(0,60)}`);
    }
  }
  
  log(`\nRESULT: ${ourCount} OUR_WALLET / ${extCount} EXTERNAL / ${errCount} errors (out of ${txHashes.length})`);
  
  fs.writeFileSync('diag_senders.txt', out.join('\n'), 'utf-8');
  log('Saved to diag_senders.txt');
}

main().catch(e => { console.error('FATAL:', e); process.exit(1); });
