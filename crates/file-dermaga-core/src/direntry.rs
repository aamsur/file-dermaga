//! Directory entry and directory listing types.
//!
//! A [`DirEntry`] describes one file, directory or link as reported by a
//! server; a [`DirectoryListing`] is the ordered collection of entries for
//! one remote path.

use crate::datetime::{Accuracy, DateTime};

/// The entry is a directory (or may be one, for links).
pub const FLAG_DIR: u32 = 1;
/// The entry is a symbolic link.
pub const FLAG_LINK: u32 = 2;
/// The entry's information may be stale or guessed.
pub const FLAG_UNSURE: u32 = 4;

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq)]
pub struct DirEntry {
    pub name: String,
    /// Size in bytes, `-1` when unknown.
    pub size: i64,
    /// Permissions as reported by the server, `""` when unknown.
    pub permissions: String,
    /// `"owner group"`, `"owner"`, or `""`.
    pub owner_group: String,
    /// Symlink target if known.
    pub target: Option<String>,
    /// Modification time; empty when unknown.
    pub time: DateTime,
    /// Combination of `FLAG_*` values.
    pub flags: u32,
}

impl Default for DirEntry {
    fn default() -> Self {
        DirEntry {
            name: String::new(),
            size: -1,
            permissions: String::new(),
            owner_group: String::new(),
            target: None,
            time: DateTime::empty(),
            flags: 0,
        }
    }
}

impl DirEntry {
    pub fn is_dir(&self) -> bool {
        self.flags & FLAG_DIR != 0
    }

    pub fn is_link(&self) -> bool {
        self.flags & FLAG_LINK != 0
    }

    pub fn is_unsure(&self) -> bool {
        self.flags & FLAG_UNSURE != 0
    }

    /// True when the entry has a (non-empty) timestamp.
    pub fn has_date(&self) -> bool {
        !self.time.is_empty()
    }

    /// True when the timestamp includes at least the hour.
    pub fn has_time(&self) -> bool {
        self.has_date() && self.time.accuracy() >= Accuracy::Hours
    }

    /// True when the timestamp includes seconds.
    pub fn has_seconds(&self) -> bool {
        self.has_date() && self.time.accuracy() >= Accuracy::Seconds
    }

    /// One-line human readable description, mainly for test messages.
    pub fn dump(&self) -> String {
        let mut kinds = Vec::new();
        if self.is_dir() {
            kinds.push("dir");
        }
        if self.is_link() {
            kinds.push("link");
        }
        if self.is_unsure() {
            kinds.push("unsure");
        }
        let kind = if kinds.is_empty() {
            "file".to_string()
        } else {
            kinds.join("|")
        };
        let mut s = format!(
            "name={:?} {} size={} perms={:?} owner_group={:?} time={}",
            self.name, kind, self.size, self.permissions, self.owner_group, self.time
        );
        if self.has_date() {
            s.push_str(&format!(" ({:?})", self.time.accuracy()));
        }
        if let Some(t) = &self.target {
            s.push_str(&format!(" target={t:?}"));
        }
        s
    }
}

/// The entries of one remote directory.
#[derive(Clone, Debug, Default)]
pub struct DirectoryListing {
    pub path: String,
    pub entries: Vec<DirEntry>,
    /// True when the listing could not be obtained or parsed.
    pub failed: bool,
}

impl DirectoryListing {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_entry() {
        let e = DirEntry::default();
        assert_eq!(e.size, -1);
        assert_eq!(e.flags, 0);
        assert!(!e.is_dir() && !e.is_link() && !e.is_unsure());
        assert!(!e.has_date() && !e.has_time() && !e.has_seconds());
        assert!(e.target.is_none());
    }

    #[test]
    fn flags_and_time_helpers() {
        let e = DirEntry {
            name: "x".into(),
            flags: FLAG_DIR | FLAG_LINK | FLAG_UNSURE,
            time: DateTime::ymd_hm(2024, 3, 14, 9, 15),
            target: Some("y".into()),
            ..Default::default()
        };
        assert!(e.is_dir() && e.is_link() && e.is_unsure());
        assert!(e.has_date() && e.has_time() && !e.has_seconds());
        let d = e.dump();
        assert!(d.contains("dir|link|unsure"), "{d}");
        assert!(d.contains("2024-03-14 09:15"), "{d}");
        assert!(d.contains("target=\"y\""), "{d}");
        let day = DirEntry {
            time: DateTime::ymd(2024, 3, 14),
            ..Default::default()
        };
        assert!(day.has_date() && !day.has_time());
    }

    #[test]
    fn listing_len() {
        let mut l = DirectoryListing::default();
        assert!(l.is_empty());
        l.entries.push(DirEntry::default());
        assert_eq!(l.len(), 1);
        assert!(!l.failed);
    }
}
