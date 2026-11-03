//! 内存里的历史列表（最新的在最前面），面板和快捷键都从这里取数据。
//! 完整内容在 SQLite 里，这里只放预览和搜索用的文本。

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
    /// 小写，用于搜索。
    pub search: String,
    pub last_used: i64,
}

pub struct History {
    items: Vec<Entry>,
    max: usize,
}

impl History {
    pub fn new(mut items: Vec<Entry>, max: usize) -> History {
        items.truncate(max);
        History { items, max }
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
        self.items.truncate(self.max);
    }

    /// 把已有条目移到最前面。
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

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn set_max(&mut self, max: usize) {
        self.max = max;
        self.items.truncate(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(key: i64) -> Entry {
        Entry { key, kind: Kind::Text, preview: key.to_string(), search: String::new(), last_used: 0 }
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
}
