//! Supporto minimo alle estensioni.
//!
//! Una estensione è una cartella dentro `~/.local/share/peats-browser/extensions`
//! con un `manifest.json`:
//!
//! ```json
//! {
//!   "name": "Tema scuro per esempio",
//!   "version": "1.0",
//!   "description": "Testo mostrato in about:extensions",
//!   "matches": ["*://*.example.com/*"],
//!   "js": ["inject.js"],
//!   "css": ["style.css"]
//! }
//! ```
//!
//! Gli script vengono iniettati come `WebKitUserScript` all'inizio del
//! documento; il CSS viene inserito da uno script che crea un elemento
//! `<style>`. Non esiste ancora un'API per estensioni, permessi o pagine di
//! sfondo: questa è solo l'infrastruttura di caricamento e iniezione.

use std::fs;
use std::path::Path;

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub matches: Vec<String>,
    #[serde(default)]
    pub js: Vec<String>,
    #[serde(default)]
    pub css: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Extension {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub matches: Vec<String>,
    pub sources: Vec<String>,
}

impl Extension {
    pub fn subtitle(&self) -> String {
        let version = if self.version.is_empty() {
            String::new()
        } else {
            format!("v{} · ", self.version)
        };
        format!("{version}{}", self.description)
    }

    /// Pattern di navigazione passati a WebKit (`*` = tutti i siti).
    pub fn patterns(&self) -> Vec<String> {
        if self.matches.is_empty() {
            vec!["*".to_owned()]
        } else {
            self.matches.clone()
        }
    }
}

/// Carica tutte le estensioni valide dalla cartella indicata.
pub fn load_all(directory: &Path) -> Vec<Extension> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };

    let mut extensions = Vec::new();
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("manifest.json");
        let Ok(bytes) = fs::read(&manifest_path) else {
            continue;
        };
        let manifest = match serde_json::from_slice::<Manifest>(&bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                eprintln!("cannot parse {}: {error}", manifest_path.display());
                continue;
            }
        };

        let id = entry.file_name().to_string_lossy().to_string();
        let mut sources = Vec::new();

        for file in &manifest.js {
            match fs::read_to_string(path.join(file)) {
                Ok(source) => sources.push(source),
                Err(error) => eprintln!("cannot read {id}/{file}: {error}"),
            }
        }

        for file in &manifest.css {
            match fs::read_to_string(path.join(file)) {
                Ok(css) => sources.push(style_injection_script(&css)),
                Err(error) => eprintln!("cannot read {id}/{file}: {error}"),
            }
        }

        if sources.is_empty() {
            continue;
        }

        extensions.push(Extension {
            name: if manifest.name.trim().is_empty() {
                id.clone()
            } else {
                manifest.name
            },
            version: manifest.version,
            description: manifest.description,
            matches: manifest.matches,
            sources,
            id,
        });
    }

    extensions
}

fn style_injection_script(css: &str) -> String {
    let literal = serde_json::to_string(css).unwrap_or_else(|_| "\"\"".to_owned());
    format!(
        "(() => {{ if (!document.documentElement) return; \
         const style = document.createElement('style'); \
         style.textContent = {literal}; \
         document.documentElement.appendChild(style); }})();"
    )
}
