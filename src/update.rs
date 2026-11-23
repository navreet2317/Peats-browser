//! Aggiornamenti: manifest JSON, confronto delle versioni, SHA-256 del
//! pacchetto e comando di installazione.
//!
//! Il manifest (a un indirizzo scelto dall'utente, HTTPS) ha questa forma:
//!
//! ```json
//! {"version": "0.2.0",
//!  "url": "https://esempio.test/peats-browser-0.2.0-1.fc44.x86_64.rpm",
//!  "sha256": "…",
//!  "notes": "Novità di questa versione"}
//! ```
//!
//! Il pacchetto viene scaricato, confrontato con lo SHA-256 del manifest e solo
//! allora passato a `dnf install` tramite `pkexec` (la password la chiede
//! polkit). Nessun controllo né download parte se l'opzione è spenta.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// Indirizzo HTTPS del pacchetto (RPM).
    pub url: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
}

pub fn parse_manifest(text: &str) -> Result<Manifest> {
    let manifest: Manifest =
        serde_json::from_str(text).context("manifest degli aggiornamenti non valido")?;
    if manifest.version.trim().is_empty() {
        bail!("il manifest non indica la versione");
    }
    Ok(manifest)
}

/// Vero se `candidate` è più recente di `current`, con confronto numerico per
/// segmenti: `0.1.10` è più recente di `0.1.9`, `1.0` equivale a `1.0.0`.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    let left = version_parts(current);
    let right = version_parts(candidate);
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        if a != b {
            return b > a;
        }
    }
    false
}

fn version_parts(version: &str) -> Vec<u64> {
    version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// Solo un'installazione di sistema può aggiornarsi: una build di sviluppo non
/// deve sovrascrivere file in `/usr`.
pub fn managed_install() -> bool {
    std::env::current_exe()
        .map(|path| {
            let text = path.to_string_lossy();
            text.starts_with("/usr/bin/") || text.starts_with("/usr/local/bin/")
        })
        .unwrap_or(false)
}

/// `pkexec dnf install -y <pacchetto>`: l'autorizzazione la gestisce polkit,
/// l'app non chiede né conserva password di amministratore.
pub fn install_command(package: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new("pkexec");
    command.args(["dnf", "install", "-y"]).arg(package);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_compared_by_segments() {
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.1.9", "0.1.10"));
        assert!(is_newer("0.9.0", "1.0.0"));
        assert!(is_newer("0.1.0", "v0.2"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("1.0", "1.0.0"));
        assert!(!is_newer("0.2.0", "0.1.9"));
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn manifest_is_parsed_and_validated() {
        let manifest =
            parse_manifest(r#"{"version": "0.2.0", "url": "https://a.test/x.rpm", "sha256": "ab"}"#)
                .unwrap();
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.notes, "");
        assert!(parse_manifest("{}").is_err());
        assert!(parse_manifest("non json").is_err());
    }
}
