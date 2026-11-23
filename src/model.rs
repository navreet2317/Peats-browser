use serde::{Deserialize, Serialize};

use crate::url::DUCKDUCKGO_TEMPLATE;

pub type TabId = u64;
pub type WorkspaceId = u64;
pub type GroupId = u64;

pub const DUCKDUCKGO_ID: &str = "duckduckgo";

/// Decisione memorizzata per un permesso richiesto da un sito.
///
/// `origin` è schema+host(+porta), `kind` è un identificatore stabile
/// (``geolocation``, ``notifications``, ``camera``, ``microphone``, …) e
/// `decision` vale ``allow`` oppure ``deny``.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SitePermission {
    pub origin: String,
    pub kind: String,
    pub decision: String,
}

pub const GROUP_COLORS: [&str; 8] = [
    "blue", "green", "orange", "purple", "red", "teal", "yellow", "pink",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchEngine {
    pub id: String,
    pub name: String,
    pub template: String,
    /// Parola chiave per la barra indirizzi (`g rust` cerca «rust» su questo
    /// motore). Vuota quando il motore non ha scorciatoie.
    #[serde(default)]
    pub keyword: String,
    #[serde(default)]
    pub builtin: bool,
}

impl SearchEngine {
    pub fn duckduckgo() -> Self {
        Self {
            id: DUCKDUCKGO_ID.to_owned(),
            name: "DuckDuckGo".to_owned(),
            template: DUCKDUCKGO_TEMPLATE.to_owned(),
            keyword: "d".to_owned(),
            builtin: true,
        }
    }

    pub fn is_valid_template(&self) -> bool {
        !self.name.trim().is_empty() && self.template.contains("{query}")
    }

    /// La parola chiave normalizzata (`!g` e `G` diventano entrambe `g`).
    pub fn keyword_token(&self) -> Option<String> {
        valid_keyword(&self.keyword)
    }
}

/// Normalizza una parola chiave. Restituisce `None` se non è utilizzabile
/// nella barra indirizzi (vuota, con spazi o con caratteri che confondono la
/// risoluzione URL/ricerca).
pub fn valid_keyword(keyword: &str) -> Option<String> {
    let keyword = keyword.trim().trim_start_matches('!').trim().to_lowercase();
    if keyword.is_empty() {
        return None;
    }
    if keyword
        .chars()
        .any(|character| character.is_whitespace() || matches!(character, '.' | '/' | ':' | '?' | '&' | '=' | '#'))
    {
        return None;
    }
    Some(keyword)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    /// Segue il tema chiaro/scuro di sistema.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "Segui il sistema",
            Self::Light => "Chiaro",
            Self::Dark => "Scuro",
        }
    }

    pub fn from_index(index: u32) -> Self {
        match index {
            1 => Self::Light,
            2 => Self::Dark,
            _ => Self::System,
        }
    }

    pub fn index(self) -> u32 {
        match self {
            Self::System => 0,
            Self::Light => 1,
            Self::Dark => 2,
        }
    }
}

fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub default_engine: String,
    pub engines: Vec<SearchEngine>,
    #[serde(default)]
    pub theme: ThemePreference,
    /// Pagina iniziale personalizzata; vuota significa la pagina interna.
    #[serde(default)]
    pub start_page: String,
    #[serde(default = "enabled")]
    pub block_ads: bool,
    /// Blocca i cookie di terze parti tramite la politica del motore.
    #[serde(default = "enabled")]
    pub block_third_party_cookies: bool,
    /// Segnali GPC/DNT esposti alle pagine via JavaScript (non header HTTP).
    #[serde(default = "enabled")]
    pub privacy_signals: bool,
    /// Assistenza per i banner dei CMP riconosciuti (rifiuto non essenziali).
    #[serde(default)]
    pub cookie_banner_helper: bool,
    /// Suggerimenti sotto la barra indirizzi (schede, cronologia, segnalibri e
    /// ricerca rapida su DuckDuckGo).
    #[serde(default = "enabled")]
    pub search_suggestions: bool,
    /// Nasconde gli spazi pubblicitari con regole CSS (`css-display-none`)
    /// oltre a bloccarne le richieste.
    #[serde(default = "enabled")]
    pub cosmetic_blocking: bool,
    /// Scarica le WebView delle schede inattive per liberare memoria; la
    /// scheda si ricarica quando la si riapre.
    #[serde(default)]
    pub unload_inactive_tabs: bool,
    /// Giorni di cronologia conservati (0 = nessuna potatura).
    #[serde(default = "default_history_days")]
    pub history_retention_days: i64,
    /// Controllo automatico degli aggiornamenti (una volta al giorno).
    #[serde(default)]
    pub check_updates: bool,
    /// Indirizzo HTTPS del manifest che descrive l'ultima versione.
    #[serde(default)]
    pub update_url: String,
    /// Secondi dall'epoca dell'ultimo controllo riuscito.
    #[serde(default)]
    pub last_update_check: i64,
}

fn default_history_days() -> i64 {
    365
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_engine: DUCKDUCKGO_ID.to_owned(),
            engines: vec![SearchEngine::duckduckgo()],
            theme: ThemePreference::System,
            start_page: String::new(),
            block_ads: true,
            block_third_party_cookies: true,
            privacy_signals: true,
            cookie_banner_helper: false,
            search_suggestions: true,
            cosmetic_blocking: true,
            unload_inactive_tabs: false,
            history_retention_days: default_history_days(),
            check_updates: false,
            update_url: String::new(),
            last_update_check: 0,
        }
    }
}

impl Settings {
    pub fn sanitize(mut self) -> Self {
        if let Some(ddg) = self
            .engines
            .iter_mut()
            .find(|engine| engine.id == DUCKDUCKGO_ID)
        {
            *ddg = SearchEngine::duckduckgo();
        } else {
            self.engines.insert(0, SearchEngine::duckduckgo());
        }

        self.engines
            .retain(|engine| engine.id == DUCKDUCKGO_ID || engine.is_valid_template());

        for engine in &mut self.engines {
            if engine.id != DUCKDUCKGO_ID {
                engine.builtin = false;
            }
        }

        if !self
            .engines
            .iter()
            .any(|engine| engine.id == self.default_engine)
        {
            self.default_engine = DUCKDUCKGO_ID.to_owned();
        }

        self
    }

    pub fn default_engine(&self) -> &SearchEngine {
        self.engines
            .iter()
            .find(|engine| engine.id == self.default_engine)
            .or_else(|| {
                self.engines
                    .iter()
                    .find(|engine| engine.id == DUCKDUCKGO_ID)
            })
            .expect("DuckDuckGo is always present after sanitize")
    }

    pub fn find_engine(&self, id: &str) -> Option<&SearchEngine> {
        self.engines.iter().find(|engine| engine.id == id)
    }

    /// Rimuove un motore personalizzato. Se era il predefinito, la ricerca
    /// torna a DuckDuckGo. DuckDuckGo non è rimovibile.
    pub fn remove_engine(&mut self, id: &str) -> bool {
        if id == DUCKDUCKGO_ID {
            return false;
        }
        let before = self.engines.len();
        self.engines.retain(|engine| engine.id != id);
        if self.engines.len() == before {
            return false;
        }
        if self.default_engine == id {
            self.default_engine = DUCKDUCKGO_ID.to_owned();
        }
        true
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabGroup {
    pub id: GroupId,
    pub name: String,
    #[serde(default = "default_group_color")]
    pub color: String,
    #[serde(default)]
    pub collapsed: bool,
}

fn default_group_color() -> String {
    GROUP_COLORS[0].to_owned()
}

impl TabGroup {
    pub fn color_index(&self) -> usize {
        GROUP_COLORS
            .iter()
            .position(|color| *color == self.color)
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    #[serde(default = "session_version")]
    pub version: u32,
    #[serde(default)]
    pub name: String,
    pub active_workspace: usize,
    pub workspaces: Vec<WorkspaceSession>,
}

impl Session {
    pub fn new(name: &str) -> Self {
        Self {
            version: session_version(),
            name: name.to_owned(),
            active_workspace: 0,
            workspaces: vec![WorkspaceSession {
                name: name.to_owned(),
                active_tab: 0,
                tabs: vec![TabSession::new_start_page()],
                groups: Vec::new(),
                profile: String::new(),
            }],
        }
    }

    pub fn tab_count(&self) -> usize {
        self.workspaces.iter().map(|workspace| workspace.tabs.len()).sum()
    }
}

fn session_version() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceSession {
    pub name: String,
    pub active_tab: usize,
    pub tabs: Vec<TabSession>,
    #[serde(default)]
    pub groups: Vec<TabGroup>,
    /// Identificatore stabile del profilo dati dello spazio (cartella WebKit
    /// separata). Vuoto nelle sessioni salvate prima dell'isolamento: in quel
    /// caso il browser assegna `default` al primo spazio e un profilo nuovo
    /// agli altri.
    #[serde(default)]
    pub profile: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabSession {
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub group: Option<GroupId>,
}

impl TabSession {
    pub fn new_start_page() -> Self {
        Self {
            uri: String::new(),
            title: "Nuova scheda".to_owned(),
            pinned: false,
            group: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bookmark {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub visited_at: i64,
    #[serde(default)]
    pub visits: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadEntry {
    pub id: i64,
    pub url: String,
    pub filename: String,
    pub destination: String,
    pub status: String,
    pub started_at: i64,
}

impl DownloadEntry {
    pub fn status_label(&self) -> &'static str {
        match self.status.as_str() {
            "completed" => "Completato",
            "failed" => "Non riuscito",
            "in_progress" => "In corso",
            _ => "Sconosciuto",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duckduckgo_keyword_is_valid() {
        assert_eq!(
            SearchEngine::duckduckgo().keyword_token().as_deref(),
            Some("d")
        );
    }

    #[test]
    fn keyword_rejects_spaces_and_dots() {
        assert_eq!(valid_keyword(" g "), Some("g".to_owned()));
        assert_eq!(valid_keyword("!w"), Some("w".to_owned()));
        assert_eq!(valid_keyword("g g"), None);
        assert_eq!(valid_keyword("example.com"), None);
        assert_eq!(valid_keyword("   "), None);
    }

    #[test]
    fn sanitize_keeps_duckduckgo_and_resets_default() {
        let mut settings = Settings::default();
        settings.engines.push(SearchEngine {
            id: "kagi".to_owned(),
            name: "Kagi".to_owned(),
            template: "https://kagi.com/search?q={query}".to_owned(),
            keyword: "k".to_owned(),
            builtin: false,
        });
        settings.default_engine = "kagi".to_owned();
        assert!(settings.remove_engine("kagi"));
        assert_eq!(settings.default_engine, DUCKDUCKGO_ID);
        assert!(!settings.remove_engine(DUCKDUCKGO_ID));
        assert_eq!(settings.engines.len(), 1);
    }

    #[test]
    fn privacy_settings_defaults_and_roundtrip() {
        let settings = Settings::default();
        assert!(settings.block_ads);
        assert!(settings.block_third_party_cookies);
        assert!(settings.privacy_signals);
        assert!(!settings.cookie_banner_helper);

        let mut settings = settings;
        settings.block_ads = false;
        settings.block_third_party_cookies = false;
        settings.cookie_banner_helper = true;
        let json = serde_json::to_string(&settings).expect("serialize");
        let restored: Settings = serde_json::from_str(&json).expect("deserialize");
        assert!(!restored.block_ads);
        assert!(!restored.block_third_party_cookies);
        assert!(restored.cookie_banner_helper);
        assert!(restored.privacy_signals);
    }

    #[test]
    fn session_roundtrip_keeps_groups_and_pinned_tabs() {
        let session = Session {
            version: session_version(),
            name: "Lavoro".to_owned(),
            active_workspace: 0,
            workspaces: vec![WorkspaceSession {
                name: "Lavoro".to_owned(),
                profile: "ws-1".to_owned(),
                active_tab: 1,
                tabs: vec![
                    TabSession {
                        uri: "https://example.com/a".to_owned(),
                        title: "A".to_owned(),
                        pinned: true,
                        group: None,
                    },
                    TabSession {
                        uri: "https://example.com/b".to_owned(),
                        title: "B".to_owned(),
                        pinned: false,
                        group: Some(7),
                    },
                ],
                groups: vec![TabGroup {
                    id: 7,
                    name: "Finanze".to_owned(),
                    color: "teal".to_owned(),
                    collapsed: true,
                }],
            }],
        };

        let json = serde_json::to_string(&session).expect("serialize");
        let restored: Session = serde_json::from_str(&json).expect("deserialize");

        let workspace = &restored.workspaces[0];
        assert_eq!(restored.name, "Lavoro");
        assert!(workspace.tabs[0].pinned);
        assert_eq!(workspace.tabs[1].group, Some(7));
        assert_eq!(workspace.groups[0].name, "Finanze");
        assert_eq!(workspace.groups[0].color, "teal");
        assert!(workspace.groups[0].collapsed);
        assert_eq!(restored.tab_count(), 2);
    }
}
