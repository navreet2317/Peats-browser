//! Aggiornamenti: release GitHub o manifest JSON, confronto delle versioni,
//! SHA-256 del pacchetto e comando di installazione.
//!
//! Con un indirizzo di repository (`https://github.com/navreet2317/Peats-browser`,
//! il predefinito) Peats legge `GET /repos/proprietario/nome/releases/latest` e
//! usa il tag come versione e l'allegato `.rpm` come pacchetto.
//!
//! In alternativa l'indirizzo può essere un manifest JSON (HTTPS):
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

/// Repository GitHub controllato quando nelle Preferenze non è indicato un
/// indirizzo diverso: Peats legge la sua ultima release.
pub const DEFAULT_REPO_URL: &str = "https://github.com/navreet2317/Peats-browser";

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

/// Indirizzo della API «ultima release» se `input` è l'indirizzo di un
/// repository GitHub (`https://github.com/proprietario/nome`, anche con `.git`
/// finale o con un percorso come `/releases`).
pub fn github_api_url(input: &str) -> Option<String> {
    let text = input.trim().trim_end_matches('/');
    let rest = text
        .strip_prefix("https://github.com/")
        .or_else(|| text.strip_prefix("http://github.com/"))?;
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let mut parts = rest.split('/').filter(|part| !part.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?.trim_end_matches(".git");
    if !valid_repo_segment(owner) || !valid_repo_segment(repo) {
        return None;
    }
    Some(format!(
        "https://api.github.com/repos/{owner}/{repo}/releases/latest"
    ))
}

fn valid_repo_segment(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.starts_with('.')
        && !segment.ends_with('.')
        && segment.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

/// Release come la restituisce la API di GitHub (campi usati soltanto).
#[derive(Clone, Debug, Deserialize)]
struct GithubRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    digest: String,
}

/// Trasforma `GET /repos/proprietario/nome/releases/latest` in un [`Manifest`]:
/// versione dal tag, pacchetto `.rpm` allegato (prima quello `x86_64`) e
/// SHA-256 dal `digest` dell'allegato oppure, se GitHub non lo riporta, da una
/// riga `sha256: …` nella descrizione della release.
pub fn manifest_from_github_release(text: &str) -> Result<Manifest> {
    let value: serde_json::Value =
        serde_json::from_str(text).context("risposta di GitHub non valida")?;
    if let Some(message) = value.get("message").and_then(|message| message.as_str()) {
        bail!("GitHub: {message}");
    }
    let release: GithubRelease =
        serde_json::from_value(value).context("release di GitHub non riconosciuta")?;
    if release.tag_name.trim().is_empty() {
        bail!("la release non ha un numero di versione");
    }

    let asset = release
        .assets
        .iter()
        .filter(|asset| is_package(&asset.name))
        .min_by_key(|asset| package_rank(&asset.name))
        .with_context(|| format!("la release non contiene un pacchetto {}", PACKAGE_LABEL))?;
    if !asset.browser_download_url.starts_with("https://") {
        bail!("la release non indica un indirizzo HTTPS per il pacchetto");
    }

    let sha256 = asset
        .digest
        .trim()
        .strip_prefix("sha256:")
        .map(|digest| digest.trim().to_ascii_lowercase())
        .filter(|digest| digest.len() == 64)
        .or_else(|| sha256_in_text(&release.body))
        .unwrap_or_default();

    Ok(Manifest {
        version: release.tag_name.trim().to_owned(),
        url: asset.browser_download_url.clone(),
        sha256,
        notes: release.body.trim().to_owned(),
    })
}

/// Estensioni accettate per il pacchetto di aggiornamento sulla piattaforma
/// corrente: RPM su Linux, MSI o EXE su Windows.
fn is_package(name: &str) -> bool {
    #[cfg(windows)]
    const EXTENSIONS: [&str; 2] = [".msi", ".exe"];
    #[cfg(not(windows))]
    const EXTENSIONS: [&str; 1] = [".rpm"];

    let name = name.to_ascii_lowercase();
    EXTENSIONS.iter().any(|extension| name.ends_with(extension))
}

#[cfg(windows)]
const PACKAGE_LABEL: &str = "MSI o EXE";
#[cfg(not(windows))]
const PACKAGE_LABEL: &str = "RPM";

/// Ordine di preferenza fra più pacchetti allegati: prima quelli per
/// l'architettura corrente, poi gli altri.
fn package_rank(name: &str) -> u8 {
    let name = name.to_ascii_lowercase();
    if name.contains("x86_64") || name.contains("x64") {
        0
    } else {
        1
    }
}

/// Cerca uno SHA-256 (64 cifre esadecimali) nella descrizione della release,
/// con o senza il prefisso `sha256:`.
fn sha256_in_text(text: &str) -> Option<String> {
    let lowered = text.to_ascii_lowercase().replace("sha256:", " ");
    lowered
        .split(|character: char| !character.is_ascii_hexdigit())
        .find(|token| token.len() == 64)
        .map(str::to_owned)
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
/// deve sovrascrivere file di sistema.
pub fn managed_install() -> bool {
    let Ok(path) = std::env::current_exe() else {
        return false;
    };
    let text = path.to_string_lossy();

    #[cfg(windows)]
    {
        let lowered = text.to_ascii_lowercase();
        !(lowered.contains("\\target\\debug\\") || lowered.contains("\\target\\release\\"))
    }
    #[cfg(not(windows))]
    {
        text.starts_with("/usr/bin/") || text.starts_with("/usr/local/bin/")
    }
}

/// Comando che installa il pacchetto scaricato.
///
/// Su Linux è `pkexec dnf install -y`: l'autorizzazione la gestisce polkit e
/// l'app non chiede né conserva password di amministratore. Su Windows un MSI
/// passa da `msiexec`, un EXE viene avviato direttamente (il programma di
/// installazione chiede da sé l'elevazione).
#[cfg(not(windows))]
pub fn install_command(package: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new("pkexec");
    command.args(["dnf", "install", "-y"]).arg(package);
    command
}

#[cfg(windows)]
pub fn install_command(package: &std::path::Path) -> std::process::Command {
    let extension = package
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if extension == "msi" {
        let mut command = std::process::Command::new("msiexec");
        command.arg("/i").arg(package);
        command
    } else {
        std::process::Command::new(package)
    }
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
    fn github_repository_urls_map_to_the_release_api() {
        let expected = Some("https://api.github.com/repos/navreet2317/Peats-browser/releases/latest");
        assert_eq!(
            github_api_url("https://github.com/navreet2317/Peats-browser").as_deref(),
            expected.map(str::to_owned).as_deref()
        );
        assert!(github_api_url("https://github.com/navreet2317/Peats-browser/").is_some());
        assert!(github_api_url("https://github.com/navreet2317/Peats-browser.git").is_some());
        assert!(github_api_url("https://github.com/navreet2317/Peats-browser/releases").is_some());
        assert!(github_api_url("https://esempio.test/update.json").is_none());
        assert!(github_api_url("https://github.com/solo-proprietario").is_none());
        assert!(github_api_url("https://github.com/../x").is_none());
    }

    #[test]
    fn github_release_becomes_a_manifest_with_digest() {
        let json = r#"{
            "tag_name": "v0.2.0",
            "body": "Correzioni varie",
            "assets": [
                {"name": "note.txt", "browser_download_url": "https://github.com/a/b/releases/download/v0.2.0/note.txt"},
                {"name": "peats-browser-0.2.0-1.fc44.x86_64.rpm",
                 "browser_download_url": "https://github.com/a/b/releases/download/v0.2.0/peats-browser-0.2.0-1.fc44.x86_64.rpm",
                 "digest": "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"}
            ]
        }"#;
        let manifest = manifest_from_github_release(json).expect("release valida");
        assert_eq!(manifest.version, "v0.2.0");
        assert!(manifest.url.ends_with("peats-browser-0.2.0-1.fc44.x86_64.rpm"));
        assert_eq!(
            manifest.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(manifest.notes, "Correzioni varie");
        assert!(is_newer("0.1.0", &manifest.version));
    }

    #[test]
    fn github_release_can_take_the_hash_from_its_notes() {
        let json = r#"{
            "tag_name": "0.3.0",
            "body": "Pacchetto\nsha256: ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "assets": [
                {"name": "peats-browser-0.3.0-1.fc44.aarch64.rpm", "browser_download_url": "https://github.com/a/b/releases/download/0.3.0/x.rpm"},
                {"name": "peats-browser-0.3.0-1.fc44.x86_64.rpm", "browser_download_url": "https://github.com/a/b/releases/download/0.3.0/y.rpm"}
            ]
        }"#;
        let manifest = manifest_from_github_release(json).expect("release valida");
        assert!(manifest.url.ends_with("y.rpm"));
        assert_eq!(
            manifest.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn github_release_errors_are_reported() {
        let message = manifest_from_github_release(r#"{"message": "Not Found"}"#).unwrap_err();
        assert!(format!("{message:#}").contains("Not Found"));

        let no_rpm = manifest_from_github_release(
            r#"{"tag_name": "1.0.0", "assets": [{"name": "a.txt", "browser_download_url": "https://a.test/a.txt"}]}"#,
        );
        assert!(no_rpm.is_err());

        let no_version =
            manifest_from_github_release(r#"{"assets": []}"#).unwrap_err();
        assert!(format!("{no_version:#}").contains("versione"));
        assert!(manifest_from_github_release("non json").is_err());
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
