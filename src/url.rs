use url::Url;

pub const DUCKDUCKGO_TEMPLATE: &str = "https://duckduckgo.com/?q={query}";

/// Un motore interrogabile tramite parola chiave nella barra indirizzi.
pub struct KeywordEngine<'a> {
    pub keyword: &'a str,
    pub template: &'a str,
}

/// Risolve il testo digitato nella barra indirizzi.
///
/// The function is intentionally conservative: a single word such as `rust`
/// is a search query, while `rust-lang.org`, `localhost:3000` and explicit
/// `http(s)://` URLs are navigated to directly.
pub fn resolve_input(input: &str, search_template: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    if let Some(url) = explicit_url(input) {
        return Some(url);
    }

    if looks_like_navigable_host(input) {
        let candidate = format!("https://{input}");
        if is_valid_navigation_url(&candidate) {
            return Some(candidate);
        }
    }

    Some(search_url(search_template, input))
}

/// Come [`resolve_input`], ma riconosce le scorciatoie dei motori di ricerca:
/// `g rust` e `!g rust` cercano «rust» sul motore con parola chiave `g`.
pub fn resolve_input_with_keywords(
    input: &str,
    search_template: &str,
    engines: &[KeywordEngine<'_>],
) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    if let Some((index, query)) = keyword_target(input, engines) {
        return Some(search_url(engines[index].template, &query));
    }

    resolve_input(input, search_template)
}

fn keyword_target(input: &str, engines: &[KeywordEngine<'_>]) -> Option<(usize, String)> {
    let body = input.strip_prefix('!').unwrap_or(input);
    let boundary = body.find(char::is_whitespace)?;
    let token = body[..boundary].to_ascii_lowercase();
    if token.is_empty() {
        return None;
    }
    let query = body[boundary..].trim();
    if query.is_empty() {
        return None;
    }

    engines
        .iter()
        .position(|engine| engine.keyword.eq_ignore_ascii_case(&token))
        .map(|index| (index, query.to_owned()))
}

fn explicit_url(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();

    if lower.starts_with("http://") || lower.starts_with("https://") {
        return is_valid_navigation_url(input).then(|| input.to_owned());
    }

    if lower.starts_with("file://") {
        return Url::parse(input).ok().map(|_| input.to_owned());
    }

    // Le pagine interne (`about:settings`, `about:blank`, ...) sono gestite
    // dal browser; qui vengono solo riconosciute.
    if lower.starts_with("about:") && !input.contains(char::is_whitespace) {
        return Some(input.to_owned());
    }

    // Never hand arbitrary schemes to the web view from the address bar.
    None
}

fn is_valid_navigation_url(input: &str) -> bool {
    let Ok(url) = Url::parse(input) else {
        return false;
    };

    matches!(url.scheme(), "http" | "https") && url.host().is_some()
}

fn looks_like_navigable_host(input: &str) -> bool {
    if input.contains(char::is_whitespace) || input.contains('@') {
        return false;
    }

    if input.starts_with('/') || input.starts_with('.') || input.starts_with('-') {
        return false;
    }

    if input.starts_with('[') {
        return input.contains("]:") || input.ends_with(']');
    }

    if input.eq_ignore_ascii_case("localhost") || input.starts_with("localhost:") {
        return true;
    }

    if let Some(host) = input.split(['/', ':']).next() {
        if host.parse::<std::net::IpAddr>().is_ok() {
            return true;
        }

        // A dot is required for normal domain names. This keeps single words
        // such as "rust" in the search path.
        if host.contains('.') && !host.starts_with('.') && !host.ends_with('.') {
            return true;
        }
    }

    false
}

pub fn search_url(template: &str, query: &str) -> String {
    let encoded: String =
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();

    if template.contains("{query}") {
        template.replace("{query}", &encoded)
    } else if template.contains('?') {
        format!("{template}&q={encoded}")
    } else {
        format!("{template}?q={encoded}")
    }
}

pub fn is_history_url(url: &str) -> bool {
    matches!(
        Url::parse(url).map(|u| u.scheme().to_owned()).as_deref(),
        Ok("http") | Ok("https")
    )
}

/// Origine (`scheme://host`) usata come chiave per lo zoom per sito.
pub fn origin_of(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let host = url.host_str()?;
    // La porta fa parte dell'origine quando non è quella predefinita: due
    // servizi sullo stesso host ma su porte diverse non devono condividere
    // lo zoom.
    Some(match url.port() {
        Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
        None => format!("{}://{}", url.scheme(), host),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DDG: &str = DUCKDUCKGO_TEMPLATE;

    fn engines() -> Vec<KeywordEngine<'static>> {
        vec![
            KeywordEngine {
                keyword: "d",
                template: DUCKDUCKGO_TEMPLATE,
            },
            KeywordEngine {
                keyword: "g",
                template: "https://www.google.com/search?q={query}",
            },
        ]
    }

    #[test]
    fn navigates_domains() {
        assert_eq!(
            resolve_input("rust-lang.org", DDG).as_deref(),
            Some("https://rust-lang.org")
        );
        assert_eq!(
            resolve_input("example.com/path?q=1", DDG).as_deref(),
            Some("https://example.com/path?q=1")
        );
    }

    #[test]
    fn navigates_local_hosts() {
        assert_eq!(
            resolve_input("localhost:3000", DDG).as_deref(),
            Some("https://localhost:3000")
        );
        assert_eq!(
            resolve_input("127.0.0.1:8080", DDG).as_deref(),
            Some("https://127.0.0.1:8080")
        );
    }

    #[test]
    fn keeps_explicit_urls() {
        assert_eq!(
            resolve_input("https://example.com/a", DDG).as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(
            resolve_input("about:blank", DDG).as_deref(),
            Some("about:blank")
        );
        assert_eq!(
            resolve_input("about:settings", DDG).as_deref(),
            Some("about:settings")
        );
    }

    #[test]
    fn searches_plain_text() {
        assert_eq!(
            resolve_input("rust programming", DDG).as_deref(),
            Some("https://duckduckgo.com/?q=rust+programming")
        );
        assert_eq!(
            resolve_input("rust", DDG).as_deref(),
            Some("https://duckduckgo.com/?q=rust")
        );
        assert_eq!(
            resolve_input("a&b", DDG).as_deref(),
            Some("https://duckduckgo.com/?q=a%26b")
        );
    }

    #[test]
    fn does_not_allow_javascript_scheme() {
        assert_eq!(
            resolve_input("javascript:alert(1)", DDG).as_deref(),
            Some("https://duckduckgo.com/?q=javascript%3Aalert%281%29")
        );
    }

    #[test]
    fn custom_template() {
        assert_eq!(
            resolve_input("ciao", "https://search.example/?text={query}").as_deref(),
            Some("https://search.example/?text=ciao")
        );
    }

    #[test]
    fn empty_input_is_ignored() {
        assert_eq!(resolve_input("   ", DDG), None);
    }

    #[test]
    fn keyword_shortcut_selects_engine() {
        assert_eq!(
            resolve_input_with_keywords("g rust", DDG, &engines()).as_deref(),
            Some("https://www.google.com/search?q=rust")
        );
        assert_eq!(
            resolve_input_with_keywords("!g rust programming", DDG, &engines()).as_deref(),
            Some("https://www.google.com/search?q=rust+programming")
        );
    }

    #[test]
    fn keyword_does_not_hijack_domains() {
        assert_eq!(
            resolve_input_with_keywords("g.example.com", DDG, &engines()).as_deref(),
            Some("https://g.example.com")
        );
        assert_eq!(
            resolve_input_with_keywords("g", DDG, &engines()).as_deref(),
            Some("https://duckduckgo.com/?q=g")
        );
        assert_eq!(
            resolve_input_with_keywords("http://g.com", DDG, &engines()).as_deref(),
            Some("http://g.com")
        );
    }

    #[test]
    fn origin_is_scheme_and_host() {
        assert_eq!(
            origin_of("https://example.com/a/b?c=1").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(origin_of("about:blank"), None);
        assert_eq!(
            origin_of("http://127.0.0.1:8765/plain.html").as_deref(),
            Some("http://127.0.0.1:8765")
        );
        // La porta predefinita noncompare nell'origine.
        assert_eq!(
            origin_of("https://example.com:443/x").as_deref(),
            Some("https://example.com")
        );
    }
}
