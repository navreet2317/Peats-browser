use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::model::{Bookmark, DownloadEntry, HistoryEntry, SitePermission};

const APP_DIR: &str = "peats-browser";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryOrder {
    Recent,
    MostVisited,
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub database: PathBuf,
    pub settings: PathBuf,
    pub session: PathBuf,
    pub sessions_dir: PathBuf,
    pub extensions_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let data_dir = dirs::data_dir()
            .context("XDG data directory is not available")?
            .join(APP_DIR);
        let config_dir = dirs::config_dir()
            .context("XDG config directory is not available")?
            .join(APP_DIR);
        let cache_dir = dirs::cache_dir()
            .context("XDG cache directory is not available")?
            .join(APP_DIR);

        let sessions_dir = config_dir.join("sessions");
        let extensions_dir = data_dir.join("extensions");

        fs::create_dir_all(&data_dir)?;
        fs::create_dir_all(&config_dir)?;
        fs::create_dir_all(&cache_dir)?;
        fs::create_dir_all(&sessions_dir)?;
        fs::create_dir_all(&extensions_dir)?;

        Ok(Self {
            database: data_dir.join("peats.sqlite3"),
            settings: config_dir.join("settings.json"),
            session: config_dir.join("session.json"),
            sessions_dir,
            extensions_dir,
            data_dir,
            cache_dir,
        })
    }

    pub fn download_dir(&self) -> PathBuf {
        dirs::download_dir().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Downloads")
        })
    }

    /// Percorso del file JSON di una sessione nominata.
    pub fn named_session(&self, slug: &str) -> PathBuf {
        self.sessions_dir.join(format!("{slug}.json"))
    }
}

/// Account importato: `nonce` e `secret` restano cifrati finché non vengono
/// copiati dall'utente.
#[derive(Clone, Debug)]
pub struct StoredCredential {
    pub id: i64,
    pub host: String,
    pub url: String,
    pub username: String,
    pub nonce: Vec<u8>,
    pub secret: Vec<u8>,
}

pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(3))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            -- WAL + NORMAL: le scritture non aspettano un fsync completo a ogni
            -- pagina; un crash può perdere solo gli ultimi commit, non
            -- corrompere il database.
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS history (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                url         TEXT NOT NULL,
                title       TEXT NOT NULL DEFAULT '',
                visited_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_history_visited
                ON history(visited_at DESC);
            CREATE INDEX IF NOT EXISTS idx_history_url
                ON history(url);

            CREATE TABLE IF NOT EXISTS bookmarks (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                url         TEXT NOT NULL UNIQUE,
                title       TEXT NOT NULL,
                created_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS downloads (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                url          TEXT NOT NULL,
                filename     TEXT NOT NULL,
                destination  TEXT NOT NULL,
                status       TEXT NOT NULL,
                started_at   INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS site_zoom (
                origin  TEXT PRIMARY KEY,
                zoom    REAL NOT NULL
            );

            CREATE TABLE IF NOT EXISTS site_permissions (
                origin      TEXT NOT NULL,
                kind        TEXT NOT NULL,
                decision    TEXT NOT NULL,
                updated_at  INTEGER NOT NULL,
                PRIMARY KEY (origin, kind)
            );

            CREATE TABLE IF NOT EXISTS block_exceptions (
                origin      TEXT PRIMARY KEY,
                created_at  INTEGER NOT NULL
            );

            -- Account e password importati. Il segreto è cifrato con la chiave
            -- che sta nel portachiavi di sistema: il solo database non basta
            -- per leggerlo.
            CREATE TABLE IF NOT EXISTS credentials (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                host       TEXT NOT NULL,
                url        TEXT NOT NULL,
                username   TEXT NOT NULL,
                secret     BLOB NOT NULL,
                nonce      BLOB NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE (host, username)
            );

            -- Ricerca testuale della cronologia (suggerimenti della barra
            -- indirizzi). Tabella esterna con triggers: il contenuto resta in
            -- `history`, l'indice è sempre coerente anche con le cancellazioni.
            CREATE VIRTUAL TABLE IF NOT EXISTS history_fts USING fts5(
                title,
                url,
                content='history',
                content_rowid='id',
                tokenize='unicode61 remove_diacritics 2'
            );
            CREATE TRIGGER IF NOT EXISTS history_ai AFTER INSERT ON history BEGIN
                INSERT INTO history_fts(rowid, title, url)
                VALUES (new.id, new.title, new.url);
            END;
            CREATE TRIGGER IF NOT EXISTS history_ad AFTER DELETE ON history BEGIN
                INSERT INTO history_fts(history_fts, rowid, title, url)
                VALUES ('delete', old.id, old.title, old.url);
            END;
            CREATE TRIGGER IF NOT EXISTS history_au AFTER UPDATE ON history BEGIN
                INSERT INTO history_fts(history_fts, rowid, title, url)
                VALUES ('delete', old.id, old.title, old.url);
                INSERT INTO history_fts(rowid, title, url)
                VALUES (new.id, new.title, new.url);
            END;
            ",
        )?;

        // Le righe rimaste «in corso» da una sessione precedente non sono più
        // scaricabili: vengono marcate come non riuscite invece di restare
        // per sempre in sospeso nella finestra dei download.
        conn.execute(
            "UPDATE downloads SET status = 'failed' WHERE status = 'in_progress'",
            [],
        )?;

        // Database creato prima dell'indice FTS: l'indice viene ricostruito una
        // volta sola dai dati già presenti.
        let history_rows: i64 = conn.query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?;
        let indexed_rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM history_fts", [], |row| row.get(0))?;
        if history_rows > 0 && indexed_rows == 0 {
            conn.execute("INSERT INTO history_fts(history_fts) VALUES('rebuild')", [])?;
        }

        Ok(Self { conn })
    }

    /// Ricerca nell'indice FTS5: ogni parola diventa un prefisso (`"rus"*`),
    /// così «rust» trova anche «Rustico». Le virgolette vengono rimosse per
    /// non costruire espressioni non valide.
    fn search_history_fts(&self, term: &str, limit: u32) -> Result<Vec<HistoryEntry>> {
        let mut tokens: Vec<String> = Vec::new();
        for token in term.split_whitespace() {
            let token = token.replace('"', "");
            if token.is_empty() {
                continue;
            }
            tokens.push(format!("\"{token}\"*"));
        }
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        let mut statement = self.conn.prepare(
            "SELECT MAX(h.id) AS id,
                    h.url,
                    COALESCE((
                        SELECT x.title FROM history x
                        WHERE x.url = h.url
                        ORDER BY x.visited_at DESC, x.id DESC
                        LIMIT 1
                    ), h.url) AS title,
                    MAX(h.visited_at) AS visited_at,
                    COUNT(*) AS visits
             FROM history h
             WHERE h.id IN (
                 SELECT rowid FROM history_fts WHERE history_fts MATCH ?1
             )
             GROUP BY h.url
             ORDER BY MAX(h.visited_at) DESC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![tokens.join(" "), limit], |row| {
            Ok(HistoryEntry {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                visited_at: row.get(3)?,
                visits: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Solo per i test: il browser passa da `add_history_many`.
    #[cfg(test)]
    pub fn add_history(&self, url: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO history (url, title, visited_at) VALUES (?1, ?2, ?3)",
            params![url, title, now_epoch()],
        )?;
        Ok(())
    }

    /// Registra più visite in un'unica transazione: il browser raccoglie le
    /// navigazioni e le scrive a gruppi invece di fare una `INSERT` per
    /// caricamento.
    pub fn add_history_many(&self, visits: &[(String, String)]) -> Result<()> {
        if visits.is_empty() {
            return Ok(());
        }
        let transaction = self.conn.unchecked_transaction()?;
        let visited_at = now_epoch();
        {
            let mut statement = transaction
                .prepare("INSERT INTO history (url, title, visited_at) VALUES (?1, ?2, ?3)")?;
            for (url, title) in visits {
                statement.execute(params![url, title, visited_at])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Elimina le visite più vecchie di `days` giorni. `days <= 0` disattiva la
    /// potatura. Restituisce il numero di righe rimosse.
    pub fn prune_history(&self, days: i64) -> Result<usize> {
        if days <= 0 {
            return Ok(0);
        }
        let cutoff = now_epoch() - days * 24 * 60 * 60;
        let removed = self
            .conn
            .execute("DELETE FROM history WHERE visited_at < ?1", params![cutoff])?;
        Ok(removed)
    }

    /// Cronologia raggruppata per URL, con il numero di visite.
    pub fn history(&self, order: HistoryOrder, limit: u32) -> Result<Vec<HistoryEntry>> {
        let ordering = match order {
            HistoryOrder::Recent => "MAX(h.visited_at) DESC, MAX(h.id) DESC",
            HistoryOrder::MostVisited => "COUNT(*) DESC, MAX(h.visited_at) DESC",
        };
        let sql = format!(
            "SELECT MAX(h.id) AS id,
                    h.url,
                    COALESCE((
                        SELECT x.title FROM history x
                        WHERE x.url = h.url
                        ORDER BY x.visited_at DESC, x.id DESC
                        LIMIT 1
                    ), h.url) AS title,
                    MAX(h.visited_at) AS visited_at,
                    COUNT(*) AS visits
             FROM history h
             GROUP BY h.url
             ORDER BY {ordering}
             LIMIT ?1"
        );

        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params![limit], |row| {
            Ok(HistoryEntry {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                visited_at: row.get(3)?,
                visits: row.get(4)?,
            })
        })?;

        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn clear_history(&self) -> Result<()> {
        self.conn.execute("DELETE FROM history", [])?;
        Ok(())
    }

    pub fn is_bookmarked(&self, url: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM bookmarks WHERE url = ?1",
            params![url],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn add_bookmark(&self, url: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO bookmarks (url, title, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(url) DO UPDATE SET title = excluded.title",
            params![url, title, now_epoch()],
        )?;
        Ok(())
    }

    pub fn remove_bookmark_by_url(&self, url: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM bookmarks WHERE url = ?1", params![url])?;
        Ok(())
    }

    pub fn remove_bookmark(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM bookmarks WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn bookmarks(&self) -> Result<Vec<Bookmark>> {
        let mut statement = self.conn.prepare(
            "SELECT id, url, title, created_at
             FROM bookmarks
             ORDER BY created_at DESC, id DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Bookmark {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Cronologia filtrata per testo, per i suggerimenti della barra indirizzi.
    /// La ricerca è case-insensitive (`LIKE`, default di SQLite) su URL e
    /// titolo, raggruppata per URL come la pagina della cronologia.
    pub fn search_history(&self, term: &str, limit: u32) -> Result<Vec<HistoryEntry>> {
        // Prima l'indice FTS5 (parole intere e prefissi, molto più veloce su
        // tabelle grandi); se non trova nulla si ripiega sul `LIKE`, che copre
        // anche le sottostringhe a metà parola.
        if let Ok(rows) = self.search_history_fts(term, limit) {
            if !rows.is_empty() {
                return Ok(rows);
            }
        }

        let mut statement = self.conn.prepare(
            "SELECT MAX(h.id) AS id,
                    h.url,
                    COALESCE((
                        SELECT x.title FROM history x
                        WHERE x.url = h.url
                        ORDER BY x.visited_at DESC, x.id DESC
                        LIMIT 1
                    ), h.url) AS title,
                    MAX(h.visited_at) AS visited_at,
                    COUNT(*) AS visits
             FROM history h
             WHERE h.url LIKE ?1 ESCAPE '\\' OR h.title LIKE ?1 ESCAPE '\\'
             GROUP BY h.url
             ORDER BY MAX(h.visited_at) DESC
             LIMIT ?2",
        )?;
        let pattern = format!("%{}%", escape_like(term));
        let rows = statement.query_map(params![pattern, limit], |row| {
            Ok(HistoryEntry {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                visited_at: row.get(3)?,
                visits: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Segnalibri filtrati per testo, per i suggerimenti della barra indirizzi.
    pub fn search_bookmarks(&self, term: &str, limit: u32) -> Result<Vec<Bookmark>> {
        let mut statement = self.conn.prepare(
            "SELECT id, url, title, created_at
             FROM bookmarks
             WHERE url LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\'
             ORDER BY created_at DESC, id DESC
             LIMIT ?2",
        )?;
        let pattern = format!("%{}%", escape_like(term));
        let rows = statement.query_map(params![pattern, limit], |row| {
            Ok(Bookmark {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn add_download(
        &self,
        url: &str,
        filename: &str,
        destination: &str,
        status: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO downloads
                (url, filename, destination, status, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![url, filename, destination, status, now_epoch()],
        )?;
        Ok(())
    }

    /// Aggiorna lo stato del download identificato dal percorso di
    /// destinazione, che è univoco per ogni download avviato.
    pub fn set_download_status(&self, destination: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE downloads SET status = ?1
             WHERE destination = ?2 AND status = 'in_progress'",
            params![status, destination],
        )?;
        Ok(())
    }

    pub fn remove_download(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM downloads WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn downloads(&self) -> Result<Vec<DownloadEntry>> {
        let mut statement = self.conn.prepare(
            "SELECT id, url, filename, destination, status, started_at
             FROM downloads
             ORDER BY started_at DESC, id DESC
             LIMIT 200",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(DownloadEntry {
                id: row.get(0)?,
                url: row.get(1)?,
                filename: row.get(2)?,
                destination: row.get(3)?,
                status: row.get(4)?,
                started_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn site_zoom(&self, origin: &str) -> Result<Option<f64>> {
        let mut statement = self
            .conn
            .prepare("SELECT zoom FROM site_zoom WHERE origin = ?1")?;
        let mut rows = statement.query(params![origin])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(row.get(0)?));
        }
        Ok(None)
    }

    pub fn set_site_zoom(&self, origin: &str, zoom: f64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO site_zoom (origin, zoom) VALUES (?1, ?2)
             ON CONFLICT(origin) DO UPDATE SET zoom = excluded.zoom",
            params![origin, zoom],
        )?;
        Ok(())
    }

    pub fn clear_site_zoom(&self, origin: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM site_zoom WHERE origin = ?1", params![origin])?;
        Ok(())
    }

    /// Decisione memorizzata per un permesso, se l'utente ne ha già scelta una.
    pub fn permission(&self, origin: &str, kind: &str) -> Result<Option<String>> {
        let mut statement = self.conn.prepare(
            "SELECT decision FROM site_permissions WHERE origin = ?1 AND kind = ?2",
        )?;
        let mut rows = statement.query(params![origin, kind])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(row.get(0)?));
        }
        Ok(None)
    }

    pub fn set_permission(&self, origin: &str, kind: &str, decision: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO site_permissions (origin, kind, decision, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(origin, kind) DO UPDATE SET
                 decision = excluded.decision,
                 updated_at = excluded.updated_at",
            params![origin, kind, decision, now_epoch()],
        )?;
        Ok(())
    }

    pub fn permissions(&self) -> Result<Vec<SitePermission>> {
        let mut statement = self.conn.prepare(
            "SELECT origin, kind, decision FROM site_permissions ORDER BY origin, kind",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SitePermission {
                origin: row.get(0)?,
                kind: row.get(1)?,
                decision: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn remove_permission(&self, origin: &str, kind: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM site_permissions WHERE origin = ?1 AND kind = ?2",
            params![origin, kind],
        )?;
        Ok(())
    }

    pub fn clear_permissions(&self) -> Result<()> {
        self.conn.execute("DELETE FROM site_permissions", [])?;
        Ok(())
    }
}

pub fn load_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!("cannot parse {}: {error}", path.display());
            None
        }
    }
}

pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let data = serde_json::to_vec_pretty(value)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, data)
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Neutralizza i metacaratteri di `LIKE` (`%`, `_`, `\\`) così il testo
/// digitato nella barra indirizzi viene cercato letteralmente.
fn escape_like(term: &str) -> String {
    let mut escaped = String::with_capacity(term.len());
    for character in term.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}


impl Storage {
    /// Origini per cui l'utente ha disattivato il blocco contenuti.
    pub fn block_exceptions(&self) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT origin FROM block_exceptions ORDER BY origin")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut origins = Vec::new();
        for row in rows {
            origins.push(row?);
        }
        Ok(origins)
    }

    pub fn add_block_exception(&self, origin: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO block_exceptions (origin, created_at) VALUES (?1, ?2)",
            params![origin, now_epoch()],
        )?;
        Ok(())
    }

    pub fn remove_block_exception(&self, origin: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM block_exceptions WHERE origin = ?1",
            params![origin],
        )?;
        Ok(())
    }

    pub fn clear_block_exceptions(&self) -> Result<()> {
        self.conn.execute("DELETE FROM block_exceptions", [])?;
        Ok(())
    }

    // -------------------------------------------------- Importazione dei dati

    /// Importa visite da un altro browser conservando la data originale e
    /// saltando i duplicati esatti. Senza data si usa l'ora attuale.
    pub fn import_history(&self, visits: &[(String, String, Option<i64>)]) -> Result<usize> {
        if visits.is_empty() {
            return Ok(0);
        }
        let transaction = self.conn.unchecked_transaction()?;
        let mut imported = 0usize;
        {
            let mut duplicate = transaction
                .prepare("SELECT COUNT(*) FROM history WHERE url = ?1 AND visited_at = ?2")?;
            let mut insert = transaction.prepare(
                "INSERT INTO history (url, title, visited_at) VALUES (?1, ?2, ?3)",
            )?;
            for (url, title, visited_at) in visits {
                let visited_at = visited_at.unwrap_or_else(now_epoch);
                let count: i64 =
                    duplicate.query_row(params![url, visited_at], |row| row.get(0))?;
                if count > 0 {
                    continue;
                }
                insert.execute(params![url, title, visited_at])?;
                imported += 1;
            }
        }
        transaction.commit()?;
        Ok(imported)
    }

    /// Importa segnalibri in blocco: lo stesso URL aggiorna il titolo invece di
    /// creare un doppione.
    pub fn add_bookmarks_bulk(&self, items: &[(String, String)]) -> Result<usize> {
        if items.is_empty() {
            return Ok(0);
        }
        let transaction = self.conn.unchecked_transaction()?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO bookmarks (url, title, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(url) DO UPDATE SET title = excluded.title",
            )?;
            for (url, title) in items {
                statement.execute(params![url, title, now_epoch()])?;
            }
        }
        transaction.commit()?;
        Ok(items.len())
    }

    /// Account salvati, con il segreto ancora cifrato.
    pub fn credentials(&self) -> Result<Vec<StoredCredential>> {
        let mut statement = self.conn.prepare(
            "SELECT id, host, url, username, nonce, secret
             FROM credentials
             ORDER BY host COLLATE NOCASE, username COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(StoredCredential {
                id: row.get(0)?,
                host: row.get(1)?,
                url: row.get(2)?,
                username: row.get(3)?,
                nonce: row.get(4)?,
                secret: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Salva o aggiorna account e password (`nonce` e `secret` arrivano già
    /// cifrati). L'identità di una voce è la coppia host + nome utente.
    pub fn save_credentials(
        &self,
        rows: &[(String, String, String, Vec<u8>, Vec<u8>)],
    ) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let transaction = self.conn.unchecked_transaction()?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO credentials (host, url, username, nonce, secret, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(host, username) DO UPDATE SET
                     url = excluded.url,
                     nonce = excluded.nonce,
                     secret = excluded.secret,
                     updated_at = excluded.updated_at",
            )?;
            for (host, url, username, nonce, secret) in rows {
                statement.execute(params![host, url, username, nonce, secret, now_epoch()])?;
            }
        }
        transaction.commit()?;
        Ok(rows.len())
    }

    pub fn remove_credential(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM credentials WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn clear_credentials(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM credentials", [])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_storage(name: &str) -> Storage {
        let path = std::env::temp_dir().join(format!(
            "peats-test-{name}-{}-{:?}.sqlite3",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_file(&path);
        Storage::open(&path).expect("cannot open the test database")
    }

    #[test]
    fn downloads_are_recorded_and_updated() {
        let storage = temp_storage("downloads");
        storage
            .add_download(
                "https://example.com/a.zip",
                "a.zip",
                "/tmp/a.zip",
                "in_progress",
            )
            .unwrap();

        let entries = storage.downloads().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].filename, "a.zip");
        assert_eq!(entries[0].destination, "/tmp/a.zip");
        assert_eq!(entries[0].status, "in_progress");

        storage
            .set_download_status("/tmp/a.zip", "completed")
            .unwrap();
        let entries = storage.downloads().unwrap();
        assert_eq!(entries[0].status, "completed");
        assert_eq!(entries[0].status_label(), "Completato");
    }

    #[test]
    fn history_is_grouped_with_visit_counts() {
        let storage = temp_storage("history");
        for _ in 0..3 {
            storage
                .add_history("https://example.com", "Esempio")
                .unwrap();
        }
        storage
            .add_history("https://rust-lang.org", "Rust")
            .unwrap();

        let most_visited = storage.history(HistoryOrder::MostVisited, 10).unwrap();
        assert_eq!(most_visited.len(), 2);
        assert_eq!(most_visited[0].url, "https://example.com");
        assert_eq!(most_visited[0].visits, 3);
        assert_eq!(most_visited[0].title, "Esempio");

        let recent = storage.history(HistoryOrder::Recent, 10).unwrap();
        assert_eq!(recent[0].url, "https://rust-lang.org");
    }

    #[test]
    fn history_search_uses_the_fts_index_and_falls_back_to_like() {
        let storage = temp_storage("fts");
        storage
            .add_history("https://doc.example/rust", "Documentazione Rust")
            .unwrap();
        storage
            .add_history("https://blog.example/rustico", "Rustico")
            .unwrap();
        storage
            .add_history("https://news.example/altro", "Altre notizie")
            .unwrap();

        // Prefisso: «rus» trova entrambe le voci con l'indice.
        let prefix = storage.search_history("rus", 5).unwrap();
        assert_eq!(prefix.len(), 2);

        // Sottostringa a metà parola: l'indice non la trova, il LIKE sì.
        let middle = storage.search_history("mentaz", 5).unwrap();
        assert_eq!(middle.len(), 1);
        assert_eq!(middle[0].url, "https://doc.example/rust");

        // Una query con virgolette non deve rompere la ricerca.
        assert!(storage.search_history("\"rust", 5).is_ok());
    }

    #[test]
    fn history_is_written_in_batches_and_pruned() {
        let storage = temp_storage("prune");
        storage
            .add_history_many(&[
                ("https://a.example".to_owned(), "A".to_owned()),
                ("https://b.example".to_owned(), "B".to_owned()),
            ])
            .unwrap();
        assert_eq!(storage.history(HistoryOrder::Recent, 10).unwrap().len(), 2);

        // Una visita molto vecchia viene rimossa dalla potatura.
        storage
            .conn
            .execute(
                "INSERT INTO history (url, title, visited_at) VALUES (?1, ?2, ?3)",
                params!["https://old.example", "Vecchia", now_epoch() - 400 * 24 * 60 * 60],
            )
            .unwrap();
        let removed = storage.prune_history(365).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(storage.history(HistoryOrder::Recent, 10).unwrap().len(), 2);
        assert_eq!(storage.prune_history(0).unwrap(), 0);
    }

    #[test]
    fn imported_history_keeps_dates_and_skips_duplicates() {
        let storage = temp_storage("import-history");
        let visits = vec![
            ("https://a.example".to_owned(), "A".to_owned(), Some(1_600_000_000)),
            ("https://a.example".to_owned(), "A".to_owned(), Some(1_600_000_000)),
            ("https://b.example".to_owned(), "B".to_owned(), None),
        ];
        assert_eq!(storage.import_history(&visits).unwrap(), 2);
        assert_eq!(storage.history(HistoryOrder::Recent, 10).unwrap().len(), 2);
        // L'indice FTS vede anche le righe importate.
        assert!(!storage.search_history("example", 5).unwrap().is_empty());
    }

    #[test]
    fn bulk_bookmarks_are_imported_without_duplicates() {
        let storage = temp_storage("import-bookmarks");
        let items = vec![
            ("https://a.example".to_owned(), "A".to_owned()),
            ("https://a.example".to_owned(), "A aggiornato".to_owned()),
        ];
        assert_eq!(storage.add_bookmarks_bulk(&items).unwrap(), 2);
        let bookmarks = storage.bookmarks().unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].title, "A aggiornato");
    }

    #[test]
    fn credentials_are_upserted_and_removed() {
        let storage = temp_storage("credentials");
        let rows = vec![
            (
                "a.example".to_owned(),
                "https://a.example".to_owned(),
                "mario".to_owned(),
                vec![1u8; 12],
                vec![9u8, 9, 9],
            ),
            (
                "b.example".to_owned(),
                "https://b.example".to_owned(),
                String::new(),
                vec![2u8; 12],
                vec![8u8],
            ),
        ];
        assert_eq!(storage.save_credentials(&rows).unwrap(), 2);
        assert_eq!(storage.credentials().unwrap().len(), 2);

        // Stessa coppia host + nome utente: aggiorna, non duplica.
        let again = vec![(
            "a.example".to_owned(),
            "https://a.example".to_owned(),
            "mario".to_owned(),
            vec![3u8; 12],
            vec![7u8, 7],
        )];
        storage.save_credentials(&again).unwrap();
        let stored = storage.credentials().unwrap();
        assert_eq!(stored.len(), 2);
        let mario = stored
            .iter()
            .find(|entry| entry.username == "mario")
            .unwrap();
        assert_eq!(mario.secret, vec![7u8, 7]);
        assert_eq!(mario.nonce, vec![3u8; 12]);

        storage.remove_credential(mario.id).unwrap();
        assert_eq!(storage.credentials().unwrap().len(), 1);
        assert_eq!(storage.clear_credentials().unwrap(), 1);
        assert!(storage.credentials().unwrap().is_empty());
    }

    #[test]
    fn search_matches_history_and_bookmarks_by_text() {
        let storage = temp_storage("search");
        storage
            .add_history("https://doc.example/rust", "Documentazione Rust")
            .unwrap();
        storage
            .add_history("https://news.example/other", "Notizie")
            .unwrap();
        storage
            .add_bookmark("https://blog.example/gtk", "GTK e Rust")
            .unwrap();

        let history = storage.search_history("rust", 5).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].url, "https://doc.example/rust");

        // La ricerca funziona anche sul solo URL.
        let by_url = storage.search_history("news.example", 5).unwrap();
        assert_eq!(by_url.len(), 1);

        let bookmarks = storage.search_bookmarks("gtk", 5).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].title, "GTK e Rust");

        // I metacaratteri di LIKE vengono cercati letteralmente.
        assert!(storage.search_bookmarks("%", 5).unwrap().is_empty());
        assert!(storage.search_history("_", 5).unwrap().is_empty());
    }

    #[test]
    fn permissions_are_stored_and_cleared() {
        let storage = temp_storage("permissions");
        assert_eq!(
            storage.permission("https://example.com", "geolocation").unwrap(),
            None
        );

        storage
            .set_permission("https://example.com", "geolocation", "allow")
            .unwrap();
        storage
            .set_permission("https://example.com", "geolocation", "deny")
            .unwrap();
        assert_eq!(
            storage
                .permission("https://example.com", "geolocation")
                .unwrap()
                .as_deref(),
            Some("deny")
        );

        let all = storage.permissions().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].origin, "https://example.com");
        assert_eq!(all[0].kind, "geolocation");

        storage.clear_permissions().unwrap();
        assert!(storage.permissions().unwrap().is_empty());
    }

    #[test]
    fn block_exceptions_are_stored_and_removed() {
        let storage = temp_storage("block-exceptions");
        assert!(storage.block_exceptions().unwrap().is_empty());

        storage
            .add_block_exception("https://example.com")
            .unwrap();
        storage
            .add_block_exception("http://127.0.0.1:8765")
            .unwrap();
        // Una seconda aggiunta della stessa origine non duplica la riga.
        storage
            .add_block_exception("https://example.com")
            .unwrap();
        assert_eq!(
            storage.block_exceptions().unwrap(),
            vec![
                "http://127.0.0.1:8765".to_owned(),
                "https://example.com".to_owned()
            ]
        );

        storage
            .remove_block_exception("https://example.com")
            .unwrap();
        assert_eq!(
            storage.block_exceptions().unwrap(),
            vec!["http://127.0.0.1:8765".to_owned()]
        );

        storage.clear_block_exceptions().unwrap();
        assert!(storage.block_exceptions().unwrap().is_empty());
    }

    #[test]
    fn clearing_history_keeps_site_permissions() {
        let storage = temp_storage("history-permissions");
        storage
            .add_history("https://example.com", "Esempio")
            .unwrap();
        storage
            .set_permission("https://example.com", "geolocation", "allow")
            .unwrap();

        storage.clear_history().unwrap();

        assert!(storage.history(HistoryOrder::Recent, 10).unwrap().is_empty());
        assert_eq!(storage.permissions().unwrap().len(), 1);
    }

    #[test]
    fn site_zoom_roundtrip() {
        let storage = temp_storage("zoom");
        assert_eq!(storage.site_zoom("https://example.com").unwrap(), None);
        storage.set_site_zoom("https://example.com", 1.25).unwrap();
        assert_eq!(
            storage.site_zoom("https://example.com").unwrap(),
            Some(1.25)
        );
        storage.clear_site_zoom("https://example.com").unwrap();
        assert_eq!(storage.site_zoom("https://example.com").unwrap(), None);
    }
}
