# WIN11 Persistent Browser PoC Checklist

Aktualizováno: 2026-02-24
Cíl: Na domácím Win11 nodu držet 24/7 přihlášený browser, kontinuálně sbírat live data a ověřit Rust feed fusion.

---

## 1) Host hardening (Win11)

- [ ] Power plan: never sleep, never hibernate
- [ ] Disable automatic restart during active hours
- [ ] Stable network mode (prefer kabel/Wi-Fi s fixní kvalitou)
- [ ] Auto-login + startup sequence po rebootu
- [ ] Základní monitoring procesů (browser + Rust listener)

## 2) Persistent browser runtime

- [ ] Vybraný primární browser profil (Edge/Chrome)
- [ ] Dedicated profile pouze pro ingest (bez běžného surfování)
- [ ] Startup tabs definovány (live stats + odds stránky)
- [ ] Session persistence ověřena po restartu stroje
- [ ] Jednorázové ruční přihlášení hotové na všech kritických stránkách

## 3) Source inventory (musí být explicitní)

Pro každý zdroj vyplnit:

- Název zdroje
- Typ dat (live score / live odds / oboje)
- Sporty
- URL
- Login required (ano/ne)
- Priorita (A/B/C)
- Fallback zdroj

Minimální požadavek:

- [ ] 2 nezávislé score feedy na klíčový sport
- [ ] 2 odds feedy na cílové trhy

## 4) Rust fusion proof

- [ ] Browser feed dorazí do Rust listeneru bez výpadků
- [ ] Match identity normalizace funguje (team aliases + deduplikace)
- [ ] V dashboard/logu vidíme: "co je live" + "kde je live odds"
- [ ] Replay log se ukládá průběžně

## 5) 24h acceptance test

- [ ] Uptime feedu >= 98%
- [ ] p95 lag < 2s
- [ ] Consensus >= 80%
- [ ] False join rate < 5%
- [ ] Žádný kritický crash bez auto-recovery

## 6) Stop/Go pravidlo

STOP (neskalovat):

- Uptime pod 98%
- p95 lag >= 2s
- Consensus pod 80%
- Opakované mapovací chyby

GO (lze řešit další zařízení/profit vrstvu):

- Všechny acceptance metriky splněné alespoň 24h kontinuálně

---

## Operativní poznámka

Cíl 2k USD/měsíc je pracovní target, ne garance. Rozhodnutí o navýšení kapitálu se dělá až podle net výsledků po fees/slippage a kvality feedu.
