# UK VPS Setup Guide pro Betfair/Smarkets Access

## 🎯 Cíl
Získat UK IP adresu pro přístup k Betfair Exchange a Smarkets bez geoblocku.

## 📋 Krok za krokem

### 1. **Založení UK VPS (7 denní trial)**
**Doporučený provider:** [Contabo](https://contabo.com/en/vps/) - London datacenter

**Proces:**
1. Navštiv https://contabo.com/en/vps/
2. Vyber "VPS S" (£4.99/měs) nebo "VPS M" (£8.99/měs)
3. **Důležité:** Vyber London jako datacenter
4. V checkoutu použij validní email (obdržíš přístupové údaje)
5. Platba: PayPal nebo kreditní karta
6. **Po dokončení:** Obdržíš email s:
   - IP adresou VPS (UK IP)
   - SSH přihlašovacími údaji (root password)

### 2. **Připojení k VPS (SSH)**
```bash
# Na tvém lokálním počítači
ssh root@<vps-ip-address>
# Heslo z emailu
```

### 3. **Instalace základního software na VPS**
```bash
# Update systému
apt-get update && apt-get upgrade -y

# Instalace Rust a build tools
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# Instalace Git
apt-get install -y git build-essential

# Instalace Node.js (pro případné proxy tools)
curl -fsSL https://deb.nodesource.com/setup_18.x | bash -
apt-get install -y nodejs

# Instalace PM2 pro process management
npm install -g pm2
```

### 4. **Klonování RustMiskoLive na VPS**
```bash
cd /root
git clone https://github.com/<your-repo>/RustMiskoLive.git
cd RustMiskoLive

# Build projektu
cargo build --release

# Test že vše funguje
./target/release/hltv-test
```

### 5. **Nastavení Proxy Rotation pro prevenci banu**
Betfair detekuje a banuje datacenter IP (VPS). Potřebujeme **residential proxy**.

**Možnosti:**
- **Bright Data (Luminati):** ~$15/měs za 5GB UK residential IP
- **Smartproxy:** ~$12/měs
- **Proxy-Cheap:** ~$10/měs

**Konfigurace proxy v Rust kódu:**
```rust
// crates/price_monitor/src/betfair_client.rs
use reqwest::{Client, Proxy};

pub struct BetfairClient {
    client: Client,
    proxy_list: Vec<String>,
    current_proxy_idx: usize,
}

impl BetfairClient {
    pub fn new() -> Self {
        let proxy_list = vec![
            "http://user:pass@uk-residential-proxy1:8888".to_string(),
            "http://user:pass@uk-residential-proxy2:8888".to_string(),
            // Přidej více proxy pro rotaci
        ];
        
        let client = Client::builder()
            .proxy(Proxy::all(&proxy_list[0]).unwrap())
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap();
        
        Self {
            client,
            proxy_list,
            current_proxy_idx: 0,
        }
    }
    
    pub fn rotate_proxy(&mut self) {
        self.current_proxy_idx = (self.current_proxy_idx + 1) % self.proxy_list.len();
        self.client = Client::builder()
            .proxy(Proxy::all(&self.proxy_list[self.current_proxy_idx]).unwrap())
            .build()
            .unwrap();
    }
}
```

### 6. **Betfair API Setup**
**Registrace Developer Account:**
1. Přihlas se na https://developer.betfair.com/
2. Vytvoř novou aplikaci
3. Získej:
   - **App Key** (identifikace aplikace)
   - **Username** a **Password** (tvůj Betfair účet)
   - **Certificates** pro SSL (pokud použiješ)

**Testovací kód pro Betfair API:**
```rust
// test_betfair_api.rs
use reqwest::Client;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new();
    
    // Login request
    let login_payload = json!({
        "username": "YOUR_USERNAME",
        "password": "YOUR_PASSWORD"
    });
    
    let response = client.post("https://identitysso.betfair.com/api/login")
        .header("X-Application", "YOUR_APP_KEY")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("username={}&password={}", 
            "YOUR_USERNAME", "YOUR_PASSWORD"))
        .send()
        .await?;
    
    println!("Status: {}", response.status());
    println!("Body: {}", response.text().await?);
    
    Ok(())
}
```

### 7. **Smarkets API Setup**
Smarkets má podobné REST API jako Betfair, ale s nižšími poplatky (2%).

**Registrace:**
1. Přes UK VPS navštiv https://smarkets.com/
2. Vytvoř účet (přes UK IP by neměl být geoblock)
3. Pro API: kontaktuj support@smarkets.com pro API přístup

### 8. **Automatický Deploy a Monitoring**
**PM2 konfigurace pro automatický restart:**
```bash
# Na VPS v /root/RustMiskoLive
pm2 start ./target/release/ultra-live --name "rustmisko-ultra"
pm2 save
pm2 startup  # Pro automatický start při rebootu
```

**Logování:**
```bash
# Sleduj logy
pm2 logs rustmisko-ultra

# Status aplikace
pm2 status

# Restart při změně kódu
pm2 restart rustmisko-ultra
```

### 9. **Firewall a Bezpečnost**
```bash
# Povol pouze potřebné porty
ufw allow ssh
ufw allow 22/tcp
ufw enable

# Monitoruj přístupy
apt-get install -y fail2ban
systemctl enable fail2ban
```

### 10. **Backup Strategy**
```bash
# Denní backup kódu
cd /root
tar -czf rustmisko-backup-$(date +%Y%m%d).tar.gz RustMiskoLive/
# Upload na S3 nebo další úložiště
```

## ⚠️ **Rizika a Mitigace**

### Riziko 1: Betfair detekce botů
- **Mitigace:** 
  - Používat realistic request patterns (ne příliš rychlé)
  - Rotace residential proxy
  - Human-like delays mezi requesty (1-3s)

### Riziko 2: VPS ban za příliš mnoho requestů
- **Mitigace:**
  - Rate limiting v kódu
  - Implementace exponential backoff při 429/503
  - Monitorování HTTP status codes

### Riziko 3: SX Bet oracle zrychlení
- **Mitigace:**
  - Diversifikace na další Web3 sázkovky
  - Monitoring jejich GitHubu pro změny v oracle contracts

## 🚀 **Testovací Scénář**

### Den 1-2: Testování connectivity
```bash
# Test že VPS má UK IP
curl ifconfig.me

# Test Betfair API z VPS
cd /root/RustMiskoLive
cargo run --bin test-betfair-connectivity
```

### Den 3-4: Test scraping rychlosti
```bash
# Benchmark HLTV vs GosuGamers
./target/release/hltv-test --benchmark
```

### Den 5-7: Integrační testy
```bash
# Spusť ultra-live monitor na 24 hodin
pm2 start ./target/release/ultra-live --name "test-run"
```

## 📊 **Metriky Úspěchu**

1. **Latence detekce:** <15s (vs. 60-120s původně)
2. **Betfair API úspěšnost:** >95% requestů
3. **Sniper mode activation:** při confidence >90%
4. **Uptime:** >95% (monitorováno přes PM2)

## 💰 **Odhad Nákladů**

- **UK VPS:** £4.99/měs (Contabo)
- **Residential Proxy:** $12-15/měs
- **Celkem:** ~$20/měs

## 📞 **Support**

**Při problémech:**
1. Zkontroluj logy: `pm2 logs rustmisko-ultra`
2. Testuj connectivity: `curl https://api.betfair.com`
3. Kontaktuj support@contabo.com pro VPS problémy
4. Pro proxy problémy: kontaktuj poskytovatele proxy

---

**Stav:** ✅ Návrh kompletní  
**Následující krok:** Založit Contabo trial a otestovat connectivity