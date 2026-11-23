//! Liste di blocco convertite nel formato del filtro contenuti di WebKit.
//!
//! Fonte predefinita: `StevenBlack/hosts` (licenza MIT), un file in formato
//! hosts (righe `0.0.0.0 dominio`) che raccoglie host pubblicitari e di
//! tracciamento da più liste pubbliche.
//!
//! - Fonte: <https://github.com/StevenBlack/hosts>
//! - Licenza: MIT (vedi LICENSE del repository)
//! - Aggiornamento: su richiesta (pulsante «Aggiorna liste») oppure automatico
//!   all'avvio quando la copia locale ha più di `MAX_AGE` giorni; in caso di
//!   errore resta l'ultima copia valida.
//!
//! Il file scaricato viene convertito in regole `trigger`/`action` accettate da
//! `WebKitUserContentFilter`; è lo stesso meccanismo del filtro incorporato in
//! `blocklist.rs`, che resta come fallback quando non esiste una copia locale.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const LIST_NAME: &str = "StevenBlack/hosts";
pub const LIST_URL: &str = "https://raw.githubusercontent.com/StevenBlack/hosts/master/hosts";
pub const LIST_LICENSE: &str = "MIT";
pub const CACHE_HOSTS: &str = "peats-hosts.txt";
pub const CACHE_META: &str = "peats-meta.json";
/// Dopo questo numero di secondi la copia locale è considerata vecchia.
pub const MAX_AGE: i64 = 7 * 24 * 60 * 60;

/// URL della lista. `PEATS_FILTER_LIST_URL` permette di usare un mirror o un
/// file locale nei test senza modificare il binario.
pub fn list_url() -> String {
    std::env::var("PEATS_FILTER_LIST_URL").unwrap_or_else(|_| LIST_URL.to_owned())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ListMeta {
    pub source: String,
    pub url: String,
    pub license: String,
    pub updated_at: i64,
    pub hosts: usize,
}

pub fn now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// `true` quando la copia locale ha più di `max_age` secondi.
pub fn is_stale(meta: &ListMeta, now: i64, max_age: i64) -> bool {
    meta.updated_at <= 0 || now.saturating_sub(meta.updated_at) > max_age
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.contains('.')
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.ends_with(".local")
        && host != "localhost"
        && host != "local"
        && host != "broadcasthost"
        && host != "localhost.localdomain"
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Estrae gli host da un file in formato hosts, ignorando commenti, indirizzi
/// che non siano redirect a `0.0.0.0`/loopback e nomi non validi.
pub fn parse_hosts(text: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(address) = fields.next() else {
            continue;
        };
        let redirect = matches!(address, "0.0.0.0" | "127.0.0.1" | "::1" | "::" | "0");
        if !redirect {
            continue;
        }
        for host in fields {
            let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
            if valid_host(&host) {
                hosts.push(host);
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Unione fra lista incorporata e copia locale, senza duplicati.
pub fn all_hosts(cached: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = crate::blocklist::BLOCKED_HOSTS
        .iter()
        .map(|host| (*host).to_owned())
        .collect();
    hosts.extend(cached.iter().cloned());
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Estrae gli host dalle origini eccettuate (`https://example.com:8443`).
pub fn exception_hosts(origins: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = origins
        .iter()
        .filter_map(|origin| {
            url::Url::parse(origin)
                .ok()
                .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
        })
        .filter(|host| valid_host(host))
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

fn escape(host: &str) -> String {
    host.replace('.', "\\\\.")
}

/// Tipi di risorsa bloccati. `document` è escluso di proposito: una regola su
/// `document` bloccherebbe anche la navigazione diretta verso l'host, e in
/// ogni caso WebKit non distingue il documento principale da un iframe. Gli
/// iframe ospitati da un host bloccato restano quindi caricabili come
/// documento, ma script, immagini e fogli di stile che richiedono vengono
/// bloccati; i pop-up sono bloccati dal tipo `popup`.
pub const BLOCKED_RESOURCE_TYPES: &str = "[\"image\",\"style-sheet\",\"script\",\"font\",\"raw\",\"svg-document\",\"media\",\"popup\",\"ping\",\"fetch\",\"websocket\",\"other\"]";

/// Selettori del blocco cosmetico (`css-display-none`).
///
/// Sono pochi e conservativi: contengono la parola `ad`/`ads` o un attributo
/// esplicito, così non nascondono contenuti legittimi. Non vengono copiati da
/// liste di terze parti (che richiederebbero un convertitore di regole ABP):
/// sono scritti qui e verificati dai test. Si disattivano dalle Preferenze.
pub const COSMETIC_SELECTORS: &[&str] = &[
    ".adsbygoogle",
    "ins.adsbygoogle",
    "[id^=\"div-gpt-ad\"]",
    "iframe[id^=\"google_ads_iframe\"]",
    "[data-ad-slot]",
    "[data-ad-client]",
    "[aria-label=\"Advertisement\"]",
    ".ad-banner",
    "#ad-banner",
    ".ad-slot",
    ".ad-unit",
    ".ad-container",
    ".advert",
    ".advertisement",
    ".ads-container",
    ".sponsor-box",
    ".sponsored-content",
    ".taboola",
    ".outbrain",
    "#adsense",
    ".adsense",
];

/// Escape minimo per inserire un selettore in una stringa JSON.
fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 4);
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Impronta del testo delle regole (FNV-1a a 64 bit). Serve a riconoscere se il
/// filtro già compilato da WebKit corrisponde alle regole attuali, evitando di
/// ricompilarlo a ogni avvio.
pub fn fingerprint(rules: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in rules.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Regole nel formato di WebKit: un blocco per host e, in coda, una regola
/// `ignore-previous-rules` per ogni dominio eccettuato. Le eccezioni usano
/// `if-domain`, quindi valgono per la pagina di quel dominio e dei sottodomini
/// senza disattivare il blocco sulle altre pagine.
pub fn build_filter_json(hosts: &[String], exceptions: &[String], cosmetic: bool) -> String {
    let mut rules: Vec<String> = Vec::with_capacity(hosts.len() * 2 + exceptions.len());
    for host in hosts {
        let escaped = escape(host);
        rules.push(format!(
            "{{\"trigger\":{{\"url-filter\":\".*://([^/]*\\\\.)?{escaped}[:/]\",\"resource-type\":{BLOCKED_RESOURCE_TYPES}}},\"action\":{{\"type\":\"block\"}}}}"
        ));
        rules.push(format!(
            "{{\"trigger\":{{\"url-filter\":\".*://([^/]*\\\\.)?{escaped}$\",\"resource-type\":{BLOCKED_RESOURCE_TYPES}}},\"action\":{{\"type\":\"block\"}}}}"
        ));
    }
    for host in exceptions {
        // `if-domain` accetta domini letterali (con `*` iniziale per i
        // sottodomini), non regex: i punti non vanno escapati.
        rules.push(format!(
            "{{\"trigger\":{{\"url-filter\":\".*\",\"if-domain\":[\"*{host}\"]}},\"action\":{{\"type\":\"ignore-previous-rules\"}}}}"
        ));
    }
    if cosmetic {
        // Una regola per selettore, tutte con le stesse eccezioni: `unless-domain`
        // vale anche per `css-display-none`, così disattivare il blocco su un
        // sito lo riporta esattamente com'era.
        let unless = if exceptions.is_empty() {
            String::new()
        } else {
            let domains = exceptions
                .iter()
                .map(|host| format!("\"*{host}\""))
                .collect::<Vec<_>>()
                .join(",");
            format!(",\"unless-domain\":[{domains}]")
        };
        for selector in COSMETIC_SELECTORS {
            rules.push(format!(
                "{{\"trigger\":{{\"url-filter\":\".*\"{unless}}},\"action\":{{\"type\":\"css-display-none\",\"selector\":\"{}\"}}}}",
                json_escape(selector)
            ));
        }
    }

    format!("[{}]", rules.join(","))
}

/// Metadati del filtro compilato: identificatore e impronta delle regole.
pub const CACHE_COMPILED: &str = "peats-compiled.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CompiledMeta {
    pub identifier: String,
    pub fingerprint: String,
}

pub fn load_compiled(dir: &Path) -> Option<CompiledMeta> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(CACHE_COMPILED)).ok()?).ok()
}

pub fn save_compiled(dir: &Path, meta: &CompiledMeta) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(meta)
        .map_err(|error| io::Error::other(error.to_string()))?;
    std::fs::write(dir.join(CACHE_COMPILED), json)
}

pub fn load_cache(dir: &Path) -> Option<(ListMeta, Vec<String>)> {
    let meta: ListMeta = serde_json::from_str(&std::fs::read_to_string(dir.join(CACHE_META)).ok()?).ok()?;
    let hosts = parse_hosts(&std::fs::read_to_string(dir.join(CACHE_HOSTS)).ok()?);
    if hosts.is_empty() {
        return None;
    }
    Some((meta, hosts))
}

pub fn save_cache(dir: &Path, meta: &ListMeta, hosts: &[String]) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut body = String::with_capacity(hosts.len() * 24);
    for host in hosts {
        body.push_str("0.0.0.0 ");
        body.push_str(host);
        body.push('\n');
    }
    std::fs::write(dir.join(CACHE_HOSTS), body)?;
    let json = serde_json::to_string_pretty(meta)
        .map_err(|error| io::Error::other(error.to_string()))?;
    std::fs::write(dir.join(CACHE_META), json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# commento
127.0.0.1 localhost
127.0.0.1 localhost.localdomain
0.0.0.0 doubleclick.net
0.0.0.0 tracker.example.com  # inline
0.0.0.0 doubleclick.net
::1 ipv6-only.example.com
10.0.0.1 not-a-block.example.com
0.0.0.0 local
";

    #[test]
    fn parses_hosts_format() {
        let hosts = parse_hosts(SAMPLE);
        assert_eq!(
            hosts,
            vec![
                "doubleclick.net".to_owned(),
                "ipv6-only.example.com".to_owned(),
                "tracker.example.com".to_owned(),
            ]
        );
    }

    #[test]
    fn ignores_invalid_lines() {
        assert!(parse_hosts("").is_empty());
        assert!(parse_hosts("# solo commenti\n\n").is_empty());
        assert!(parse_hosts("0.0.0.0 nodot\n").is_empty());
        assert!(parse_hosts("0.0.0.0 bad host\n").is_empty());
    }

    #[test]
    fn merges_embedded_and_cached_hosts() {
        let hosts = all_hosts(&["doubleclick.net".to_owned(), "x.example".to_owned()]);
        assert_eq!(hosts.iter().filter(|h| *h == "doubleclick.net").count(), 1);
        assert!(hosts.iter().any(|h| h == "x.example"));
        assert!(hosts.iter().any(|h| h == "criteo.com"));
        let mut sorted = hosts.clone();
        sorted.sort();
        assert_eq!(hosts, sorted);
    }

    #[test]
    fn builds_block_and_exception_rules() {
        let hosts = vec!["doubleclick.net".to_owned()];
        let exceptions = vec!["example.com".to_owned()];
        let json = build_filter_json(&hosts, &exceptions, false);
        let rules: serde_json::Value = serde_json::from_str(&json).expect("JSON valido");
        let rules = rules.as_array().expect("array");
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0]["action"]["type"], "block");
        assert_eq!(rules[1]["action"]["type"], "block");
        // Il documento principale non deve essere bloccato, le sottorisorse sì.
        let types = rules[0]["trigger"]["resource-type"].as_array().expect("tipi");
        assert!(types.iter().any(|value| value == "image"));
        assert!(types.iter().any(|value| value == "script"));
        assert!(!types.iter().any(|value| value == "document"));
        assert_eq!(rules[2]["action"]["type"], "ignore-previous-rules");
        assert_eq!(rules[2]["trigger"]["if-domain"][0], "*example.com");
        // WebKit rifiuta le disgiunzioni nei filtri.
        assert!(!json.contains('|'));
    }

    #[test]
    fn cosmetic_rules_hide_ads_and_respect_exceptions() {
        let json = build_filter_json(&[], &["example.com".to_owned()], true);
        let rules: serde_json::Value = serde_json::from_str(&json).expect("JSON valido");
        let rules = rules.as_array().expect("array");
        // Le regole cosmetiche più quella dell'eccezione.
        let cosmetic_rules: Vec<&serde_json::Value> = rules
            .iter()
            .filter(|rule| rule["action"]["type"] == "css-display-none")
            .collect();
        assert_eq!(cosmetic_rules.len(), COSMETIC_SELECTORS.len());
        assert!(cosmetic_rules
            .iter()
            .all(|rule| rule["trigger"]["unless-domain"][0] == "*example.com"));
        // I selettori con virgolette restano JSON validi.
        assert!(rules
            .iter()
            .any(|rule| rule["action"]["selector"] == "[aria-label=\"Advertisement\"]"));

        // Senza eccezioni il trigger non contiene `unless-domain`.
        let plain = build_filter_json(&[], &[], true);
        let plain: serde_json::Value = serde_json::from_str(&plain).expect("JSON valido");
        assert!(plain.as_array().expect("array")[0]["trigger"]
            .get("unless-domain")
            .is_none());

        // Disattivato: nessuna regola cosmetica.
        let off = build_filter_json(&[], &[], false);
        assert_eq!(off, "[]");
    }

    #[test]
    fn fingerprints_are_stable_and_change_with_rules() {
        let rules = build_filter_json(&["ads.example".to_owned()], &[], true);
        assert_eq!(fingerprint(&rules), fingerprint(&rules));
        assert_ne!(fingerprint(&rules), fingerprint(&format!("{rules} ")));
        assert_eq!(fingerprint("").len(), 16);
    }

    #[test]
    fn compiled_metadata_roundtrip() {
        let dir = std::env::temp_dir().join(format!("peats-compiled-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load_compiled(&dir).is_none());
        let meta = CompiledMeta {
            identifier: "peats-ads".to_owned(),
            fingerprint: "abc123".to_owned(),
        };
        save_compiled(&dir, &meta).expect("salvataggio");
        assert_eq!(load_compiled(&dir), Some(meta));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exceptions_come_from_origins() {
        let hosts = exception_hosts(&[
            "https://example.com:8443".to_owned(),
            "http://127.0.0.1:8765".to_owned(),
            "non-un-origine".to_owned(),
        ]);
        assert_eq!(hosts, vec!["127.0.0.1".to_owned(), "example.com".to_owned()]);
    }

    #[test]
    fn cache_roundtrip_and_staleness() {
        let dir = std::env::temp_dir().join(format!("peats-filters-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hosts = vec!["a.example".to_owned(), "b.example".to_owned()];
        let meta = ListMeta {
            source: LIST_NAME.to_owned(),
            url: LIST_URL.to_owned(),
            license: LIST_LICENSE.to_owned(),
            updated_at: 1_700_000_000,
            hosts: hosts.len(),
        };
        save_cache(&dir, &meta, &hosts).expect("salvataggio");
        let (loaded, loaded_hosts) = load_cache(&dir).expect("lettura");
        assert_eq!(loaded, meta);
        assert_eq!(loaded_hosts, hosts);

        assert!(!is_stale(&loaded, meta.updated_at + 10, MAX_AGE));
        assert!(is_stale(&loaded, meta.updated_at + MAX_AGE + 1, MAX_AGE));
        assert!(is_stale(&ListMeta::default(), 100, MAX_AGE));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
