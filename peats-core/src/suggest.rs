//! Suggerimenti per la barra indirizzi.
//!
//! Qui vive solo la logica: il browser fornisce le schede aperte, i risultati
//! del database e le frasi ricevute da DuckDuckGo, e riceve indietro una lista
//! già ordinata e senza duplicati. Niente GTK e niente rete, così il
//! comportamento è verificabile con i test.

use serde_json::Value;

use crate::url::search_url;

/// Voci mostrate al massimo nel menu a tendina.
pub const LIMIT: usize = 8;

/// Endpoint di autocompletamento di DuckDuckGo. Restituisce un array JSON di
/// oggetti `{"phrase": "..."}`; non richiede chiavi e non invia cookie perché
/// la richiesta parte da una `NetworkSession` senza la pagina corrente come
/// referrer.
pub const DDG_AUTOCOMPLETE: &str = "https://duckduckgo.com/ac/?q={query}&type=list";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestionKind {
    Tab,
    Bookmark,
    History,
    Search,
}

impl SuggestionKind {
    /// Etichetta breve mostrata a destra nella riga.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tab => "Scheda",
            Self::Bookmark => "Segnalibro",
            Self::History => "Cronologia",
            Self::Search => "Ricerca",
        }
    }

    /// Icona simbolica del tema Adwaita.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Tab => "window-new-symbolic",
            Self::Bookmark => "starred-symbolic",
            Self::History => "document-open-recent-symbolic",
            Self::Search => "system-search-symbolic",
        }
    }
}

/// Una pagina candidata: scheda aperta, segnalibro o voce di cronologia.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub title: String,
    pub uri: String,
}

impl Candidate {
    pub fn new(title: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            uri: uri.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub kind: SuggestionKind,
    pub title: String,
    /// Riga secondaria: indirizzo della pagina o motore di ricerca.
    pub subtitle: String,
    pub uri: String,
}

/// Motore usato per le ricerche rapide.
pub struct EngineSpec<'a> {
    pub name: &'a str,
    pub template: &'a str,
}

/// URL dell'endpoint di autocompletamento per la query indicata.
pub fn ddg_url(query: &str) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    DDG_AUTOCOMPLETE.replace("{query}", &encoded)
}

/// Estrae le frasi dalla risposta di DuckDuckGo.
///
/// Accetta sia `[{"phrase": "..."}]` sia `["..."]` e ignora tutto ciò che non
/// riconosce: una risposta inattesa produce una lista vuota, non un errore.
pub fn parse_ddg(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };

    let mut phrases: Vec<String> = Vec::new();
    for item in items {
        let phrase = match item {
            Value::String(text) => Some(text.clone()),
            Value::Object(map) => map
                .get("phrase")
                .and_then(|phrase| phrase.as_str())
                .map(str::to_owned),
            _ => None,
        };
        let Some(phrase) = phrase else { continue };
        let phrase = phrase.trim().to_owned();
        if phrase.is_empty() || phrases.iter().any(|seen| seen.eq_ignore_ascii_case(&phrase)) {
            continue;
        }
        phrases.push(phrase);
        if phrases.len() >= LIMIT {
            break;
        }
    }
    phrases
}

fn matches(entry: &Candidate, needle: &str) -> bool {
    entry.title.to_lowercase().contains(needle) || entry.uri.to_lowercase().contains(needle)
}

/// Vero quando il testo digitato sembra un indirizzo e non una ricerca:
/// nessuno spazio e almeno un punto nel dominio (o `localhost`). Serve a non
/// interrogare il servizio di autocompletamento mentre si scrive un URL.
pub fn is_probably_url(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || text.contains(char::is_whitespace) {
        return false;
    }
    if text.starts_with("http://") || text.starts_with("https://") || text.starts_with("about:") {
        return true;
    }
    // Toglie percorso e porta prima di guardare il nome dell'host.
    let host = text.split('/').next().unwrap_or(text);
    let host = host.split(':').next().unwrap_or(host);
    host.eq_ignore_ascii_case("localhost") || host.contains('.')
}

/// Costruisce l'elenco mostrato sotto la barra indirizzi.
///
/// Ordine: schede aperte, segnalibri, cronologia, ricerche. Lo stesso URL
/// compare una volta sola, con la categoria più specifica (una scheda aperta
/// vince sul segnalibro, che vince sulla cronologia).
pub fn build(
    query: &str,
    tabs: &[Candidate],
    bookmarks: &[Candidate],
    history: &[Candidate],
    remote: &[String],
    engine: EngineSpec<'_>,
) -> Vec<Suggestion> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    let mut suggestions: Vec<Suggestion> = Vec::new();

    let push = |kind: SuggestionKind, entry: &Candidate, out: &mut Vec<Suggestion>| {
        if out.len() >= LIMIT || out.iter().any(|seen| seen.uri == entry.uri) {
            return;
        }
        out.push(Suggestion {
            kind,
            title: if entry.title.trim().is_empty() {
                entry.uri.clone()
            } else {
                entry.title.clone()
            },
            subtitle: entry.uri.clone(),
            uri: entry.uri.clone(),
        });
    };

    for entry in tabs.iter().filter(|entry| matches(entry, &needle)) {
        push(SuggestionKind::Tab, entry, &mut suggestions);
    }
    for entry in bookmarks.iter().filter(|entry| matches(entry, &needle)) {
        push(SuggestionKind::Bookmark, entry, &mut suggestions);
    }
    for entry in history.iter().filter(|entry| matches(entry, &needle)) {
        push(SuggestionKind::History, entry, &mut suggestions);
    }

    for phrase in remote {
        if suggestions.len() >= LIMIT {
            break;
        }
        let uri = search_url(engine.template, phrase);
        if suggestions.iter().any(|seen| seen.uri == uri) {
            continue;
        }
        suggestions.push(Suggestion {
            kind: SuggestionKind::Search,
            title: phrase.clone(),
            subtitle: format!("Cerca con {}", engine.name),
            uri,
        });
    }

    // Senza rete (o con i suggerimenti remoti disattivati) la ricerca diretta
    // resta raggiungibile dallo stesso elenco.
    if remote.is_empty() && suggestions.len() < LIMIT {
        suggestions.push(Suggestion {
            kind: SuggestionKind::Search,
            title: format!("Cerca «{query}» con {}", engine.name),
            subtitle: engine.template.to_owned(),
            uri: search_url(engine.template, query),
        });
    }

    suggestions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> EngineSpec<'static> {
        EngineSpec {
            name: "DuckDuckGo",
            template: "https://duckduckgo.com/?q={query}",
        }
    }

    #[test]
    fn ddg_url_encodes_the_query() {
        let url = ddg_url("rust gtk");
        // `byte_serialize` applica la codifica dei form: lo spazio diventa `+`,
        // come già accade per le ricerche normali (url::search_url).
        assert!(url.contains("q=rust+gtk"), "{url}");
        assert!(url.ends_with("&type=list"));
    }

    #[test]
    fn parse_ddg_reads_phrases_and_skips_junk() {
        let body = r#"[{"phrase":"rust"},{"phrase":"rust gtk"},{"phrase":"rust"},42,{"nope":"x"},"gtk"]"#;
        assert_eq!(parse_ddg(body), vec!["rust", "rust gtk", "gtk"]);
    }

    #[test]
    fn parse_ddg_tolerates_unexpected_payloads() {
        assert!(parse_ddg("").is_empty());
        assert!(parse_ddg("<html>").is_empty());
        assert!(parse_ddg(r#"{"phrase":"rust"}"#).is_empty());
    }

    #[test]
    fn urls_are_recognized_and_queries_are_not() {
        assert!(is_probably_url("example.com"));
        assert!(is_probably_url("https://rust-lang.org/learn"));
        assert!(is_probably_url("localhost:3000"));
        assert!(is_probably_url("about:settings"));
        assert!(!is_probably_url("rust gtk"));
        assert!(!is_probably_url(""));
        assert!(!is_probably_url("rust"));
    }

    #[test]
    fn build_orders_tabs_then_bookmarks_then_history_without_duplicates() {
        let tabs = vec![Candidate::new("Documentazione", "https://doc.example/rust")];
        let bookmarks = vec![
            Candidate::new("Rust", "https://doc.example/rust"),
            Candidate::new("Rust blog", "https://blog.example/rust"),
        ];
        let history = vec![
            Candidate::new("Rust", "https://doc.example/rust"),
            Candidate::new("Rust weekly", "https://weekly.example/rust"),
        ];

        let list = build("rust", &tabs, &bookmarks, &history, &[], engine());
        let kinds: Vec<SuggestionKind> = list.iter().map(|item| item.kind).collect();
        assert_eq!(
            kinds,
            vec![
                SuggestionKind::Tab,
                SuggestionKind::Bookmark,
                SuggestionKind::History,
                SuggestionKind::Search,
            ]
        );
        assert_eq!(list[1].uri, "https://blog.example/rust");
        assert_eq!(list[3].uri, "https://duckduckgo.com/?q=rust");
    }

    #[test]
    fn build_lists_remote_phrases_before_the_direct_search() {
        let remote = vec!["rust lang".to_owned(), "rust gtk".to_owned()];
        let list = build("rust", &[], &[], &[], &remote, engine());

        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|item| item.kind == SuggestionKind::Search));
        assert_eq!(list[0].uri, "https://duckduckgo.com/?q=rust+lang");
    }

    #[test]
    fn build_respects_the_limit_and_ignores_empty_queries() {
        let tabs: Vec<Candidate> = (0..20)
            .map(|index| Candidate::new(format!("Scheda {index}"), format!("https://e.example/{index}")))
            .collect();
        assert_eq!(build("scheda", &tabs, &[], &[], &[], engine()).len(), LIMIT);
        assert!(build("   ", &tabs, &[], &[], &[], engine()).is_empty());
    }
}
