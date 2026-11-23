//! Importazione di cronologia, segnalibri e account/password da file esportati
//! da altri browser.
//!
//! Formati riconosciuti, scelti dal contenuto e non solo dal nome del file:
//!
//! - database SQLite di Chrome/Edge (`History`, tabella `urls`) e di Firefox
//!   (`places.sqlite`, tabelle `moz_places` e `moz_bookmarks`);
//! - segnalibri HTML esportati (formato Netscape, `<A HREF=…>`);
//! - CSV con intestazione `url,title[,visited_at]` oppure
//!   `url,username,password` (Chrome e Firefox usano nomi di colonna diversi,
//!   che vengono riconosciuti);
//! - JSON: lista di oggetti con `url` + `password`, `url` + `title` oppure solo
//!   `url`.
//!
//! Tutto viene letto in locale; i file non vengono mai inviati altrove.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use url::Url;

#[derive(Debug, Clone)]
pub struct HistoryVisit {
    pub url: String,
    pub title: String,
    /// Secondi dall'epoca Unix, quando il file li contiene.
    pub visited_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct BookmarkItem {
    pub url: String,
    pub title: String,
}

/// Un account: sito, nome utente e password.
#[derive(Debug, Clone)]
pub struct Credential {
    pub url: String,
    pub username: String,
    pub password: String,
}

impl Credential {
    pub fn host(&self) -> String {
        host_of(&self.url)
    }
}

#[derive(Debug, Default)]
pub struct ImportData {
    pub history: Vec<HistoryVisit>,
    pub bookmarks: Vec<BookmarkItem>,
    pub credentials: Vec<Credential>,
    /// Cose che l'utente deve sapere (per esempio password non importabili).
    pub notes: Vec<String>,
}

impl ImportData {
    pub fn is_empty(&self) -> bool {
        self.history.is_empty() && self.bookmarks.is_empty() && self.credentials.is_empty()
    }
}

/// Host normalizzato di un indirizzo, usato per raggruppare gli account.
pub fn host_of(url: &str) -> String {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();

    // Con uno schema esplicito si accettano solo gli schemi web:
    // `chrome://settings` non ha un host e resta com'è.
    if lower.contains("://") {
        if let Ok(parsed) = Url::parse(trimmed) {
            if matches!(parsed.scheme(), "http" | "https" | "ftp") {
                if let Some(host) = parsed.host_str() {
                    if !host.is_empty() {
                        return host.to_ascii_lowercase();
                    }
                }
            }
        }
        return lower;
    }

    // Nome di host senza schema («esempio.test», «esempio.test/login»).
    if let Ok(parsed) = Url::parse(&format!("https://{trimmed}")) {
        if let Some(host) = parsed.host_str() {
            if !host.is_empty() {
                return host.to_ascii_lowercase();
            }
        }
    }
    lower
}

/// Interpreta il file e restituisce quello che contiene.
pub fn parse(bytes: &[u8], filename: &str) -> Result<ImportData> {
    if bytes.starts_with(b"SQLite format 3\0") {
        return read_sqlite_bytes(bytes);
    }

    let text = std::str::from_utf8(bytes)
        .context("il file non è testo UTF-8 (per i database serve il file SQLite originale)")?
        .trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        bail!("Il file è vuoto.");
    }

    let trimmed = text.trim_start();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        return parse_json(trimmed);
    }

    let lower = text.to_ascii_lowercase();
    if lower.contains("<a href") || lower.contains("netscape-bookmark-file") {
        let data = parse_bookmarks_html(text);
        if data.bookmarks.is_empty() {
            bail!("Nessun segnalibro trovato nell'HTML.");
        }
        return Ok(data);
    }

    let rows = parse_csv(text);
    if rows.is_empty() {
        bail!("Formato non riconosciuto ({filename}): usa un file SQLite, HTML, CSV o JSON.");
    }
    parse_csv_rows(&rows)
}

// ---------------------------------------------------------------- SQLite

fn read_sqlite_bytes(bytes: &[u8]) -> Result<ImportData> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("peats-import-{}-{stamp}.sqlite", std::process::id()));
    std::fs::write(&path, bytes).context("copia temporanea del database non riuscita")?;
    let result = read_sqlite(&path);
    let _ = std::fs::remove_file(&path);
    result
}

fn read_sqlite(path: &Path) -> Result<ImportData> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("database SQLite non leggibile")?;
    let mut data = ImportData::default();

    // Chrome, Edge, Chromium, Brave…
    if has_table(&connection, "urls") {
        let mut statement = connection.prepare(
            "SELECT url, COALESCE(title, ''), COALESCE(last_visit_time, 0)
             FROM urls WHERE url LIKE 'http%'",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (url, title, visited) = row?;
            data.history.push(HistoryVisit {
                url,
                title,
                visited_at: chrome_time(visited),
            });
        }
    }

    // Firefox
    if has_table(&connection, "moz_places") {
        let mut statement = connection.prepare(
            "SELECT url, COALESCE(title, ''), COALESCE(last_visit_date, 0)
             FROM moz_places WHERE url LIKE 'http%'",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (url, title, visited) = row?;
            data.history.push(HistoryVisit {
                url,
                title,
                visited_at: firefox_time(visited),
            });
        }
    }

    if has_table(&connection, "moz_bookmarks") {
        let mut statement = connection.prepare(
            "SELECT p.url, COALESCE(NULLIF(b.title, ''), p.title)
             FROM moz_bookmarks b JOIN moz_places p ON p.id = b.fk
             WHERE b.type = 1 AND p.url LIKE 'http%'",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(BookmarkItem {
                url: row.get(0)?,
                title: row.get(1)?,
            })
        })?;
        for row in rows {
            data.bookmarks.push(row?);
        }
    }

    if has_table(&connection, "logins") {
        data.notes.push(
            "Le password salvate da Chrome/Edge sono cifrate dal sistema e non si possono \
             importare da qui: esportale in CSV (chrome://password-manager/settings)."
                .to_owned(),
        );
    }

    if data.is_empty() && data.notes.is_empty() {
        bail!("Il database non contiene tabelle di cronologia o segnalibri riconosciute.");
    }
    Ok(data)
}

fn has_table(connection: &Connection, name: &str) -> bool {
    connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .unwrap_or(false)
}

/// Chrome conta i microsecondi dal 1601-01-01; qui diventano secondi Unix.
fn chrome_time(value: i64) -> Option<i64> {
    if value <= 0 {
        None
    } else {
        Some(value / 1_000_000 - 11_644_473_600)
    }
}

/// Firefox salva i microsecondi dall'epoca Unix.
fn firefox_time(value: i64) -> Option<i64> {
    if value <= 0 {
        None
    } else {
        Some(value / 1_000_000)
    }
}

// ---------------------------------------------------------------- JSON

fn parse_json(text: &str) -> Result<ImportData> {
    let value: Value = serde_json::from_str(text).context("JSON non valido")?;
    let items = value
        .as_array()
        .ok_or_else(|| anyhow!("Il JSON deve essere una lista di oggetti."))?;

    let mut data = ImportData::default();
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        let url = object
            .get("url")
            .or_else(|| object.get("origin_url"))
            .or_else(|| object.get("hostname"))
            .and_then(Value::as_str);
        let Some(url) = url else {
            continue;
        };

        let password = object
            .get("password")
            .or_else(|| object.get("password_value"))
            .and_then(Value::as_str);
        if let Some(password) = password {
            let username = object
                .get("username")
                .or_else(|| object.get("username_value"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            data.credentials.push(Credential {
                url: url.to_owned(),
                username: username.to_owned(),
                password: password.to_owned(),
            });
        } else if let Some(title) = object.get("title").and_then(Value::as_str) {
            data.history.push(HistoryVisit {
                url: url.to_owned(),
                title: title.to_owned(),
                visited_at: object.get("visited_at").and_then(Value::as_i64),
            });
        } else {
            data.bookmarks.push(BookmarkItem {
                url: url.to_owned(),
                title: object
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
    }
    Ok(data)
}

// ---------------------------------------------------------------- HTML

fn parse_bookmarks_html(text: &str) -> ImportData {
    let lower = text.to_ascii_lowercase();
    let mut data = ImportData::default();
    let mut seen = HashSet::new();
    let mut cursor = 0usize;

    while let Some(found) = lower[cursor..].find("<a ") {
        let tag_start = cursor + found;
        let Some(offset) = lower[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + offset;
        let tag = &text[tag_start..tag_end];

        if let Some(href) = attribute(tag, "href") {
            let href = unescape(&href);
            if href.starts_with("http") && seen.insert(href.clone()) {
                let close = lower[tag_end..]
                    .find("</a>")
                    .map(|offset| tag_end + offset)
                    .unwrap_or(tag_end);
                let title = unescape(text[tag_end + 1..close].trim());
                data.bookmarks.push(BookmarkItem {
                    url: href.clone(),
                    title: if title.is_empty() { href } else { title },
                });
            }
        }
        cursor = tag_end + 1;
    }
    data
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let start = lower.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[start..];
    let first = rest.chars().next()?;
    if first == '"' || first == '\'' {
        let body = &rest[1..];
        let end = body.find(first)?;
        Some(body[..end].to_owned())
    } else {
        let end = rest
            .find(|character: char| character.is_whitespace() || character == '>')
            .unwrap_or(rest.len());
        Some(rest[..end].to_owned())
    }
}

fn unescape(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        output.push_str(&rest[..index]);
        let tail = &rest[index..];
        let Some(semicolon) = tail.find(';') else {
            output.push_str(tail);
            return output;
        };
        let entity = &tail[1..semicolon];
        match decode_entity(entity) {
            Some(character) => output.push(character),
            None => output.push_str(&tail[..=semicolon]),
        }
        rest = &tail[semicolon + 1..];
    }
    output.push_str(rest);
    output
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let digits = entity.strip_prefix('#')?;
            let value = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => digits.parse::<u32>().ok(),
            };
            value.and_then(char::from_u32)
        }
    }
}

// ---------------------------------------------------------------- CSV

/// Parser CSV minimo ma corretto: gestisce virgolette, virgole e righe
/// spezzate dentro i campi, come fanno le esportazioni dei browser.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        if quoted {
            if character == '"' {
                if characters.peek() == Some(&'"') {
                    characters.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(character);
            }
            continue;
        }
        match character {
            '"' => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(character),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.retain(|row| !(row.len() == 1 && row[0].trim().is_empty()));
    rows
}

fn parse_csv_rows(rows: &[Vec<String>]) -> Result<ImportData> {
    let header: Vec<String> = rows[0]
        .iter()
        .map(|cell| cell.trim().to_ascii_lowercase())
        .collect();
    let find = |names: &[&str]| {
        header
            .iter()
            .position(|cell| names.contains(&cell.as_str()))
    };

    let mut data = ImportData::default();

    let password_index = find(&["password"]);
    if let (Some(url_index), Some(password_index)) = (find(&["url", "origin_url", "hostname"]), password_index)
    {
        let username_index = find(&["username", "username_value"]);
        for row in &rows[1..] {
            let url = cell(row, url_index);
            let password = cell(row, password_index);
            if url.is_empty() || password.is_empty() {
                continue;
            }
            data.credentials.push(Credential {
                url,
                username: username_index.map(|index| cell(row, index)).unwrap_or_default(),
                password,
            });
        }
        return Ok(data);
    }

    if let Some(url_index) = find(&["url"]) {
        let title_index = find(&["title", "name"]);
        let time_index = find(&["visited_at", "timestamp", "last_visit"]);
        for row in &rows[1..] {
            let url = cell(row, url_index);
            if !url.starts_with("http") {
                continue;
            }
            data.history.push(HistoryVisit {
                url,
                title: title_index.map(|index| cell(row, index)).unwrap_or_default(),
                visited_at: time_index.and_then(|index| cell(row, index).parse::<i64>().ok()),
            });
        }
        return Ok(data);
    }

    bail!("Intestazione non riconosciuta: servono le colonne url/title oppure url/username/password.")
}

fn cell(row: &[String], index: usize) -> String {
    row.get(index)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_handles_quotes_commas_and_newlines() {
        let rows = parse_csv("url,title\n\"https://a.example/x,y\",\"Titolo, con virgola\"\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][0], "https://a.example/x,y");
        assert_eq!(rows[1][1], "Titolo, con virgola");

        let rows = parse_csv("url,password\r\n\"https://a.example\",\"p\"\"a\"\r\n");
        assert_eq!(rows[1][1], "p\"a");
    }

    #[test]
    fn chrome_password_csv_is_detected() {
        let csv = "name,url,username,password,note\n\
                   Esempio,https://esempio.test/login,mario,segreta,\n";
        let data = parse(csv.as_bytes(), "passwords.csv").unwrap();
        assert_eq!(data.credentials.len(), 1);
        assert_eq!(data.credentials[0].username, "mario");
        assert_eq!(data.credentials[0].password, "segreta");
        assert_eq!(data.credentials[0].host(), "esempio.test");
    }

    #[test]
    fn firefox_password_csv_is_detected() {
        let csv = "\"url\",\"username\",\"password\",\"httpRealm\",\"formActionOrigin\"\n\
                   \"https://esempio.test\",\"mario\",\"segreta\",,\n";
        let data = parse(csv.as_bytes(), "logins.csv").unwrap();
        assert_eq!(data.credentials.len(), 1);
        assert_eq!(data.credentials[0].host(), "esempio.test");
    }

    #[test]
    fn history_csv_keeps_timestamps() {
        let csv = "url,title,visited_at\nhttps://esempio.test,Esempio,1700000000\n";
        let data = parse(csv.as_bytes(), "history.csv").unwrap();
        assert_eq!(data.history.len(), 1);
        assert_eq!(data.history[0].visited_at, Some(1_700_000_000));
    }

    #[test]
    fn bookmarks_html_unescapes_titles() {
        let html = "<!DOCTYPE NETSCAPE-Bookmark-file-1>\n\
                    <DT><A HREF=\"https://esempio.test/?a=1&amp;b=2\" ADD_DATE=\"1\">Esempio &amp; prova</A>\n\
                    <DT><A HREF=\"place:sort=8\">Cartella</A>\n";
        let data = parse(html.as_bytes(), "bookmarks.html").unwrap();
        assert_eq!(data.bookmarks.len(), 1);
        assert_eq!(data.bookmarks[0].url, "https://esempio.test/?a=1&b=2");
        assert_eq!(data.bookmarks[0].title, "Esempio & prova");
    }

    #[test]
    fn json_array_splits_history_credentials_and_bookmarks() {
        let json = r#"[
            {"url": "https://a.test", "title": "A", "visited_at": 1700000000},
            {"url": "https://b.test", "username": "mario", "password": "segreta"},
            {"url": "https://c.test"}
        ]"#;
        let data = parse(json.as_bytes(), "dati.json").unwrap();
        assert_eq!(data.history.len(), 1);
        assert_eq!(data.credentials.len(), 1);
        assert_eq!(data.bookmarks.len(), 1);
    }

    #[test]
    fn chrome_sqlite_history_is_imported() {
        let path = std::env::temp_dir().join(format!(
            "peats-import-test-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE urls (url TEXT, title TEXT, last_visit_time INTEGER);
                     INSERT INTO urls VALUES
                        ('https://a.test/', 'A', 13300000000000000),
                        ('chrome://settings', 'Impostazioni', 13300000000000000);",
                )
                .unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let data = parse(&bytes, "History").unwrap();
        assert_eq!(data.history.len(), 1, "chrome:// viene scartato");
        assert_eq!(data.history[0].url, "https://a.test/");
        // 13300000000000000 µs dal 1601 → 2022-05-03 circa.
        let visited = data.history[0].visited_at.unwrap();
        assert!(visited > 1_600_000_000 && visited < 1_800_000_000, "{visited}");
    }

    #[test]
    fn hosts_are_normalised() {
        assert_eq!(host_of("https://Esempio.Test/login"), "esempio.test");
        assert_eq!(host_of("esempio.test"), "esempio.test");
        assert_eq!(host_of("chrome://settings"), "chrome://settings");
    }
}
