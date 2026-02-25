# AKTUALNI_PROGRESS — handoff pro Sonneta

Aktualizováno: **2026-02-25**  
Repo: RustMiskoLive (`C:\RustMiskoLive`)  
Git: **v4.4.0** (f932f2b)

## 🟢 STAV: LIVE PRODUKCE — SYSTÉM FUNGUJE, BALANCE ROSTE

### Čísla
| Metrika | Hodnota |
|---------|---------|
| **USDT Balance** | **$38.74** (bylo $33.77 před 24h) |
| **Sázky dnes** | 5× CS2, $2 stake každá |
| **Výsledky** | 2× Won, 2× Lost, 1× Canceled |
| **Claim stav** | ✅ $10.93 claimováno (tx: 0x07352dd...) |
| **Live matches** | ~100-180 (FlashScore multisport) |
| **Azuro odds** | ~40 kurzů (cs2, football, tennis, basketball) |
| **Fused pairs** | ~19-50 |
| **Příležitosti** | ~50-120 (ARB + score momentum) |

---

### Architektura (PRODUKCE v4.4.0)

```
Chrome Tabs (Tampermonkey)
  ├── flashscore_multisport_scraper.user.js v3.0
  │     → 7 sports: tennis, football, basketball, hockey, esports, baseball, handball
  │     → URL-based sport detection (cs-go/ → cs2, dota-2/ → dota-2) [v4.4.0 FIX]
  ├── tipsport_odds_scraper.user.js v2.1
  │     → ~7-14 kurzů (bookmaker: "tipsport")
  │
  └─── WebSocket ws://127.0.0.1:8080 → Feed Hub
                                          │
                         ┌────────────────┴───────────────────┐
                         │  FEED HUB (Rust, port 8081)        │
                         │  match_key() normalizace            │
                         │  esports→cs2 fallback [v4.4.0 FIX]  │
                         │  Staleness TTL: 120s                │
                         │  gate_odds: liquidity≥500, stale≤10s│
                         └────────────────┬───────────────────┘
                                          │ /state poll 10s
                         ┌────────────────┴───────────────────┐
                         │  ALERT BOT (Rust, background)       │
                         │  find_score_edges() — cs2/tennis    │
                         │  find_odds_anomalies() — ARB        │
                         │  AUTO-BET: edge≥15%, HIGH conf      │
                         │  AUTO-CLAIM: 60s ticker [v4.4.0]    │
                         │  TOKEN_ID: betId discovery [FIX]    │
                         └────────────────┬───────────────────┘
                                          │ POST /bet, /claim
                         ┌────────────────┴───────────────────┐
                         │  EXECUTOR (Node.js, port 3030)      │
                         │  @azuro-org/toolkit LIVE            │
                         │  RPC fallback: 4× Polygon RPC       │
                         │  Wallet: 0x8226D38e...              │
                         │  USDT (USDT0) on Polygon            │
                         └────────────────────────────────────┘
```

---

### Procesy (aktuálně běží)
| Proces | Port | PID | Spuštěn |
|--------|------|-----|---------|
| feed-hub | :8080/:8081 | ~21628 | 19:36 |
| alert_bot | — | ~29076 | 22:xx |
| node (executor) | :3030 | ~36636 | 21:56 |

---

### KRITICKÉ OPRAVY v4.4.0 (2026-02-25)

#### BUG #1 — tokenId vs betId (KRITICKÝ — peníze se nezaobratily!)
- **Problém:** Azuro toolkit.getBet() vrací `betId: 220860` (číslo), alert_bot hledal `tokenId` (string)
- **Důsledek:** Všechny sázky byly "Settled" na chainu, ale alert_bot to neviděl → nezclaimoval
- **Fix:** Obě cesty (cashout + claim) nyní čtou `betId` s u64→string konverzí

#### BUG #2 — State "Settled" nerozpoznán
- **Problém:** is_settled kontroloval jen "Resolved"/"Canceled", Azuro vrací "Settled"
- **Fix:** Přidáno "Settled" do match armu

#### BUG #3 — Startup recovery s "?" tokenId
- **Problém:** pending_claims.txt ukládal "?" jako tokenId → po restartu se "?" načetl jako validní → PATH A failoval
- **Fix:** "?" nebo prázdný string → None → PATH B discovery

#### BUG #4 — esports ↔ cs2 sport mismatch (silently dropped CS2 matches!)
- **Problém:** FlashScore posílá sport="esports", Azuro má sport="cs2" → match_key nikdy neodpovídal
- **Fix A:** feed_hub fuse loop zkouší esports_alts = ["cs2","dota-2","league-of-legends","valorant"]
- **Fix B:** FlashScore scraper detectSportFromURL() kontroluje /cs-go/, /dota-2/ PŘED /esports/

#### BUG #5 — RPC reliability
- **Fix:** executor/index.js používá viem `fallback([4× Polygon RPC])` s rank=true

---

### Konfigurace (LIVE)
```bash
feed-hub:   FEED_DB_PATH=data/feed.db
alert_bot:  TELEGRAM_BOT_TOKEN=7611316975:AAG_bStGX283uHCdog96y07eQfyyBhOGYuk
            TELEGRAM_CHAT_ID=6458129071
            FEED_HUB_URL=http://127.0.0.1:8081
            EXECUTOR_URL=http://127.0.0.1:3030
executor:   PRIVATE_KEY=0x34fb468...  (Polygon USDT wallet)
            CHAIN_ID=137
```

---

### Datové soubory
| Soubor | Obsah | Stav |
|--------|-------|------|
| `data/bet_history.txt` | 5 sázek (dedup ochrana) | ✅ |
| `data/pending_claims.txt` | vyčištěno po claimu | ✅ prázdný |
| `logs/2026-02-25.jsonl` | aplikační logy | — |

---

### Auto-bet konfigurace
```rust
AUTO_BET_ENABLED = true
AUTO_BET_STAKE = 2.0  // $2 per bet
AUTO_BET_MIN_EDGE_PCT = 15.0  // min 15% edge
AUTO_BET_MIN_ODDS = 1.15
AUTO_BET_MAX_ODDS = 3.50
AUTO_BET_MAX_PER_SESSION = 10
CASHOUT_CHECK_SECS = 30
CLAIM_CHECK_SECS = 60
```

---

### Známé problémy / Sledovat
- Chrome tabs musí být otevřeny manuálně po restartu PC
- FlashScore "esports" tab na general URL stále posílá fotbalové/basketbalové týmy jako esports
  → Řešení: otevřít specificky `flashscore.com/esports/cs-go/` pro CS2 data
- Fused=50 = živý count opportunities, ne hard cap

---

### NEXT STEPS (viz EDGE_NAPADY.md + IMPLEMENTACNI_ROADMAP.md)
1. **Fortuna.cz scraper** — okamžitě
2. **Football score model** v alert_bot
3. **Betfair Exchange scraper** — velká likvidita


