//! SQLite 持久化。所有写操作都在后台线程执行，不阻塞 UI 消息循环。
//! 图片用 LZ4 压缩后存储。

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use rusqlite::{Connection, OptionalExtension, params};

use crate::clip::ClipData;
use crate::history::{Entry, Kind};

const SCHEMA: &str = "
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS items (
    key       INTEGER PRIMARY KEY,
    kind      INTEGER NOT NULL,
    preview   TEXT    NOT NULL,
    search    TEXT    NOT NULL,
    last_used INTEGER NOT NULL,
    size      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS items_last_used ON items(last_used DESC);
CREATE TABLE IF NOT EXISTS payloads (
    key   INTEGER PRIMARY KEY REFERENCES items(key) ON DELETE CASCADE,
    text  TEXT,
    html  BLOB,
    rtf   BLOB,
    image BLOB,
    files TEXT
);
";

pub struct Record {
    pub entry: Entry,
    pub data: ClipData,
}

enum Cmd {
    Upsert(Box<Record>),
    Touch(i64, i64),
    Delete(i64),
    Clear,
    SetMax(usize),
    Load(i64, Sender<Option<ClipData>>),
}

pub struct Store {
    tx: Option<Sender<Cmd>>,
    worker: Option<JoinHandle<()>>,
}

impl Store {
    /// 打开数据库，返回 Store 和最近的历史（按使用时间倒序）。
    pub fn open(path: &Path, max_items: usize) -> rusqlite::Result<(Store, Vec<Entry>)> {
        let conn = Connection::open(path)?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.execute_batch(SCHEMA)?;
        let entries = load_recent(&conn, max_items)?;
        let (tx, rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("clipstack-store".into())
            .spawn(move || run(conn, rx, max_items))
            .expect("failed to spawn store thread");
        Ok((Store { tx: Some(tx), worker: Some(worker) }, entries))
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    pub fn upsert(&self, record: Record) {
        self.send(Cmd::Upsert(Box::new(record)));
    }

    pub fn touch(&self, key: i64, now: i64) {
        self.send(Cmd::Touch(key, now));
    }

    pub fn delete(&self, key: i64) {
        self.send(Cmd::Delete(key));
    }

    pub fn clear(&self) {
        self.send(Cmd::Clear);
    }

    pub fn set_max(&self, max: usize) {
        self.send(Cmd::SetMax(max));
    }

    /// 读取完整内容。命令按顺序处理，所以刚 upsert 的条目一定能读到。
    pub fn load(&self, key: i64) -> Option<ClipData> {
        let (tx, rx) = mpsc::channel();
        self.send(Cmd::Load(key, tx));
        rx.recv().ok().flatten()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // 关闭通道让后台线程处理完剩余命令后退出
        self.tx.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn load_recent(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<Entry>> {
    let mut stmt =
        conn.prepare("SELECT key, kind, preview, search, last_used FROM items ORDER BY last_used DESC LIMIT ?1")?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok(Entry {
            key: r.get(0)?,
            kind: Kind::from_i64(r.get(1)?),
            preview: r.get(2)?,
            search: r.get(3)?,
            last_used: r.get(4)?,
        })
    })?;
    rows.collect()
}

fn run(mut conn: Connection, rx: Receiver<Cmd>, mut max: usize) {
    for cmd in rx {
        if let Err(e) = handle(&mut conn, cmd, &mut max) {
            crate::log::write(format!("store error: {e}"));
        }
    }
}

fn handle(conn: &mut Connection, cmd: Cmd, max: &mut usize) -> rusqlite::Result<()> {
    match cmd {
        Cmd::Upsert(r) => upsert(conn, &r, *max),
        Cmd::Touch(key, now) => {
            conn.execute("UPDATE items SET last_used = ?2 WHERE key = ?1", params![key, now])?;
            Ok(())
        }
        Cmd::Delete(key) => {
            conn.execute("DELETE FROM items WHERE key = ?1", [key])?;
            Ok(())
        }
        Cmd::Clear => {
            conn.execute_batch("DELETE FROM payloads; DELETE FROM items; PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok(())
        }
        Cmd::SetMax(m) => {
            *max = m;
            trim(conn, m)?;
            Ok(())
        }
        Cmd::Load(key, reply) => {
            let data = load(conn, key)?;
            let _ = reply.send(data);
            Ok(())
        }
    }
}

fn trim(conn: &Connection, max: usize) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM items WHERE key IN (SELECT key FROM items ORDER BY last_used DESC LIMIT -1 OFFSET ?1)",
        [max as i64],
    )
}

fn upsert(conn: &mut Connection, r: &Record, max: usize) -> rusqlite::Result<()> {
    let e = &r.entry;
    let (text, html, rtf, image, files) = match &r.data {
        ClipData::Text { text, html, rtf } => (Some(text.as_str()), html.as_deref(), rtf.as_deref(), None, None),
        ClipData::Image { dib } => (None, None, None, Some(lz4_flex::compress_prepend_size(dib)), None),
        ClipData::Files { paths } => (None, None, None, None, Some(paths.join("\n"))),
    };
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO items (key, kind, preview, search, last_used, size) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(key) DO UPDATE SET preview = excluded.preview, search = excluded.search,
             last_used = excluded.last_used, size = excluded.size",
        params![e.key, e.kind.to_i64(), e.preview, e.search, e.last_used, r.data.size() as i64],
    )?;
    tx.execute(
        "INSERT INTO payloads (key, text, html, rtf, image, files) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(key) DO UPDATE SET text = excluded.text, html = excluded.html, rtf = excluded.rtf,
             image = excluded.image, files = excluded.files",
        params![e.key, text, html, rtf, image, files],
    )?;
    trim(&tx, max)?;
    tx.commit()
}

fn load(conn: &Connection, key: i64) -> rusqlite::Result<Option<ClipData>> {
    conn.query_row(
        "SELECT i.kind, p.text, p.html, p.rtf, p.image, p.files
         FROM items i JOIN payloads p ON p.key = i.key WHERE i.key = ?1",
        [key],
        |r| {
            Ok(match Kind::from_i64(r.get(0)?) {
                Kind::Text => Some(ClipData::Text {
                    text: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    html: r.get(2)?,
                    rtf: r.get(3)?,
                }),
                Kind::Image => r
                    .get::<_, Option<Vec<u8>>>(4)?
                    .and_then(|b| lz4_flex::decompress_size_prepended(&b).ok())
                    .map(|dib| ClipData::Image { dib }),
                Kind::Files => {
                    let files: String = r.get::<_, Option<String>>(5)?.unwrap_or_default();
                    Some(ClipData::Files { paths: files.split('\n').map(str::to_string).collect() })
                }
            })
        },
    )
    .optional()
    .map(Option::flatten)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(data: ClipData, last_used: i64) -> Record {
        let entry = Entry {
            key: data.key(),
            kind: data.kind(),
            preview: data.preview(),
            search: data.search_text(),
            last_used,
        };
        Record { entry, data }
    }

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("clipstack-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("history.db")
    }

    #[test]
    fn roundtrip_dedup_and_trim() {
        let path = temp_db("roundtrip");
        let text = ClipData::Text { text: "hello".into(), html: Some(b"<b>hello</b>".to_vec()), rtf: None };
        let image = ClipData::Image { dib: (0..200u8).collect() };
        let files = ClipData::Files { paths: vec![r"C:\a.txt".into(), r"D:\b c.txt".into()] };
        {
            let (store, entries) = Store::open(&path, 2).unwrap();
            assert!(entries.is_empty());
            store.upsert(record(text.clone(), 1));
            store.upsert(record(image.clone(), 2));
            store.upsert(record(text.clone(), 3)); // 重复：只更新时间
            assert_eq!(store.load(text.key()), Some(text.clone()));
            assert_eq!(store.load(image.key()), Some(image.clone()));
            store.upsert(record(files.clone(), 4)); // 超过上限 2，最旧的 image 被删掉
            assert_eq!(store.load(files.key()), Some(files.clone()));
            assert_eq!(store.load(image.key()), None);
        }
        let (_store, entries) = Store::open(&path, 10).unwrap();
        let keys: Vec<i64> = entries.iter().map(|e| e.key).collect();
        assert_eq!(keys, [files.key(), text.key()]);
    }
}
