//! SQLite storage: file-name snapshot, full-text index, pins, open counts and organize history.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct Db {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pin {
    pub alias: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveRecord {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryItem {
    pub id: i64,
    pub created: i64,
    pub root: String,
    pub instruction: String,
    pub moves: Vec<MoveRecord>,
    pub undone: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentHit {
    pub path: String,
    pub snippet: String,
}

pub struct SnapshotRow {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Self { path: path.to_path_buf() };
        db.migrate()?;
        Ok(db)
    }

    pub fn conn(&self) -> Result<Connection> {
        let c = Connection::open(&self.path)?;
        c.busy_timeout(std::time::Duration::from_secs(10))?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        Ok(c)
    }

    /// Schema version stored in `PRAGMA user_version`. Bump it and add a step below
    /// whenever the tables change, so existing users' data is upgraded in place.
    pub const SCHEMA_VERSION: i64 = 3;

    fn migrate(&self) -> Result<()> {
        let c = self.conn()?;
        let current: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current > Self::SCHEMA_VERSION {
            log::warn!("database schema v{current} is newer than this app (v{}); continuing read-compatible", Self::SCHEMA_VERSION);
            return Ok(());
        }
        // v0 → v1: the original tables. `IF NOT EXISTS` also adopts databases made by v0.1.0, which had no version.
        if current < 1 {
            Self::create_v1(&c)?;
        }
        // v1 → v2: the file-name snapshot is stored WITHOUT ROWID, so each path is kept once instead of
        // twice (table + primary-key index). It is only a startup cache, so it is rebuilt by the next scan.
        if current < 2 {
            c.execute_batch(
                "DROP TABLE IF EXISTS files;
                 CREATE TABLE files(path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL, is_dir INTEGER NOT NULL) WITHOUT ROWID;",
            )?;
            // Let the file shrink when the full-text index drops documents (takes effect after one VACUUM).
            c.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")?;
        }
        // v2 → v3: cache of kanji readings from the Windows IME ("" = could not be read).
        if current < 3 {
            c.execute_batch("CREATE TABLE IF NOT EXISTS readings(name TEXT PRIMARY KEY, reading TEXT NOT NULL) WITHOUT ROWID;")?;
        }
        // Future steps go here: `if current < 4 { ... }`.
        c.execute_batch(&format!("PRAGMA user_version = {}", Self::SCHEMA_VERSION))?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn()?.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    fn create_v1(c: &Connection) -> Result<()> {
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL, is_dir INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS content_meta(path TEXT PRIMARY KEY, mtime INTEGER NOT NULL, size INTEGER NOT NULL);
             CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(path UNINDEXED, body, tokenize='trigram');
             CREATE TABLE IF NOT EXISTS pins(alias TEXT PRIMARY KEY, path TEXT NOT NULL, created INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS opens(path TEXT PRIMARY KEY, count INTEGER NOT NULL, last INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS history(id INTEGER PRIMARY KEY AUTOINCREMENT, created INTEGER NOT NULL, root TEXT NOT NULL,
                 instruction TEXT NOT NULL, moves TEXT NOT NULL, undone INTEGER NOT NULL DEFAULT 0);",
        )?;
        Ok(())
    }

    // ---- file-name snapshot ----

    pub fn load_snapshot(&self) -> Result<Vec<SnapshotRow>> {
        let c = self.conn()?;
        let mut st = c.prepare("SELECT path, size, mtime, is_dir FROM files")?;
        let rows = st
            .query_map([], |r| {
                Ok(SnapshotRow { path: r.get(0)?, size: r.get::<_, i64>(1)? as u64, mtime: r.get(2)?, is_dir: r.get::<_, i64>(3)? != 0 })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    pub fn save_snapshot(&self, rows: impl Iterator<Item = (String, u64, i64, bool)>) -> Result<()> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        tx.execute("DELETE FROM files", [])?;
        {
            let mut st = tx.prepare("INSERT OR REPLACE INTO files(path, size, mtime, is_dir) VALUES(?1, ?2, ?3, ?4)")?;
            for (p, s, m, d) in rows {
                st.execute(params![p, s as i64, m, d as i64])?;
                // Each path string is dropped right after insertion, so a 1M-entry save needs no extra copy.
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- full-text ----

    pub fn content_mtimes(&self) -> Result<HashMap<String, i64>> {
        let c = self.conn()?;
        let mut st = c.prepare("SELECT path, mtime FROM content_meta")?;
        let map = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|r| r.ok()).collect();
        Ok(map)
    }

    pub fn put_content(&self, c: &Connection, path: &str, mtime: i64, size: u64, body: &str) -> Result<()> {
        c.execute("DELETE FROM content_fts WHERE path = ?1", [path])?;
        if !body.is_empty() {
            c.execute("INSERT INTO content_fts(path, body) VALUES(?1, ?2)", params![path, body])?;
        }
        c.execute(
            "INSERT OR REPLACE INTO content_meta(path, mtime, size) VALUES(?1, ?2, ?3)",
            params![path, mtime, size as i64],
        )?;
        Ok(())
    }

    pub fn remove_content(&self, c: &Connection, path: &str) -> Result<()> {
        c.execute("DELETE FROM content_fts WHERE path = ?1", [path])?;
        c.execute("DELETE FROM content_meta WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Returns free pages to the OS after large deletions.
    pub fn shrink(&self) -> Result<()> {
        self.conn()?.execute_batch("PRAGMA incremental_vacuum;")?;
        Ok(())
    }

    pub fn content_count(&self) -> Result<i64> {
        Ok(self.conn()?.query_row("SELECT COUNT(*) FROM content_meta", [], |r| r.get(0))?)
    }

    /// Full-text search. Every term must appear in the body.
    /// Terms of 3+ chars use the trigram index; shorter ones fall back to LIKE.
    pub fn search_content(&self, terms: &[String], limit: usize) -> Result<Vec<ContentHit>> {
        let terms: Vec<&String> = terms.iter().filter(|t| !t.trim().is_empty()).collect();
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let c = self.conn()?;
        let (long, short): (Vec<&String>, Vec<&String>) = terms.iter().partition(|t| t.chars().count() >= 3);
        let mut sql = String::from("SELECT path, ");
        let mut args: Vec<String> = Vec::new();
        if long.is_empty() {
            sql.push_str("body FROM content_fts WHERE 1=1");
        } else {
            sql.push_str("snippet(content_fts, 1, '\u{1}', '\u{2}', '…', 24) FROM content_fts WHERE content_fts MATCH ?");
            let m = long.iter().map(|t| format!("\"{}\"", t.replace('"', "\"\""))).collect::<Vec<_>>().join(" AND ");
            args.push(m);
        }
        for t in &short {
            sql.push_str(" AND body LIKE ? ESCAPE '\\'");
            let esc = t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            args.push(format!("%{esc}%"));
        }
        sql.push_str(&format!(" LIMIT {limit}"));
        let mut st = c.prepare(&sql)?;
        let hits = st
            .query_map(rusqlite::params_from_iter(args.iter()), |r| Ok(ContentHit { path: r.get(0)?, snippet: r.get(1)? }))?
            .filter_map(|r| r.ok())
            .map(|mut h| {
                if long.is_empty() {
                    h.snippet = snippet_around(&h.snippet, short[0]);
                }
                h
            })
            .collect();
        Ok(hits)
    }

    /// Body text for a short-term hit, so we can show context around the first match.
    pub fn content_body(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .conn()?
            .query_row("SELECT body FROM content_fts WHERE path = ?1", [path], |r| r.get(0))
            .optional()?)
    }

    // ---- kanji readings cache ----

    pub fn readings(&self) -> Result<HashMap<String, String>> {
        let c = self.conn()?;
        let mut st = c.prepare("SELECT name, reading FROM readings")?;
        let m = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|r| r.ok()).collect();
        Ok(m)
    }

    pub fn put_readings(&self, rows: &[(String, String)]) -> Result<()> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        {
            let mut st = tx.prepare("INSERT OR REPLACE INTO readings(name, reading) VALUES(?1, ?2)")?;
            for (n, r) in rows {
                st.execute(params![n, r])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- pins ----

    pub fn pins(&self) -> Result<Vec<Pin>> {
        let c = self.conn()?;
        let mut st = c.prepare("SELECT alias, path FROM pins ORDER BY created")?;
        let v = st.query_map([], |r| Ok(Pin { alias: r.get(0)?, path: r.get(1)? }))?.filter_map(|r| r.ok()).collect();
        Ok(v)
    }

    pub fn add_pin(&self, alias: &str, path: &str) -> Result<()> {
        self.conn()?.execute(
            "INSERT OR REPLACE INTO pins(alias, path, created) VALUES(?1, ?2, ?3)",
            params![alias, path, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    }

    pub fn remove_pin(&self, alias: &str) -> Result<()> {
        self.conn()?.execute("DELETE FROM pins WHERE alias = ?1", [alias])?;
        Ok(())
    }

    // ---- open counts (ranking boost) ----

    pub fn record_open(&self, path: &str) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO opens(path, count, last) VALUES(?1, 1, ?2)
             ON CONFLICT(path) DO UPDATE SET count = count + 1, last = excluded.last",
            params![path, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    }

    pub fn open_counts(&self) -> Result<HashMap<String, u32>> {
        let c = self.conn()?;
        let mut st = c.prepare("SELECT path, count FROM opens")?;
        let m = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|r| r.ok()).collect();
        Ok(m)
    }

    // ---- organize history ----

    pub fn add_history(&self, root: &str, instruction: &str, moves: &[MoveRecord]) -> Result<i64> {
        let c = self.conn()?;
        c.execute(
            "INSERT INTO history(created, root, instruction, moves) VALUES(?1, ?2, ?3, ?4)",
            params![chrono::Utc::now().timestamp(), root, instruction, serde_json::to_string(moves)?],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn history(&self, limit: usize) -> Result<Vec<HistoryItem>> {
        let c = self.conn()?;
        let mut st = c.prepare("SELECT id, created, root, instruction, moves, undone FROM history ORDER BY id DESC LIMIT ?1")?;
        let v = st
            .query_map([limit as i64], |r| {
                let moves: String = r.get(4)?;
                Ok(HistoryItem {
                    id: r.get(0)?,
                    created: r.get(1)?,
                    root: r.get(2)?,
                    instruction: r.get(3)?,
                    moves: serde_json::from_str(&moves).unwrap_or_default(),
                    undone: r.get::<_, i64>(5)? != 0,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(v)
    }

    pub fn history_item(&self, id: i64) -> Result<Option<HistoryItem>> {
        Ok(self.history(10_000)?.into_iter().find(|h| h.id == id))
    }

    pub fn mark_undone(&self, id: i64) -> Result<()> {
        self.conn()?.execute("UPDATE history SET undone = 1 WHERE id = ?1", [id])?;
        Ok(())
    }
}

/// ~80 chars of context around the first case-insensitive occurrence of `term`,
/// with the match wrapped in \u{1}..\u{2} like FTS5's snippet().
pub fn snippet_around(body: &str, term: &str) -> String {
    let lower = body.to_lowercase();
    let t = term.to_lowercase();
    let Some(pos) = lower.find(&t) else {
        return body.chars().take(80).collect();
    };
    // Lowercasing can change byte lengths; map back via char counts.
    let char_pos = lower[..pos].chars().count();
    let tlen = t.chars().count();
    let chars: Vec<char> = body.chars().collect();
    let start = char_pos.saturating_sub(30);
    let end = (char_pos + tlen + 50).min(chars.len());
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.extend(&chars[start..char_pos.min(chars.len())]);
    s.push('\u{1}');
    s.extend(&chars[char_pos.min(chars.len())..(char_pos + tlen).min(chars.len())]);
    s.push('\u{2}');
    s.extend(&chars[(char_pos + tlen).min(chars.len())..end]);
    if end < chars.len() {
        s.push('…');
    }
    s.replace(['\n', '\r', '\t'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> (tempfile::TempDir, Db) {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("t.db")).unwrap();
        (d, db)
    }

    #[test]
    fn fts_long_and_short_terms() {
        let (_d, db) = tmp_db();
        let c = db.conn().unwrap();
        db.put_content(&c, "c:/a.docx", 1, 10, "A社 保守費用の見積書です。合計 120,000 円").unwrap();
        db.put_content(&c, "c:/b.txt", 1, 10, "議事録 来週の予定").unwrap();
        let h = db.search_content(&["保守費用".into()], 10).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].path, "c:/a.docx");
        assert!(h[0].snippet.contains('\u{1}'));
        let h = db.search_content(&["見積".into()], 10).unwrap();
        assert_eq!(h.len(), 1);
        let h = db.search_content(&["議事録".into(), "予定".into()], 10).unwrap();
        assert_eq!(h.len(), 1);
        db.remove_content(&c, "c:/b.txt").unwrap();
        assert!(db.search_content(&["議事録".into()], 10).unwrap().is_empty());
    }

    #[test]
    fn pins_and_history() {
        let (_d, db) = tmp_db();
        db.add_pin("経理", "C:/Users/x/経理").unwrap();
        assert_eq!(db.pins().unwrap().len(), 1);
        let id = db.add_history("C:/x", "種類別", &[MoveRecord { from: "a".into(), to: "b".into() }]).unwrap();
        db.mark_undone(id).unwrap();
        let h = db.history_item(id).unwrap().unwrap();
        assert!(h.undone);
        assert_eq!(h.moves.len(), 1);
    }

    #[test]
    fn schema_version_and_legacy_adoption() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("old.db");
        // A v0.1.0 database: tables exist, user_version is 0, and it holds a pin.
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE pins(alias TEXT PRIMARY KEY, path TEXT NOT NULL, created INTEGER NOT NULL);
                             INSERT INTO pins VALUES('経理', 'C:/x', 1);").unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), Db::SCHEMA_VERSION);
        assert_eq!(db.pins().unwrap().len(), 1, "existing data survives the upgrade");
        // Opening again is a no-op.
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), Db::SCHEMA_VERSION);
    }

    #[test]
    fn snippet() {
        let s = snippet_around("あいうえお見積書かきくけこ", "見積");
        assert!(s.contains("\u{1}見積\u{2}"));
    }
}
