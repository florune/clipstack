//! 内存里只缓存最近的一小段历史（最新的在最前面），供快捷键、宏和面板首屏使用。
//! 完整历史和全文索引都在 SQLite 里，内存占用不随历史条数增长。

/// 内存里最多缓存多少条最近的历史。
pub const RECENT_CACHE: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image,
    Files,
}

impl Kind {
    pub fn to_i64(self) -> i64 {
        match self {
            Kind::Text => 0,
            Kind::Image => 1,
            Kind::Files => 2,
        }
    }

    pub fn from_i64(v: i64) -> Kind {
        match v {
            1 => Kind::Image,
            2 => Kind::Files,
            _ => Kind::Text,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub key: i64,
    pub kind: Kind,
    pub preview: String,
    pub last_used: i64,
}

pub struct History {
    items: Vec<Entry>,
    cap: usize,
}

impl History {
    /// `max_items` 是配置里的历史上限；缓存取它和 RECENT_CACHE 中较小的那个。
    pub fn new(items: Vec<Entry>, max_items: usize) -> History {
        let mut h = History { items, cap: max_items.min(RECENT_CACHE) };
        h.items.truncate(h.cap);
        h
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    pub fn get(&self, index: usize) -> Option<&Entry> {
        self.items.get(index)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.items.iter()
    }

    /// 插入到最前面；同 key 的旧条目会被替换。
    pub fn push_front(&mut self, entry: Entry) {
        self.remove(entry.key);
        self.items.insert(0, entry);
        self.items.truncate(self.cap);
    }

    /// 把已有条目移到最前面；不在缓存里返回 false。
    pub fn touch(&mut self, key: i64, now: i64) -> bool {
        let Some(pos) = self.items.iter().position(|e| e.key == key) else {
            return false;
        };
        let mut e = self.items.remove(pos);
        e.last_used = now;
        self.items.insert(0, e);
        true
    }

    pub fn remove(&mut self, key: i64) -> bool {
        let before = self.items.len();
        self.items.retain(|e| e.key != key);
        self.items.len() != before
    }

    /// 用数据库里的最新数据整体替换缓存。
    pub fn replace(&mut self, mut items: Vec<Entry>) {
        items.truncate(self.cap);
        self.items = items;
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn set_max(&mut self, max_items: usize) {
        self.cap = max_items.min(RECENT_CACHE);
        self.items.truncate(self.cap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(key: i64) -> Entry {
        Entry { key, kind: Kind::Text, preview: key.to_string(), last_used: 0 }
    }

    #[test]
    fn push_front_dedups_and_truncates() {
        let mut h = History::new(vec![], 3);
        for k in [1, 2, 3, 2, 4] {
            h.push_front(e(k));
        }
        let keys: Vec<i64> = h.iter().map(|e| e.key).collect();
        assert_eq!(keys, [4, 2, 3]);
    }

    #[test]
    fn touch_moves_to_front() {
        let mut h = History::new(vec![e(1), e(2), e(3)], 10);
        assert!(h.touch(3, 99));
        assert_eq!(h.get(0).unwrap().key, 3);
        assert_eq!(h.get(0).unwrap().last_used, 99);
        assert!(!h.touch(42, 0));
    }

    #[test]
    fn cache_is_bounded() {
        let h = History::new((0..2000).map(e).collect(), 100_000);
        assert_eq!(h.cap(), RECENT_CACHE);
        assert_eq!(h.iter().count(), RECENT_CACHE);
    }
}
