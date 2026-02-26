# CONTEXT

Aktualizováno: **2026-02-26 14:27**

## Co projekt dělá

RustMiskoLive je lokální automatizační stack pro sběr live kurzů/skóre, detekci edge příležitostí a exekuci sázek přes Azuro executor na Polygon chain.

## Aktivní komponenty

- `feed-hub` (Rust): ingest WS feedů + HTTP `/state` a `/opportunities`
- `alert-bot` (Rust): alerting, auto-bet logika (LIVE score edges only), cashout/claim orchestrace
- `executor` (Node.js): endpointy `/bet`, `/cashout`, `/check-payout`, `/claim`, `/my-bets`, `/auto-claim`
  - **`/my-bets` a `/auto-claim` — ON-CHAIN NFT enumeration** (žádná subgraph závislost!)
- `userscripts/tipsport_odds_scraper.user.js` (v2.3): Tipsport odds/live feed

## Auto-bet strategie (v4.5)

- **LIVE score edges** → auto-bet (naše skutečná výhoda — vidíme skóre dřív než Azuro)
  - Esports (CS2/Dota/Valorant/LoL): **pouze map_winner** (match_winner na BO3 je příliš riskantní)
  - Tradiční sporty: match_winner povoleno
- **Prematch odds anomaly** → alert only, žádný auto-bet
- **Per-condition dedup** — nikdy dva bety na stejnou condition
- **Inflight lock** — race condition ochrana při čekání na executor odpověď
- **Žádný session limit** — neomezený počet betů

## Ověřené prostředí

- Chain: Polygon (`137`)
- Bet token: USDT (`0xc2132D05D31c914a87C6611C10748AEb04B58e8F`)
- Wallet: `0x8226D38e5c69c2f0a77FBa80e466082B410a8F00`
- AzuroBet NFT: `0x7A1c3FEf712753374C4DCe34254B96faF2B7265B`
- Core: `0xF9548Be470A4e130c90ceA8b179FCD66D2972AC7`
- LP: `0x0FA7FB5407eA971694652E6E16C12A52625DE1b8`

## Plánované rozšíření

- Tampermonkey scraper pro **1xbit** (LIVE sekce všech sportů)
- Tampermonkey scraper pro **Fortuna**
- **HLTV** scraper pro CS2 data
- Rozšíření na tenis (set_winner), basketball (quarter logic), další sporty

## Důležité pravidlo pro dokumentaci

- Aktuální čísla (balance, pending, procesy) drž pouze v `AKTUALNI_PROGRESS.md`.
- Ostatní `.md` používej jako strategii/plán, ne jako live telemetry.
