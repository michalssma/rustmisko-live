# AKTUALNI_PROGRESS

Aktualizováno: **2026-02-26 14:27**
Repo: `C:\RustMiskoLive`

## Source of truth (teď)

Tento soubor je jediný „live" přehled stavu. Ostatní strategické `.md` ber jako plán/historii.

## Runtime stav (ověřeno 14:27)

- Executor `/health`: `status=ok`, chain `Polygon`, wallet `0x8226D38e5c69c2f0a77FBa80e466082B410a8F00`
- **USDT balance: $36.16** (po clainu $13.81 stuck funds)
- AzuroBet NFTs: **34 celkem** (28 pending, 6 lost, 0 claimable)
- Procesy běží: `feed-hub`, `alert-bot` (PID 28724, nový build), `node` (executor PID 24656)

## Dnešní opravy (2026-02-26)

### CRITICAL: Stale subgraph fix

- Executor používal mrtvý subgraph (`thegraph.azuro.org`) — data z května 2025!
- `/auto-claim` a `/my-bets` přepsány na **on-chain NFT enumeration**
- Žádná subgraph závislost — čte přímo z AzuroBet + LP kontraktů

### Claimed $13.81 stuck funds

- 3 AzuroBet NFTs (221127=$1.26, 221198=$10.89, 221199=$1.66) claimnuty
- Balance: $22.35 → $36.16

### Alert-bot v4.5 rewrite

- Prematch odds anomaly auto-bet **DISABLED** → pouze alert
- Esports **map_winner guard** — CS2/Dota/Valorant/LoL auto-bet pouze na map_winner
- Session limit odstraněn — neomezené bety
- Per-condition dedup + inflight race condition guard

### Executor fixes

- `autostart.bat`: opraveno `executor.js` → `index.js`
- `start_system.ps1`: opraveno `alert-bot.exe` → `alert_bot.exe`, přidán PRIVATE_KEY

## Aktuální chování systému

- Auto-bet: **LIVE score edges only**, map_winner pro esports
- Prematch odds anomaly: alert na Telegram, bez auto-betu
- Auto-claim: každých 60s on-chain scan (34 NFTs → viewPayout check)
- Dedup: per-condition (různé mapy na stejný match povoleny)
- Stake: $2 (score edge), $1 (odds anomaly — ale disabled)

## Git stav

Rozpracované změny k commitu — viz git diff.
