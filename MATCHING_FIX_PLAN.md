# MATCHING FIX PLAN — 2026-03-01

## STATUS: ✅ IMPLEMENTED (commit f7bff50 + b86acf1)

Všech 5 fází implementováno a nasazeno v produkci.

## Problém (VYŘEŠEN)
- 18% fusion miss rate (13/71 odds klíčů nemá live score)
- Příčiny: sport label (`hockey` vs `ice-hockey`), diakritika (`Nový Zéland` → `novýzéland`), chybějící sufixy (`utd`), český překlad zemí

## Fáze 1: OBSERVABILITA (baseline 1-2h)
### 1.1 FUSION_MISS log v heartbeat
- Každých 10s: pro každý odds_key bez live_key → logovat:
  - `FUSION_MISS { odds_key, sport, source, league, odds_ts, raw_t1, raw_t2, norm_t1, norm_t2, top3_candidates }`
- Kde: `feed_hub.rs` heartbeat task

### 1.2 NORM_TRACE + kill-switch + sampling
- Env `FF_NORM_TRACE=true` (default OFF)
- Sampling: counter-based, 1 z 20 (5%) ingestů loguje raw→normalized chain
- `NORM_TRACE { source, sport, raw_t1, raw_t2, norm_t1, norm_t2, match_key }`

## Fáze 2: ZERO-RISK OPRAVY
### 2.1 `normalize_sport()` v `match_key()`
- `hockey` → `ice-hockey` (FlashScore, Tipsport, Chance posílají `hockey`)
- `lol` → `league-of-legends`
- `csgo` → `cs2`
- `esport` → `esports`

### 2.2 Unicode NFKD strip v `normalize_name()`
- Přidat `unicode-normalization` crate
- NFKD decomposition → strip combining marks (Combining_Mark)
- `é→e`, `ö→o`, `ü→u`, `á→a`, `ý→y`, `ž→z`, `č→c`, `ř→r`, `š→s`

### 2.3 alert_bot.rs L1092 Unicode fix
- `is_ascii_alphanumeric()` → `is_alphanumeric()` (aby nestrippoval Unicode znaky pro porovnání)

### 2.4 `translate_country_name()` — Czech→English
- Separátní funkce, NE v normalize_name
- ~30 mapování: `novýzéland→newzealand`, `čína→china`, `japonsko→japan`, ...
- Volat PŘED normalize_name v match_key

## Fáze 3: SUFFIX + TOKEN-SUBSET (medium risk)
### 3.1 Extended suffixes + FF_EXTENDED_SUFFIX_STRIP
- Přidat: `utd`, `town`, `youth`, `npl`, `reserves`, `junior`, `afc`, `bfc`
- Kill-switch env `FF_EXTENDED_SUFFIX_STRIP` (default ON)

### 3.2 Token-subset pair matching
- Kill-switch `FF_TOKEN_SUBSET_PAIR_ALIAS` (default ON)
- Guardrails:
  - OBA týmy musí matchnout simultánně
  - Každý tým ≥2 smysluplné tokeny po stripu
  - Zakázat match pokud overlap jen na ultra-common tokenech (fc/sc/afc/club/united/city)
- Alias cache: HashMap s TTL 12h + cap 1000 entries (LRU eviction)
- Log: `FUZZY_ALIAS { from_key, to_key, method, token_overlap }`

## Fáze 4: STRATEGY TUNING
- `ALERT_COOLDOWN_SECS` 45→90
- `SCORE_EDGE_COOLDOWN_SECS` 30→60
- `AUTO_BET_MIN_MARKET_SOURCES` 2→3
- `MIN_EDGE_PCT` 5.0→8.0
- `MAX_ODDS_AGE_SECS` 20→12
- `MAX_CONCURRENT_PENDING: usize = 8` (nový)
- Loss streak cooldown: 3 consecutive LOST → 300s pause (nový)
- Min bankroll guard: skip auto-bet if bankroll < $20 (nový)

## Fáze 5: BUILD & DEPLOY
- Git commit per fáze
- `cargo build --release`
- Restart via `start_system.ps1`
- Verify: FUSION_MISS logging, sport aliases, WS gate ON

## Metriky po 48h
- Fusion miss count < 5%
- Fuzzy alias hits > 10/day
- ConditionNotRunning rate ↓
- Alert count/day ↓ 50%+
- Win rate > 50%
- Net P/L > 0
