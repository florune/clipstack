//! SQLite 持久化与全文搜索。所有数据库操作都在后台线程执行，不阻塞 UI 消息循环。
//!
//! - 图片用 LZ4 压缩后存储
//! - 每条记录有一个 seq，每次被复制 / 使用都会分配新的、更大的 seq，所以 seq 倒序就是最近使用顺序
//! - 全文索引用 FTS5 的 trigram 分词（支持中文和任意子串，不区分大小写），索引的 rowid 就是 seq。
//!   搜索时按 rowid 倒序取前 N 条即可停止，不用把所有命中结果取出来排序
//! - trigram 至少要 3 个字符；更短的关键词改为在最近 SHORT_SCAN_RECENT 条里逐条匹配

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::clip::{ClipData, SEARCH_MAX_CHARS};
use crate::history::{Entry, Kind, RECENT_CACHE};

/// 关键词全都少于 3 个字符时，只在最近这么多条里找。
const SHORT_SCAN_RECENT: usize = 5000;

const SCHEMA: &str = "
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA journal_size_limit = 16777216;
CREATE TABLE IF NOT EXISTS items (
    key       INTEGER PRIMARY KEY,
    seq       INTEGER NOT NULL,
    kind      INTEGER NOT NULL,
    preview   TEXT    NOT NULL,
    last_used INTEGER NOT NULL,
    size      INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS payloads (
    key   INTEGER PRIMARY KEY REFERENCES items(key) ON DELETE CASCADE,
    text  TEXT,
    html  BLOB,
    rtf   BLOB,
    image BLOB,
    files TEXT
);
";

const SCHEMA_V1: &str = "
DROP INDEX IF EXISTS items_last_used;
CREATE UNIQUE INDEX IF NOT EXISTS items_seq ON items(seq);
CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(body, tokenize = 'trigram case_sensitive 0');
CREATE TRIGGER IF NOT EXISTS items_fts_delete AFTER DELETE ON items BEGIN
    DELETE FROM items_fts WHERE rowid = old.seq;
END;
";

pub struct Record {
    pub entry: Entry,
    pub data: ClipData,
}

/// 搜索结果：(key, 预览)，按最近使用顺序。
pub type Hit = (i64, String);

enum Cmd {
    Upsert(Box<Record>),
    Touch(i64, i64),
    Delete(i64),
    Clear,
    SetMax(usize),
    Load(i64, Sender<Option<ClipData>>),
    Recent(usize, Sender<Vec<Entry>>),
    Search(String, usize, Sender<Vec<Hit>>),
}

pub struct Store {
    tx: Option<Sender<Cmd>>,
    worker: Option<JoinHandle<()>>,
}

impl Store {
    /// 打开数据库，返回 Store 和最近的历史（最多 RECENT_CACHE 条，最近的在前）。
    pub fn open(path: &Path, max_items: usize) -> rusqlite::Result<(Store, Vec<Entry>)> {
        let mut conn = Connection::open(path)?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.execute_batch(SCHEMA)?;
        migrate(&mut conn)?;
        let entries = recent(&conn, max_items.min(RECENT_CACHE))?;
        let mut worker = Worker::new(conn, max_items)?;
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("clipstack-store".into())
            .spawn(move || worker.run(rx))
            .expect("failed to spawn store thread");
        Ok((Store { tx: Some(tx), worker: Some(handle) }, entries))
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

    /// 最近使用的 `limit` 条。
    pub fn recent(&self, limit: usize) -> Vec<Entry> {
        let (tx, rx) = mpsc::channel();
        self.send(Cmd::Recent(limit, tx));
        rx.recv().unwrap_or_default()
    }

    /// 全文搜索。空格分隔多个关键词，需全部命中。
    pub fn search(&self, query: &str, limit: usize) -> Vec<Hit> {
        let (tx, rx) = mpsc::channel();
        self.send(Cmd::Search(query.to_string(), limit, tx));
        rx.recv().unwrap_or_default()
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

/// v0 → v1：加 seq 列和全文索引，去掉旧的 search 列。
fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= 1 {
        return Ok(());
    }
    let tx = conn.transaction()?;
    let has_column = |name: &str| -> rusqlite::Result<bool> {
        tx.query_row("SELECT COUNT(*) FROM pragma_table_info('items') WHERE name = ?1", [name], |r| {
            r.get::<_, i64>(0).map(|n| n > 0)
        })
    };
    if has_column("search")? {
        tx.execute_batch("ALTER TABLE items DROP COLUMN search")?;
    }
    if !has_column("seq")? {
        tx.execute_batch(
            "ALTER TABLE items ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
             UPDATE items SET seq = r.n
             FROM (SELECT key, row_number() OVER (ORDER BY last_used, key) AS n FROM items) AS r
             WHERE r.key = items.key;",
        )?;
    }
    tx.execute_batch(SCHEMA_V1)?;
    // 给已有数据建索引，内容规则与 ClipData::search_body 一致
    tx.execute(
        "INSERT INTO items_fts (rowid, body)
         SELECT i.seq, CASE i.kind
             WHEN 0 THEN substr(coalesce(p.text, ''), 1, ?1)
             WHEN 2 THEN '文件 file ' || replace(coalesce(p.files, ''), char(10), ' ')
             ELSE '图片 image' END
         FROM items i JOIN payloads p ON p.key = i.key",
        [SEARCH_MAX_CHARS as i64],
    )?;
    tx.execute_batch("PRAGMA user_version = 1")?;
    tx.commit()
}

fn recent(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<Entry>> {
    let mut stmt = conn.prepare_cached("SELECT key, kind, preview, last_used FROM items ORDER BY seq DESC LIMIT ?1")?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok(Entry { key: r.get(0)?, kind: Kind::from_i64(r.get(1)?), preview: r.get(2)?, last_used: r.get(3)? })
    })?;
    rows.collect()
}

/// 后台线程的状态。条数和下一个 seq 记在内存里，避免每次写入都扫表。
struct Worker {
    conn: Connection,
    max: usize,
    count: usize,
    next_seq: i64,
}

impl Worker {
    fn new(conn: Connection, max: usize) -> rusqlite::Result<Worker> {
        let (count, max_seq): (i64, i64) =
            conn.query_row("SELECT COUNT(*), COALESCE(MAX(seq), 0) FROM items", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(Worker { conn, max, count: count as usize, next_seq: max_seq + 1 })
    }

    fn run(&mut self, rx: Receiver<Cmd>) {
        for cmd in rx {
            if let Err(e) = self.handle(cmd) {
                crate::log::write(format!("store error: {e}"));
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) -> rusqlite::Result<()> {
        match cmd {
            Cmd::Upsert(r) => self.upsert(&r),
            Cmd::Touch(key, now) => {
                let seq = self.take_seq();
                let tx = self.conn.transaction()?;
                bump(&tx, key, seq, now)?;
                tx.commit()
            }
            Cmd::Delete(key) => {
                let n = self.conn.execute("DELETE FROM items WHERE key = ?1", [key])?;
                self.count = self.count.saturating_sub(n);
                Ok(())
            }
            Cmd::Clear => {
                self.count = 0;
                self.conn.execute_batch(
                    "DELETE FROM items_fts; DELETE FROM payloads; DELETE FROM items; PRAGMA wal_checkpoint(TRUNCATE);",
                )
            }
            Cmd::SetMax(m) => {
                self.max = m;
                let tx = self.conn.transaction()?;
                self.count -= trim(&tx, self.count, m)?;
                tx.commit()
            }
            Cmd::Load(key, reply) => {
                let _ = reply.send(load(&self.conn, key)?);
                Ok(())
            }
            Cmd::Recent(limit, reply) => {
                let _ = reply.send(recent(&self.conn, limit)?);
                Ok(())
            }
            Cmd::Search(query, limit, reply) => {
                // 出错也要回复，否则调用方会一直等
                let hits = search(&self.conn, &query, limit);
                let _ = reply.send(hits.as_ref().cloned().unwrap_or_default());
                hits.map(|_| ())
            }
        }
    }

    fn take_seq(&mut self) -> i64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    fn upsert(&mut self, r: &Record) -> rusqlite::Result<()> {
        let e = &r.entry;
        let seq = self.take_seq();
        let (text, html, rtf, image, files) = match &r.data {
            ClipData::Text { text, html, rtf } => (Some(text.as_str()), html.as_deref(), rtf.as_deref(), None, None),
            ClipData::Image { dib } => (None, None, None, Some(lz4_flex::compress_prepend_size(dib)), None),
            ClipData::Files { paths } => (None, None, None, None, Some(paths.join("\n"))),
        };
        let tx = self.conn.transaction()?;
        let existed = bump(&tx, e.key, seq, e.last_used)?;
        if existed {
            tx.execute(
                "UPDATE items SET preview = ?2, size = ?3 WHERE key = ?1",
                params![e.key, e.preview, r.data.size() as i64],
            )?;
        } else {
            tx.execute(
                "INSERT INTO items (key, seq, kind, preview, last_used, size) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![e.key, seq, e.kind.to_i64(), e.preview, e.last_used, r.data.size() as i64],
            )?;
            tx.execute("INSERT INTO items_fts (rowid, body) VALUES (?1, ?2)", params![seq, r.data.search_body()])?;
        }
        tx.execute(
            "INSERT INTO payloads (key, text, html, rtf, image, files) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(key) DO UPDATE SET text = excluded.text, html = excluded.html, rtf = excluded.rtf,
                 image = excluded.image, files = excluded.files",
            params![e.key, text, html, rtf, image, files],
        )?;
        let count = self.count + usize::from(!existed);
        let removed = trim(&tx, count, self.max)?;
        tx.commit()?;
        self.count = count - removed;
        Ok(())
    }
}

/// 给已有条目分配新的 seq（移到最前），索引行跟着搬到新 rowid。条目不存在时返回 false。
fn bump(conn: &Connection, key: i64, seq: i64, now: i64) -> rusqlite::Result<bool> {
    let old: Option<i64> = conn.query_row("SELECT seq FROM items WHERE key = ?1", [key], |r| r.get(0)).optional()?;
    let Some(old) = old else {
        return Ok(false);
    };
    conn.execute("UPDATE items SET seq = ?2, last_used = ?3 WHERE key = ?1", params![key, seq, now])?;
    conn.execute(
        "INSERT INTO items_fts (rowid, body) SELECT ?2, body FROM items_fts WHERE rowid = ?1",
        params![old, seq],
    )?;
    conn.execute("DELETE FROM items_fts WHERE rowid = ?1", [old])?;
    Ok(true)
}

/// 条数超过上限时删掉最旧的，返回删除条数。
fn trim(conn: &Connection, count: usize, max: usize) -> rusqlite::Result<usize> {
    if count <= max {
        return Ok(0);
    }
    conn.execute("DELETE FROM items WHERE key IN (SELECT key FROM items ORDER BY seq LIMIT ?1)", [(count - max) as i64])
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

/// FTS5 短语：用双引号包起来，内部的双引号写两遍。
fn fts_phrase(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// LIKE 模式：转义 % _ \，前后加 %。
fn like_pattern(term: &str) -> String {
    let mut p = String::from("%");
    for ch in term.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            p.push('\\');
        }
        p.push(ch);
    }
    p.push('%');
    p
}

fn search(conn: &Connection, query: &str, limit: usize) -> rusqlite::Result<Vec<Hit>> {
    let (long, short): (Vec<&str>, Vec<&str>) = query.split_whitespace().partition(|t| t.chars().count() >= 3);
    if long.is_empty() && short.is_empty() {
        return Ok(Vec::new());
    }
    let mut args: Vec<String> = Vec::new();
    let mut conds: Vec<&str> = Vec::new();
    let (from, order) = if long.is_empty() {
        // 只有短关键词：用不上 trigram 索引，从最近的若干条出发逐条匹配
        conds.push("1");
        (
            format!(
                "(SELECT key, seq, preview FROM items ORDER BY seq DESC LIMIT {SHORT_SCAN_RECENT}) AS i
                 CROSS JOIN items_fts ON items_fts.rowid = i.seq"
            ),
            "i.seq",
        )
    } else {
        conds.push("items_fts MATCH ?");
        args.push(long.iter().map(|t| fts_phrase(t)).collect::<Vec<_>>().join(" "));
        // FTS 表在外层、按 rowid 倒序输出，取够 limit 条就停
        ("items_fts CROSS JOIN items i ON i.seq = items_fts.rowid".to_string(), "items_fts.rowid")
    };
    for t in &short {
        conds.push("items_fts.body LIKE ? ESCAPE '\\'");
        args.push(like_pattern(t));
    }
    let sql = format!(
        "SELECT i.key, i.preview FROM {from} WHERE {} ORDER BY {order} DESC LIMIT {limit}",
        conds.join(" AND ")
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn record(data: ClipData, last_used: i64) -> Record {
        let entry = Entry { key: data.key(), kind: data.kind(), preview: data.preview(), last_used };
        Record { entry, data }
    }

    fn text(s: &str) -> ClipData {
        ClipData::Text { text: s.into(), html: None, rtf: None }
    }

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("clipstack-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("history.db")
    }

    fn keys(hits: &[Hit]) -> Vec<i64> {
        hits.iter().map(|h| h.0).collect()
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
            store.upsert(record(text.clone(), 3)); // 重复：只移到最前，不新增
            assert_eq!(store.recent(10).len(), 2);
            assert_eq!(store.load(text.key()), Some(text.clone()));
            assert_eq!(store.load(image.key()), Some(image.clone()));
            store.upsert(record(files.clone(), 4)); // 超过上限 2，最久没用的 image 被删掉
            assert_eq!(store.load(files.key()), Some(files.clone()));
            assert_eq!(store.load(image.key()), None);
            assert!(store.search("image", 10).is_empty(), "trimmed items leave the index");
            assert_eq!(keys(&store.search("hello", 10)), [text.key()], "re-copied item stays searchable");
        }
        let (_store, entries) = Store::open(&path, 10).unwrap();
        let keys: Vec<i64> = entries.iter().map(|e| e.key).collect();
        assert_eq!(keys, [files.key(), text.key()]);
    }

    #[test]
    fn full_text_search() {
        let path = temp_db("search");
        let (store, _) = Store::open(&path, 100).unwrap();
        let a = text("Nginx upstream TIMED OUT while reading response header");
        let b = text("用户订单查询接口返回 500 错误");
        let c = text(&format!("{}needle at the very end", "x".repeat(10_000)));
        let d = ClipData::Files { paths: vec![r"C:\logs\nginx-error.log".into()] };
        for (i, data) in [&a, &b, &c, &d].into_iter().enumerate() {
            store.upsert(record(data.clone(), i as i64));
        }

        assert_eq!(keys(&store.search("timed out", 10)), [a.key()], "case-insensitive, all terms");
        assert_eq!(keys(&store.search("订单查询", 10)), [b.key()], "Chinese substring");
        assert_eq!(keys(&store.search("needle", 10)), [c.key()], "beyond the first 4096 chars");
        assert_eq!(keys(&store.search("nginx", 10)), [d.key(), a.key()], "newest first, file paths indexed");
        assert_eq!(keys(&store.search("订单", 10)), [b.key()], "2-char term falls back to LIKE");
        assert_eq!(keys(&store.search("nginx 50", 10)), Vec::<i64>::new());
        assert_eq!(keys(&store.search("50 错误", 10)), [b.key()]);
        assert!(store.search("0%", 10).is_empty(), "LIKE wildcards are escaped");
        assert!(store.search("\"quoted\"", 10).is_empty(), "FTS syntax is escaped");
        assert_eq!(store.search("nginx", 1).len(), 1, "limit");

        store.touch(a.key(), 99);
        assert_eq!(keys(&store.search("nginx", 10)), [a.key(), d.key()], "touch moves to front");

        store.delete(a.key());
        assert_eq!(keys(&store.search("nginx", 10)), [d.key()], "deleted items leave the index");
        store.clear();
        assert!(store.search("nginx", 10).is_empty());
    }

    #[test]
    fn migrates_v0_database() {
        let path = temp_db("migrate");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE items (key INTEGER PRIMARY KEY, kind INTEGER NOT NULL, preview TEXT NOT NULL,
                     search TEXT NOT NULL, last_used INTEGER NOT NULL, size INTEGER NOT NULL);
                 CREATE INDEX items_last_used ON items(last_used DESC);
                 CREATE TABLE payloads (key INTEGER PRIMARY KEY REFERENCES items(key) ON DELETE CASCADE,
                     text TEXT, html BLOB, rtf BLOB, image BLOB, files TEXT);
                 INSERT INTO items VALUES (1, 0, 'old text', 'old text', 6, 8);
                 INSERT INTO payloads (key, text) VALUES (1, 'old clipboard text');
                 INSERT INTO items VALUES (2, 2, '[文件] a.txt', '', 5, 8);
                 INSERT INTO payloads (key, files) VALUES (2, 'C:\\dir\\a.txt');",
            )
            .unwrap();
        }
        let (store, entries) = Store::open(&path, 100).unwrap();
        assert_eq!(entries.iter().map(|e| e.key).collect::<Vec<_>>(), [1, 2], "order kept by last_used");
        assert_eq!(keys(&store.search("clipboard", 10)), [1]);
        assert_eq!(keys(&store.search("a.txt", 10)), [2]);
        store.upsert(record(text("brand new clipboard"), 7));
        assert_eq!(store.search("clipboard", 10).len(), 2, "new seq continues after migrated ones");
        drop(store);
        // 再次打开不会重复迁移
        let (store, _) = Store::open(&path, 100).unwrap();
        assert_eq!(store.search("clipboard", 10).len(), 2);
    }

    /// 性能测试：cargo test --release -- --ignored --nocapture bench_search
    #[test]
    #[ignore]
    fn bench_search_100k() {
        let path = temp_db("bench");
        let (store, _) = Store::open(&path, 200_000).unwrap();
        let words = [
            "error",
            "日志",
            "select",
            "from",
            "where",
            "用户",
            "timeout",
            "订单",
            "nginx",
            "数据库",
            "panic",
            "接口",
            "the",
            "request",
            "failed",
            "返回",
            "status",
            "http",
            "https://example.com/api/v1/",
            "user_id",
            "查询",
            "connection",
            "refused",
            "INFO",
            "WARN",
            "func",
            "return",
            "nil",
            "err",
            "配置",
            "服务",
            "部署",
        ];
        let t = Instant::now();
        let mut seed = 1u64;
        for i in 0..100_000i64 {
            let mut s = String::new();
            while s.len() < 600 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                s.push_str(words[(seed >> 33) as usize % words.len()]);
                s.push(' ');
                if seed.is_multiple_of(5) {
                    s.push_str(&format!("{} ", (seed >> 40) % 10_000));
                }
            }
            store.upsert(record(text(&s), i));
        }
        store.recent(1); // 等写入完成
        println!("insert 100k: {:.1}s", t.elapsed().as_secs_f64());
        for q in ["timeout", "nginx 订单", "connection refused 查询", "zzzzzz", "日志", "a 订单", "7342"] {
            let t = Instant::now();
            let n = store.search(q, 200).len();
            println!("search {q:<24} -> {n:>3} hits in {:>6.2} ms", t.elapsed().as_secs_f64() * 1000.0);
        }
        let mb = |p: std::path::PathBuf| std::fs::metadata(p).map_or(0.0, |m| m.len() as f64 / 1e6);
        println!("db {:.0} MB, wal {:.0} MB", mb(path.clone()), mb(path.with_extension("db-wal")));
    }
}
