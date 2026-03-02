# Schema Map (Feed Hub ingest)

Tento dokument popisuje **kanonické schéma zpráv**, které ingestuje `feed-hub`, a konkrétně mapuje **aktuálně používané** browser scrapers (userscripts) na tyto zprávy.

Poznámky:
- **Flashscore scrapers nejsou součástí** (nepoužíváme je).
- Chance ingest je řešen userscriptem `FIXED_chance_scraper_v2.user.js`.

---

## 1) Transporty a endpointy

### 1.1 WebSocket ingest
- Endpoint: `ws://<FEED_HUB_BIND>/feed`
- Default bind: `FEED_HUB_BIND=0.0.0.0:8080`

`feed-hub` očekává WS text message jako JSON envelope.

### 1.2 HTTP ingest (Fortuna)
- Endpoint: `http://<FEED_HTTP_BIND>/fortuna`
- Default bind: `FEED_HTTP_BIND=127.0.0.1:8081`

Tohle je výjimka: Fortuna neposílá WS envelope, ale vlastní JSON se seznamem zápasů.

---

## 2) Kanonické WS envelope schéma

Každá WS zpráva má tvar:

```json
{
  "v": 1,
  "type": "live_match" | "odds" | "heartbeat",
  "source": "string",
  "ts": "2026-03-02T02:22:33.123Z",
  "payload": { }
}
```

- `v`: verze envelope (aktuálně `1`)
- `type`: typ zprávy
- `source`: identifikátor klienta / scraperu (např. `chance`, `tipsport`, `dust2`, `hltv-odds`)
- `ts`: ISO timestamp (doporučeno posílat vždy)
- `payload`: objekt dle `type`

---

## 3) Kanonické payloady

### 3.1 `type: "live_match"`

```json
{
  "sport": "cs2" | "tennis" | "football" | "basketball" | "hockey" | "dota-2" | "league-of-legends" | "valorant" | "esports" | "...",
  "team1": "string",
  "team2": "string",
  "score1": 0,
  "score2": 0,
  "detailed_score": "optional string",
  "status": "optional string",
  "url": "optional string"
}
```

Poznámky:
- `score1/score2` jsou optional (u některých zdrojů nejsou vždy dostupné), ale pro score-edge logiku jsou kritické.
- `detailed_score` se používá jako doplňkový kontext (mapa/set/period apod.).

### 3.2 `type: "odds"`

```json
{
  "sport": "string",
  "bookmaker": "string",
  "market": "match_winner",
  "team1": "string",
  "team2": "string",
  "odds_team1": 1.95,
  "odds_team2": 1.95,
  "liquidity_usd": 5000.0,
  "spread_pct": 0.8,
  "url": "optional string",

  "game_id": "optional string",
  "condition_id": "optional string",
  "outcome1_id": "optional string",
  "outcome2_id": "optional string",
  "chain": "optional string"
}
```

Poznámky:
- `liquidity_usd` a `spread_pct` jsou **gating signály** (feed-hub je používá v párování + filtrování/noise-control).
- `game_id/condition_id/outcome*_id/chain` se objevují u on-chain/Azuro zdrojů (tady je uvádím kvůli kompletnosti payloadu; userscripty je většinou neposílají).

### 3.3 `type: "heartbeat"`

```json
{
  "payload": {}
}
```

Používá se pro „živost“ zdrojů a observability.

---

## 4) Fortuna HTTP schéma (POST /fortuna)

Fortuna scraper posílá JSON:

```json
{
  "timestamp": 1700000000000,
  "source": "fortuna",
  "matches": [
    {
      "sport": "football",
      "league": "optional string",
      "team1": "string",
      "team2": "string",
      "score1": 0,
      "score2": 0,
      "status": "optional string",
      "odds": [
        { "market": "optional string", "label": "optional string", "value": 1.95 }
      ]
    }
  ]
}
```

`feed-hub` z toho interně udělá:
- `live_match` (pokud jsou score/status)
- `odds` pro `bookmaker="fortuna"`, `market="match_winner"` (pokud najde 1/2 ceny)

Server-side safety net:
- existuje sanitizace podezřelného skóre (typicky pro football), aby to neničilo score-edge logiku.

---

## 5) Mapování: userscripts → feed-hub

Níže jsou jen zdroje, které se teď reálně používají.

### 5.1 Chance live scraper
Soubor: `userscripts/FIXED_chance_scraper_v2.user.js`

WS `source`: typicky `chance` (dle skriptu)

Posílá:
- `type: "odds"`
  - `bookmaker: "chance"`
  - `market: "match_winner"`
  - `sport`: multi-sport (včetně esportů jako `cs2`, `dota-2`, `league-of-legends`, `valorant`)
  - **Pozn.:** 1X2 (3-way) trhy se **bezpečně skipují** (feed-hub dnes očekává 2-way `odds_team1/odds_team2`).
- `type: "live_match"`
  - `score1/score2` + občas `detailed_score`
- `type: "heartbeat"`

### 5.2 Tipsport odds scraper
Soubor: `userscripts/tipsport_odds_scraper.user.js`

WS `source`: `tipsport`

Posílá (LIVE-ONLY):
- `type: "odds"`
  - `bookmaker: "tipsport"`, `market: "match_winner"`
- `type: "live_match"`
  - `detailed_score` jako plný string (pokud dostupné)
- `type: "heartbeat"`

Navíc má zdrojovou score sanity kontrolu podle sportu (snižuje noise ještě před feed-hubem).

### 5.3 Dust2 live scraper (CS2 score)
Soubor: `userscripts/dust2_live_scraper.user.js`

WS `source`: `dust2` (dle skriptu)

Posílá:
- `type: "live_match"`
  - `sport: "cs2"`
  - `detailed_score`: round/map info
- `type: "heartbeat"`

### 5.4 HLTV live scraper (CS2 live + featured odds)
Soubor: `userscripts/hltv_live_scraper.user.js`

WS `source`: typicky `hltv` pro live + `hltv-odds` pro odds (podle skriptu)

Posílá:
- `type: "live_match"` (CS2)
- `type: "odds"`
  - bookmaker detekce (`20bet`, `ggbet`, …) nebo fallback
  - posílá `spread_pct` + často i fixní `liquidity_usd`
- `type: "heartbeat"`

### 5.5 CS2 odds scraper (bo3.gg / odds pages)
Soubor: `userscripts/odds_scraper.user.js`

WS `source`: dle skriptu (např. `odds-scraper` apod.)

Posílá:
- `type: "odds"` pro `sport:"cs2"`
  - `market:"match_winner"`
  - `bookmaker`: dle zdroje (např. `1xbit`)
  - typicky přidává `spread_pct` + `liquidity_usd`
- `type: "heartbeat"`

### 5.6 Fortuna live scraper (HTTP)
Soubor: `userscripts/fortuna_live_scraper.user.js`

Transport:
- posílá na `http://127.0.0.1:8081/fortuna` (tj. `FEED_HTTP_BIND` port)

Obsah:
- `matches[]` obsahuje jak score/status, tak odds array.
- feed-hub to rozkládá do kanonických `live_match` + `odds` stavů.

---

## 6) Praktické minimum pro „validní ingest“

Když chceš rychle ověřit, že feed-hub ingestuje správně, minimální validní set je:
- 1× `live_match` pro konkrétní `sport/team1/team2` (score může být 0-0)
- 1× `odds` pro stejné `sport/team1/team2` a `market:"match_winner"`

Jakmile se to spáruje, objeví se to v:
- `GET /state` (live + odds)
- `GET /opportunities` (pokud model/gating vyhodnotí edge)
