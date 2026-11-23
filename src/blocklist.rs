//! Filtro contenuti di base.
//!
//! WebKitGTK accetta i filtri in formato "content blocker": un array JSON di
//! regole `trigger`/`action`. Tenere la lista incorporata evita download
//! esterni e mantiene il browser autosufficiente; è volutamente corta e
//! concentrata su reti pubblicitarie e tracker molto diffusi.

/// Host bloccati: viene bloccato l'host esatto e ogni suo sottodominio.
pub const BLOCKED_HOSTS: &[&str] = &[
    "2mdn.net",
    "adnxs.com",
    "adsrvr.org",
    "advertising.com",
    "amazon-adsystem.com",
    "bidswitch.net",
    "casalemedia.com",
    "criteo.com",
    "criteo.net",
    "doubleclick.net",
    "googleadservices.com",
    "googlesyndication.com",
    "google-analytics.com",
    "googletagservices.com",
    "hotjar.com",
    "imasdk.googleapis.com",
    "indexww.com",
    "inmobi.com",
    "krxd.net",
    "mathtag.com",
    "moatads.com",
    "mookie1.com",
    "openx.net",
    "outbrain.com",
    "pubmatic.com",
    "quantserve.com",
    "rubiconproject.com",
    "scorecardresearch.com",
    "serving-sys.com",
    "sharethrough.com",
    "smartadserver.com",
    "taboola.com",
    "teads.tv",
    "tremorhub.com",
    "turn.com",
    "yieldmo.com",
    "zedo.com",
];

pub const FILTER_IDENTIFIER: &str = "peats-ads";

pub fn host_for(uri: &str) -> Option<String> {
    let url = url::Url::parse(uri).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    Some(host.trim_end_matches('.').to_owned())
}

/// `true` quando l'host appartiene alla lista incorporata.
pub fn is_blocked(uri: &str) -> bool {
    let Some(host) = host_for(uri) else {
        return false;
    };

    BLOCKED_HOSTS
        .iter()
        .any(|blocked| host == *blocked || host.ends_with(&format!(".{blocked}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_known_hosts_and_subdomains() {
        assert!(is_blocked("https://doubleclick.net/x.js"));
        assert!(is_blocked("https://securepubads.g.doubleclick.net/tag"));
        assert!(is_blocked("http://criteo.com"));
    }

    #[test]
    fn keeps_normal_sites() {
        assert!(!is_blocked("https://example.com"));
        assert!(!is_blocked("https://notdoubleclick.net"));
        assert!(!is_blocked("https://rust-lang.org/std"));
        assert!(!is_blocked("about:blank"));
    }


}
