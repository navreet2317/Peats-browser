//! Chiave con cui sono cifrate le password salvate su disco.
//!
//! La chiave vive nel portachiavi di sistema (GNOME Keyring, interrogato con
//! `secret-tool`), non nel database: copiare `peats.db` non basta per leggere
//! le password. Se il portachiavi non è disponibile l'importazione delle
//! password viene rifiutata invece di salvarle in chiaro.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::crypto;

const SERVICE: &str = "io.github.peats.Browser";
const PURPOSE: &str = "passwords";

static CACHE: OnceLock<[u8; 32]> = OnceLock::new();

/// Chiave a 256 bit per le password, creata al primo uso.
pub fn passwords_key() -> Result<[u8; 32], String> {
    if let Some(key) = CACHE.get() {
        return Ok(*key);
    }
    let key = match lookup()? {
        Some(hex) => decode(&hex)?,
        None => create()?,
    };
    let _ = CACHE.set(key);
    Ok(key)
}

fn lookup() -> Result<Option<String>, String> {
    let output = Command::new("secret-tool")
        .args(["lookup", "service", SERVICE, "purpose", PURPOSE])
        .output()
        .map_err(|error| format!("secret-tool non disponibile: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() {
        return Ok(if text.is_empty() { None } else { Some(text) });
    }
    // `secret-tool lookup` esce con 1 quando la voce non esiste ancora.
    if text.is_empty() {
        return Ok(None);
    }
    Err(format!(
        "il portachiavi ha rifiutato la richiesta: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn create() -> Result<[u8; 32], String> {
    let key: [u8; 32] =
        crypto::random_bytes().map_err(|error| format!("entropia non disponibile: {error}"))?;
    let hex = crypto::to_hex(&key);

    let mut child = Command::new("secret-tool")
        .args([
            "store",
            "--label",
            "Peats — chiave delle password",
            "service",
            SERVICE,
            "purpose",
            PURPOSE,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("secret-tool non disponibile: {error}"))?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(hex.as_bytes())
            .map_err(|error| format!("scrittura nel portachiavi non riuscita: {error}"))?;
    }
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .map_err(|error| format!("portachiavi non disponibile: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "portachiavi non disponibile: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(key)
}

fn decode(hex: &str) -> Result<[u8; 32], String> {
    let bytes = crypto::from_hex(hex)
        .ok_or_else(|| "la chiave nel portachiavi non è leggibile".to_owned())?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| "la chiave nel portachiavi ha la lunghezza sbagliata".to_owned())
}
