//! Streaming parser for FTP directory listings (`LIST`, `MLSD`, `NLST`).
//!
//! [`ListingParser`] accepts the raw bytes of a data connection in chunks of
//! any size, splits them into lines (LF, CR, CRLF or NUL), detects and
//! translates EBCDIC, decodes each line as UTF-8 or Latin-1 and recognises
//! one of many server listing formats per line. Two-line OpenVMS entries
//! are joined, and a listing consisting only of bare names is treated as
//! `NLST` output.

mod ebcdic;
mod fields;
mod formats;
mod months;

use crate::direntry::{DirEntry, DirectoryListing};
use crate::server::ServerType;
use formats::{parse_line, LineResult};

/// Lines longer than this many bytes make the listing fail.
const MAX_LINE: usize = 10_000;
/// Number of leading bytes used to decide whether the data is EBCDIC.
const DETECT_BYTES: usize = 512;

/// Incremental directory listing parser.
#[derive(Debug)]
pub struct ListingParser {
    /// Server type for lines fed from now on.
    server_type: ServerType,
    /// Server type used for the lines currently being split.
    active_type: ServerType,
    /// Server type changes that happened while bytes were held in `head`:
    /// (offset in `head`, new type).
    type_changes: Vec<(usize, ServerType)>,
    tz_offset_minutes: i32,
    limit: Option<usize>,
    truncated: bool,
    error: Option<String>,
    /// `None` until enough bytes were seen to decide.
    ebcdic: Option<bool>,
    /// Bytes held back until the EBCDIC decision.
    head: Vec<u8>,
    /// Bytes of the current, unterminated line.
    line: Vec<u8>,
    /// Unmatched single-token line that may be the first half of a wrapped
    /// OpenVMS entry.
    pending: Option<String>,
    entries: Vec<DirEntry>,
    /// Some line was recognised as a listing entry (returned or not).
    any_matched: bool,
    /// Every unmatched line so far was free of blanks.
    names_only: bool,
    /// Unmatched lines, kept while the listing may be a bare name list.
    names: Vec<String>,
    now_ms: Option<i64>,
}

impl ListingParser {
    pub fn new(server_type: ServerType) -> Self {
        ListingParser {
            server_type,
            active_type: server_type,
            type_changes: Vec::new(),
            tz_offset_minutes: 0,
            limit: None,
            truncated: false,
            error: None,
            ebcdic: None,
            head: Vec::new(),
            line: Vec::new(),
            pending: None,
            entries: Vec::new(),
            any_matched: false,
            names_only: true,
            names: Vec::new(),
            now_ms: None,
        }
    }

    /// Changes the server type for lines fed afterwards.
    pub fn set_server_type(&mut self, t: ServerType) {
        self.server_type = t;
        if self.ebcdic.is_none() && !self.head.is_empty() {
            self.type_changes.push((self.head.len(), t));
        } else {
            self.active_type = t;
        }
    }

    /// Minutes added to every non-empty parsed time (applied when the
    /// listing is returned by [`parse`](Self::parse)).
    pub fn set_server_timezone_offset(&mut self, minutes: i32) {
        self.tz_offset_minutes = minutes;
    }

    /// Maximum number of entries to keep; further entries are dropped.
    pub fn set_limit(&mut self, max_entries: usize) {
        self.limit = Some(max_entries);
    }

    /// True if entries were dropped because of the limit.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Append raw bytes from the data connection. Chunk boundaries may fall
    /// anywhere. Returns false once a fatal error occurred.
    pub fn feed(&mut self, data: &[u8]) -> bool {
        if self.error.is_some() {
            return false;
        }
        match self.ebcdic {
            Some(_) => self.push_bytes(data),
            None => {
                self.head.extend_from_slice(data);
                if self.head.len() >= DETECT_BYTES {
                    self.decide_encoding(false);
                }
            }
        }
        self.error.is_none()
    }

    /// Finish: parse what is left and return all entries in listing order.
    pub fn parse(&mut self, path: &str) -> DirectoryListing {
        if self.error.is_none() {
            if self.ebcdic.is_none() {
                self.decide_encoding(true);
            }
            if self.error.is_none() {
                self.end_line();
                if let Some(p) = self.pending.take() {
                    self.unmatched(&p);
                }
            }
        }
        if self.error.is_some() {
            return DirectoryListing {
                path: path.to_string(),
                entries: Vec::new(),
                failed: true,
            };
        }
        let mut entries = self.entries.clone();
        if !self.any_matched && self.names_only {
            for name in &self.names {
                if name == "." || name == ".." {
                    continue;
                }
                entries.push(DirEntry {
                    name: name.clone(),
                    ..Default::default()
                });
            }
        }
        if let Some(limit) = self.limit {
            if entries.len() > limit {
                entries.truncate(limit);
                self.truncated = true;
            }
        }
        if self.tz_offset_minutes != 0 {
            let delta = i64::from(self.tz_offset_minutes) * 60_000;
            for e in &mut entries {
                e.time.add_millis(delta);
            }
        }
        DirectoryListing {
            path: path.to_string(),
            entries,
            failed: false,
        }
    }

    /// Message describing why the listing failed, if it did.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Forget all data and errors; settings (server type, timezone offset,
    /// limit) are kept.
    pub fn reset(&mut self) {
        let mut fresh = ListingParser::new(self.server_type);
        fresh.tz_offset_minutes = self.tz_offset_minutes;
        fresh.limit = self.limit;
        *self = fresh;
    }

    fn decide_encoding(&mut self, at_end: bool) {
        let window = &self.head[..self.head.len().min(DETECT_BYTES)];
        self.ebcdic = Some(ebcdic::looks_like_ebcdic(window, at_end));
        let head = std::mem::take(&mut self.head);
        let changes = std::mem::take(&mut self.type_changes);
        let mut done = 0;
        for (offset, t) in changes {
            self.push_bytes(&head[done..offset]);
            done = offset;
            self.active_type = t;
        }
        self.push_bytes(&head[done..]);
        self.active_type = self.server_type;
    }

    fn push_bytes(&mut self, data: &[u8]) {
        let translate = self.ebcdic == Some(true);
        for &raw in data {
            let b = if translate {
                ebcdic::to_ascii(raw)
            } else {
                raw
            };
            if b == b'\n' || b == b'\r' || b == 0 {
                self.end_line();
            } else {
                if self.line.len() >= MAX_LINE {
                    self.error = Some(format!(
                        "listing line longer than {MAX_LINE} bytes; the server output is not a directory listing"
                    ));
                    self.line.clear();
                    return;
                }
                self.line.push(b);
            }
        }
    }

    fn end_line(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let raw = std::mem::take(&mut self.line);
        let bytes = raw.strip_prefix(b"\xEF\xBB\xBF".as_slice()).unwrap_or(&raw);
        let text = match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
        };
        if text.trim().is_empty() {
            return;
        }
        self.handle_line(&text);
    }

    fn handle_line(&mut self, text: &str) {
        let st = self.active_type;
        let now = *self
            .now_ms
            .get_or_insert_with(|| crate::DateTime::now().unix_millis().unwrap_or(0));
        if let Some(prev) = self.pending.take() {
            let combined = format!("{prev} {}", text.trim_start_matches(fields::is_blank));
            match parse_line(&combined, st, now) {
                LineResult::NoMatch => self.unmatched(&prev),
                r => {
                    self.record(r);
                    return;
                }
            }
        }
        match parse_line(text, st, now) {
            LineResult::NoMatch => {
                let t = text.trim();
                if !t.contains([' ', '\t']) && t.contains(';') {
                    self.pending = Some(t.to_string());
                } else {
                    self.unmatched(text);
                }
            }
            r => self.record(r),
        }
    }

    fn record(&mut self, r: LineResult) {
        self.any_matched = true;
        self.names.clear();
        if let LineResult::Entry(e) = r {
            if self.limit.is_some_and(|l| self.entries.len() >= l) {
                self.truncated = true;
            } else {
                self.entries.push(e);
            }
        }
    }

    fn unmatched(&mut self, text: &str) {
        if self.any_matched || !self.names_only {
            return;
        }
        let t = text.trim();
        if t.contains([' ', '\t']) {
            self.names_only = false;
            self.names.clear();
        } else {
            self.names.push(t.to_string());
        }
    }
}

/// Parse a complete listing in one call: `new` + `feed` + `parse("")`.
pub fn parse_listing(data: &[u8], server_type: ServerType) -> DirectoryListing {
    let mut p = ListingParser::new(server_type);
    p.feed(data);
    p.parse("")
}
