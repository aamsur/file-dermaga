//! Per-line recognisers for the supported listing formats (MLSD, EPLF,
//! Unix `ls -l` and relatives, numeric Unix, DOS/IIS, OpenVMS, IBM i,
//! Nortel WfFtp, OS/2, VxWorks and size-first, MVS, z/VM, HP NonStop,
//! OS-9) and the dispatcher
//! that tries them in a fixed order.

use super::fields::*;
use crate::datetime::{Accuracy, DateTime};
use crate::direntry::{DirEntry, FLAG_DIR, FLAG_LINK};
use crate::server::ServerType;

/// Outcome of parsing one line.
#[derive(Debug)]
pub(crate) enum LineResult {
    /// A listable entry.
    Entry(DirEntry),
    /// Recognised, but not to be returned (`.`, `..`, `cdir`, `pdir`).
    Skip,
    /// Not recognised.
    NoMatch,
}

/// Parse one already decoded line.
pub(crate) fn parse_line(line: &str, st: ServerType, now_ms: i64) -> LineResult {
    if line.starts_with('+') {
        if let Some(r) = eplf(line) {
            return r;
        }
    }
    if let Some(r) = mlsd(line) {
        return r;
    }
    let toks = tokenize(line);
    if toks.is_empty() {
        return LineResult::NoMatch;
    }
    let gated = match st {
        ServerType::Mvs => mvs_only(line, &toks),
        ServerType::Zvm => zvm(&toks),
        ServerType::HpNonstop => hp_nonstop(line, &toks),
        _ => None,
    };
    if let Some(e) = gated {
        return finish(e);
    }
    type Parser = fn(&str, &[Tok<'_>], i64) -> Option<DirEntry>;
    const ALWAYS: [Parser; 10] = [
        unix,
        numeric_unix,
        dos,
        vms,
        ibm_i,
        nortel,
        size_first,
        mvs_dataset,
        mvs_member,
        os9,
    ];
    for p in ALWAYS {
        if let Some(e) = p(line, &toks, now_ms) {
            return finish(e);
        }
    }
    LineResult::NoMatch
}

fn finish(e: DirEntry) -> LineResult {
    match e.name.as_str() {
        "" => LineResult::NoMatch,
        "." | ".." => LineResult::Skip,
        _ => LineResult::Entry(e),
    }
}

// ---------------------------------------------------------------- MLSD

fn mlsd(line: &str) -> Option<LineResult> {
    // Facts end with ';' followed by exactly one blank (space or tab).
    let bytes = line.as_bytes();
    let sep = (0..bytes.len().saturating_sub(1))
        .find(|&i| bytes[i] == b';' && (bytes[i + 1] == b' ' || bytes[i + 1] == b'\t'));
    if let Some(pos) = sep {
        if let Some(r) = mlsd_entry(&line[..pos], &line[pos + 2..], false) {
            return Some(r);
        }
    }
    // Older drafts: no ';' after the last fact, the name follows the first blank.
    let blank = line.find([' ', '\t'])?;
    let facts = &line[..blank];
    if !facts.contains(';') || facts.ends_with(';') {
        return None;
    }
    mlsd_entry(facts, &line[blank + 1..], true)
}

/// Build an entry from the fact list and the name. `legacy` (no final
/// `;`) additionally requires at least one well-known fact.
fn mlsd_entry(facts: &str, name: &str, legacy: bool) -> Option<LineResult> {
    let facts = facts.trim_start_matches(' ');
    if facts.is_empty() || facts.contains([' ', '\t']) {
        return None;
    }
    let mut pairs = Vec::new();
    for piece in facts.split(';') {
        if piece.is_empty() {
            continue;
        }
        let (k, v) = piece.split_once('=')?;
        if k.is_empty() {
            return None;
        }
        pairs.push((k.to_ascii_lowercase(), v));
    }
    if pairs.is_empty() {
        return None;
    }
    let known = ["type", "size", "modify", "perm", "unix.mode"];
    if legacy && !pairs.iter().any(|(k, _)| known.contains(&k.as_str())) {
        return None;
    }
    if name.is_empty() {
        return Some(LineResult::NoMatch);
    }
    let fact = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| *v);
    let mut e = DirEntry {
        name: name.to_string(),
        ..Default::default()
    };
    if let Some(t) = fact("type") {
        let lt = t.to_ascii_lowercase();
        if lt == "cdir" || lt == "pdir" {
            return Some(LineResult::Skip);
        }
        if lt == "dir" {
            e.flags = FLAG_DIR;
        } else if lt.starts_with("os.unix=slink") || lt.starts_with("os.unix=symlink") {
            e.flags = FLAG_DIR | FLAG_LINK;
            if let Some((_, target)) = t.split_once(':') {
                if !target.is_empty() {
                    e.target = Some(target.to_string());
                }
            }
        }
    }
    if let Some(s) = fact("size") {
        match uint(s) {
            Some(v) => e.size = v,
            None => return Some(LineResult::NoMatch),
        }
    }
    if let Some(m) = fact("modify") {
        match DateTime::parse_compact(m) {
            Some(t) => e.time = t,
            None => return Some(LineResult::NoMatch),
        }
    } else if let Some(c) = fact("create") {
        e.time = DateTime::parse_compact(c).unwrap_or_default();
    }
    if let Some(p) = fact("unix.mode").or_else(|| fact("perm")) {
        e.permissions = p.to_string();
    }
    let owner = ["unix.ownername", "unix.owner", "unix.user", "unix.uid"]
        .iter()
        .find_map(|k| fact(k));
    let group = ["unix.groupname", "unix.group", "unix.gid"]
        .iter()
        .find_map(|k| fact(k));
    e.owner_group = join_owner(owner.unwrap_or(""), group.unwrap_or(""));
    Some(finish(e))
}

fn join_owner(owner: &str, group: &str) -> String {
    match (owner.is_empty(), group.is_empty()) {
        (false, false) => format!("{owner} {group}"),
        (false, true) => owner.to_string(),
        (true, false) => group.to_string(),
        (true, true) => String::new(),
    }
}

// ---------------------------------------------------------------- EPLF

fn eplf(line: &str) -> Option<LineResult> {
    let (facts, name) = line[1..].split_once('\t')?;
    if name.is_empty() {
        return None;
    }
    let mut e = DirEntry {
        name: name.to_string(),
        ..Default::default()
    };
    for f in facts.split(',') {
        let mut chars = f.chars();
        let kind = chars.next();
        let value = chars.as_str();
        match kind {
            Some('/') => e.flags |= FLAG_DIR,
            Some('s') => e.size = uint(value)?,
            Some('m') => e.time = DateTime::from_unix_secs(uint(value)?, Accuracy::Seconds),
            Some('u') => {
                if let Some(p) = value.strip_prefix('p') {
                    e.permissions = p.to_string();
                }
            }
            _ => {}
        }
    }
    Some(finish(e))
}

// ---------------------------------------------------------------- Unix

fn is_perm_chars(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if "-dlbcps".contains(c) => {}
        _ => return false,
    }
    let rest = chars.as_str();
    if rest.starts_with('[') && rest.ends_with(']') && rest.len() >= 2 {
        return true;
    }
    (6..=14).contains(&rest.len()) && rest.chars().all(|c| "rwxsStTlL-+.@?*".contains(c))
}

/// Group name glued to the size: `supergroup123456`.
fn split_glued(s: &str) -> Option<(&str, i64)> {
    let digits_start = s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits_start == 0 || digits_start == s.len() {
        return None;
    }
    let (group, size) = s.split_at(digits_start);
    if !group.chars().any(|c| c.is_alphabetic()) {
        return None;
    }
    Some((group, uint(size)?))
}

/// Remove one `ls -F` type indicator.
fn strip_indicator(name: &str) -> &str {
    if name.chars().count() > 1 {
        for c in ['/', '*', '|'] {
            if let Some(n) = name.strip_suffix(c) {
                return n;
            }
        }
    }
    name
}

fn unix(line: &str, toks: &[Tok<'_>], now_ms: i64) -> Option<DirEntry> {
    let t0 = toks.first()?.s;
    let (perms, idx) = if t0.chars().count() == 1
        && "-dlbcps".contains(t0)
        && toks
            .get(1)
            .is_some_and(|t| t.s.starts_with('[') && t.s.ends_with(']'))
    {
        (format!("{} {}", t0, toks[1].s), 2)
    } else if is_perm_chars(t0) {
        (t0.to_string(), 1)
    } else {
        return None;
    };
    let kind = perms.chars().next()?;
    let device = kind == 'b' || kind == 'c';
    for j in idx + 1..toks.len() {
        let tok = toks[j].s;
        let mut size_end = j + 1;
        let mut glued_group = None;
        let size = if let Some(s) = size_with_unit(tok) {
            s
        } else if device
            && tok.len() > 1
            && tok.ends_with(',')
            && all_digits(&tok[..tok.len() - 1])
            && toks.get(j + 1).is_some_and(|t| all_digits(t.s))
        {
            size_end = j + 2;
            -1
        } else if let Some((g, s)) = split_glued(tok) {
            glued_group = Some(g);
            s
        } else {
            continue;
        };
        let Some((time, next)) = unix_date(toks, size_end, now_ms) else {
            continue;
        };
        let perms = perms.clone();
        let between: Vec<&str> = toks[idx..j].iter().map(|t| t.s).collect();
        let Some(owner_group) = unix_owner(&between, glued_group) else {
            continue;
        };
        let full = rest(line, toks, next)?;
        return Some(unix_entry(perms, size, owner_group, time, full));
    }
    // No dated interpretation: `perms [links] owner [group] size name`.
    // The usual column counts are tried first.
    for n_between in [3, 2, 4, 1] {
        let j = idx + n_between;
        if j + 1 >= toks.len() {
            continue;
        }
        let Some(size) = size_with_unit(toks[j].s) else {
            continue;
        };
        let between: Vec<&str> = toks[idx..j].iter().map(|t| t.s).collect();
        let Some(owner_group) = unix_owner(&between, None) else {
            continue;
        };
        let full = rest(line, toks, j + 1)?;
        return Some(unix_entry(
            perms,
            size,
            owner_group,
            DateTime::empty(),
            full,
        ));
    }
    None
}

/// Owner/group from the tokens between permissions and size: an optional
/// numeric link count (a lone token is always the owner), then up to three
/// owner/group tokens (Windows domain accounts use several), plus a group
/// glued to the size if there was one.
fn unix_owner(between: &[&str], glued: Option<&str>) -> Option<String> {
    let mut parts: Vec<&str> = between.to_vec();
    if parts.len() > 1 && all_digits(parts[0]) {
        parts.remove(0);
    }
    if let Some(g) = glued {
        parts.push(g);
    }
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    Some(parts.join(" "))
}

fn unix_entry(
    perms: String,
    size: i64,
    owner_group: String,
    time: DateTime,
    full: &str,
) -> DirEntry {
    let kind = perms.chars().next().unwrap_or('-');
    let mut e = DirEntry {
        size,
        permissions: perms,
        owner_group,
        time,
        ..Default::default()
    };
    match kind {
        'd' => {
            e.flags = FLAG_DIR;
            e.name = strip_indicator(full).to_string();
        }
        'l' => {
            e.flags = FLAG_DIR | FLAG_LINK;
            match full.split_once(" -> ") {
                Some((n, t)) => {
                    e.name = strip_indicator(n.trim_end_matches(is_blank)).to_string();
                    let t = t.trim_start_matches(is_blank);
                    if !t.is_empty() {
                        e.target = Some(t.to_string());
                    }
                }
                None => e.name = strip_indicator(full).to_string(),
            }
        }
        _ => e.name = strip_indicator(full).to_string(),
    }
    e
}

// ---------------------------------------------------------------- numeric Unix

fn numeric_unix(line: &str, toks: &[Tok<'_>], _now: i64) -> Option<DirEntry> {
    if toks.len() < 6 {
        return None;
    }
    let mode = toks[0].s;
    if !(5..=7).contains(&mode.len()) || !mode.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    let (uid, gid) = (toks[1].s, toks[2].s);
    if !all_digits(uid) || !all_digits(gid) {
        return None;
    }
    let size = uint(toks[3].s)?;
    let secs = uint(toks[4].s)?;
    let m = u32::from_str_radix(mode, 8).ok()?;
    let flags = match (m >> 12) & 0o17 {
        0o04 => FLAG_DIR,
        0o12 => FLAG_DIR | FLAG_LINK,
        _ => 0,
    };
    Some(DirEntry {
        name: rest(line, toks, 5)?.to_string(),
        size,
        permissions: mode.to_string(),
        owner_group: format!("{uid} {gid}"),
        time: DateTime::from_unix_secs(secs, Accuracy::Seconds),
        flags,
        ..Default::default()
    })
}

// ---------------------------------------------------------------- DOS / IIS

/// Time with optional AM/PM (glued or as the next token). Returns the time
/// and the number of tokens used.
fn dos_time(toks: &[Tok<'_>], i: usize) -> Option<(TimeOfDay, usize)> {
    let s = toks.get(i)?.s;
    let upper = s.to_ascii_uppercase();
    let (body, mut ampm, mut used) = if let Some(b) = upper.strip_suffix("AM") {
        (b.to_string(), Some(false), 1)
    } else if let Some(b) = upper.strip_suffix("PM") {
        (b.to_string(), Some(true), 1)
    } else if let Some(b) = upper.strip_suffix('A') {
        (b.to_string(), Some(false), 1)
    } else if let Some(b) = upper.strip_suffix('P') {
        (b.to_string(), Some(true), 1)
    } else {
        (upper.clone(), None, 1)
    };
    let mut t = time_of_day(&body)?;
    if ampm.is_none() {
        match toks.get(i + 1).map(|t| t.s.to_ascii_uppercase()).as_deref() {
            Some("AM") => {
                ampm = Some(false);
                used = 2;
            }
            Some("PM") => {
                ampm = Some(true);
                used = 2;
            }
            _ => {}
        }
    }
    if let Some(pm) = ampm {
        if t.h == 0 || t.h > 12 {
            return None;
        }
        t.h = t.h % 12 + if pm { 12 } else { 0 };
    }
    Some((t, used))
}

fn dos(line: &str, toks: &[Tok<'_>], _now: i64) -> Option<DirEntry> {
    let (y, mo, d) = numeric_date(toks.first()?.s)?;
    let (t, used) = dos_time(toks, 1)?;
    let k = 1 + used;
    let field = toks.get(k)?.s;
    let mut e = DirEntry {
        time: make_dt(y, mo, d, Some(t))?,
        ..Default::default()
    };
    if field.eq_ignore_ascii_case("<DIR>") {
        e.flags = FLAG_DIR;
    } else {
        e.size = size_with_separators(field)?;
    }
    e.name = rest(line, toks, k + 1)?.to_string();
    Some(e)
}

// ---------------------------------------------------------------- OpenVMS

fn vms_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '^' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Index of the version separator `;` if `name` looks like `NAME;VERSION`.
fn vms_version_pos(name: &str) -> Option<usize> {
    let pos = name.rfind(';')?;
    if pos == 0 || !all_digits(&name[pos + 1..]) {
        return None;
    }
    Some(pos)
}

/// VMS size: `used[/allocated]` blocks of 512 bytes, or bytes with a unit
/// (`3KB`).
fn vms_size(s: &str) -> Option<i64> {
    if let Some((used, alloc)) = s.split_once('/') {
        if !all_digits(alloc) {
            return None;
        }
        return uint(used)?.checked_mul(512);
    }
    if all_digits(s) {
        return uint(s)?.checked_mul(512);
    }
    if s.chars().any(|c| c.is_ascii_alphabetic()) {
        return size_with_unit(s);
    }
    None
}

/// `DD-MON-YYYY` with a four-digit year.
fn vms_date(s: &str) -> Option<(i64, u32, u32)> {
    if s.rsplit('-').next().map(str::len) != Some(4) {
        return None;
    }
    day_mon_year(s)
}

/// An owner written without brackets (`OPSUSER`).
fn is_bare_owner(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_$,.-".contains(c))
}

/// `NAME;VERSION` followed, in any order, by size, date [time], owner
/// (`[...]` or bare) and permissions `(...)`. A date is required.
fn vms(line: &str, toks: &[Tok<'_>], _now: i64) -> Option<DirEntry> {
    let raw = toks.first()?.s;
    let vpos = vms_version_pos(raw)?;
    let mut size = None;
    let mut date = None;
    let mut time = None;
    let mut owner: Option<String> = None;
    let mut perms: Option<String> = None;
    let mut i = 1;
    while i < toks.len() {
        let s = toks[i].s;
        let close = if s.starts_with('[') {
            Some(']')
        } else if s.starts_with('(') {
            Some(')')
        } else {
            None
        };
        if let Some(close) = close {
            let last = toks[i..].iter().position(|t| t.s.ends_with(close))? + i;
            let text = line.get(toks[i].start..toks[last].end())?;
            let inner = text.get(1..text.len().checked_sub(1)?)?.to_string();
            if close == ']' {
                owner = Some(inner);
            } else {
                perms = Some(inner);
            }
            i = last + 1;
            continue;
        }
        if date.is_none() {
            if let Some(d) = vms_date(s) {
                date = Some(d);
                i += 1;
                continue;
            }
        }
        if date.is_some() && time.is_none() {
            if let Some(t) = time_of_day(s) {
                time = Some(t);
                i += 1;
                continue;
            }
        }
        if size.is_none() {
            if let Some(z) = vms_size(s) {
                size = Some(z);
                i += 1;
                continue;
            }
        }
        if owner.is_none() && is_bare_owner(s) {
            owner = Some(s.to_string());
            i += 1;
            continue;
        }
        return None;
    }
    let (y, mo, d) = date?;
    let mut e = DirEntry {
        size: size.unwrap_or(-1),
        time: make_dt(y, mo, d, time)?,
        owner_group: owner.unwrap_or_default(),
        permissions: perms.unwrap_or_default(),
        ..Default::default()
    };
    let (base, version) = raw.split_at(vpos);
    let is_dir = base.len() >= 4
        && base.is_char_boundary(base.len() - 4)
        && base[base.len() - 4..].eq_ignore_ascii_case(".DIR")
        && !base[..base.len() - 4].ends_with('^');
    if is_dir {
        e.flags = FLAG_DIR;
        let stem = vms_unescape(&base[..base.len() - 4]);
        e.name = if version == ";1" {
            stem
        } else {
            format!("{stem}{version}")
        };
    } else {
        e.name = vms_unescape(raw);
    }
    Some(e)
}

// ---------------------------------------------------------------- IBM i

fn ibm_i(line: &str, toks: &[Tok<'_>], _now: i64) -> Option<DirEntry> {
    if toks.len() < 6 {
        return None;
    }
    let size = uint(toks[1].s)?;
    let date = toks[2].s;
    if !date.contains(['/', '.']) {
        return None;
    }
    let (y, mo, d) = numeric_date(date)?;
    let t = time_of_day(toks[3].s)?;
    let otype = toks[4].s;
    if !otype.starts_with('*') || otype.len() < 2 {
        return None;
    }
    let full = rest(line, toks, 5)?;
    let mut e = DirEntry {
        owner_group: toks[0].s.to_string(),
        size,
        time: make_dt(y, mo, d, Some(t))?,
        ..Default::default()
    };
    if let Some(n) = full.strip_suffix('/') {
        e.flags = FLAG_DIR;
        e.name = n.to_string();
    } else {
        if otype.eq_ignore_ascii_case("*DIR") {
            e.flags = FLAG_DIR;
        }
        e.name = full.to_string();
    }
    Some(e)
}

// ---------------------------------------------------------------- OS/2, size first

fn size_first(line: &str, toks: &[Tok<'_>], now_ms: i64) -> Option<DirEntry> {
    let size = uint(toks.first()?.s)?;
    // OS/2: size [attributes...] MM-DD-YY HH:MM name
    for k in 1..toks.len().min(6) {
        let tok = toks[k].s;
        if let Some((y, mo, d)) = numeric_date(tok) {
            let attrs = &toks[1..k];
            if !attrs
                .iter()
                .all(|a| a.s.len() <= 5 && a.s.chars().all(|c| c.is_ascii_alphabetic()))
            {
                return None;
            }
            let t = toks.get(k + 1).and_then(|t| time_of_day(t.s))?;
            let name = rest(line, toks, k + 2)?;
            let dir = attrs.iter().any(|a| a.s.eq_ignore_ascii_case("DIR"));
            return Some(DirEntry {
                name: name.to_string(),
                size: if dir { -1 } else { size },
                time: make_dt(y, mo, d, Some(t))?,
                flags: if dir { FLAG_DIR } else { 0 },
                ..Default::default()
            });
        }
        if !tok.chars().all(|c| c.is_ascii_alphabetic()) {
            break;
        }
    }
    // VxWorks: size Mon-DD-YYYY HH:MM:SS name [<DIR>]
    if let Some((y, mo, d)) = toks.get(1).and_then(|t| mon_day_year(t.s)) {
        let t = toks.get(2).and_then(|t| time_of_day(t.s))?;
        let mut last = toks.len() - 1;
        let dir = last > 3 && toks[last].s.eq_ignore_ascii_case("<DIR>");
        if dir {
            last -= 1;
        }
        if last < 3 {
            return None;
        }
        return Some(DirEntry {
            name: line.get(toks[3].start..toks[last].end())?.to_string(),
            size: if dir { -1 } else { size },
            time: make_dt(y, mo, d, Some(t))?,
            flags: if dir { FLAG_DIR } else { 0 },
            ..Default::default()
        });
    }
    // size Mon DD[,] YYYY HH:MM name (and the other ls-style date forms);
    // a trailing '/' marks a directory.
    let (time, next) = unix_date(toks, 1, now_ms)?;
    let full = rest(line, toks, next)?;
    let (name, dir) = match full.strip_suffix('/') {
        Some(n) if !n.is_empty() => (n, true),
        _ => (full, false),
    };
    Some(DirEntry {
        name: name.to_string(),
        size: if dir { -1 } else { size },
        time,
        flags: if dir { FLAG_DIR } else { 0 },
        ..Default::default()
    })
}

// ---------------------------------------------------------------- Nortel WfFtp

/// `name size MM/DD/YY weekday. HH:MM:SS`
fn nortel(_line: &str, toks: &[Tok<'_>], _now: i64) -> Option<DirEntry> {
    if toks.len() != 5 || !toks[2].s.contains('/') {
        return None;
    }
    let size = uint(toks[1].s)?;
    let (y, mo, d) = numeric_date(toks[2].s)?;
    let weekday = toks[3].s.trim_end_matches('.').to_ascii_lowercase();
    const DAYS: [&str; 10] = [
        "mon", "tue", "tues", "wed", "thu", "thur", "thurs", "fri", "sat", "sun",
    ];
    if !DAYS.contains(&weekday.as_str()) {
        return None;
    }
    let t = time_of_day(toks[4].s)?;
    Some(DirEntry {
        name: toks[0].s.to_string(),
        size,
        time: make_dt(y, mo, d, Some(t))?,
        ..Default::default()
    })
}

// ---------------------------------------------------------------- MVS

fn mvs_dataset(line: &str, toks: &[Tok<'_>], _now: i64) -> Option<DirEntry> {
    // VSAM clusters: `volume unit VSAM dsname`, no date.
    if toks.len() >= 4 && toks[2].s.eq_ignore_ascii_case("VSAM") {
        return Some(DirEntry {
            name: rest(line, toks, 3)?.to_string(),
            ..Default::default()
        });
    }
    if toks.len() < 9 {
        return None;
    }
    let referred = toks[2].s;
    let time = if referred == "**NONE**" {
        DateTime::empty()
    } else {
        let (y, mo, d) = ymd_date(referred, '/')?;
        make_dt(y, mo, d, None)?
    };
    let alpha = |s: &str| s.chars().all(|c| c.is_ascii_alphabetic());
    // `Used` may be unknown (`????`, `++++`); Ext and Used may run together.
    let used =
        |s: &str| all_digits(s) || s.chars().all(|c| c == '?') || s.chars().all(|c| c == '+');
    let k = if all_digits(toks[3].s) && used(toks[4].s) && alpha(toks[5].s) {
        5
    } else if used(toks[3].s) && alpha(toks[4].s) {
        4
    } else {
        return None;
    };
    if toks.len() <= k + 4 || !all_digits(toks[k + 1].s) || !all_digits(toks[k + 2].s) {
        return None;
    }
    let dsorg = toks[k + 3].s;
    if !dsorg.chars().all(|c| c.is_ascii_alphabetic() || c == '-') {
        return None;
    }
    let dir = dsorg.eq_ignore_ascii_case("PO") || dsorg.eq_ignore_ascii_case("PO-E");
    Some(DirEntry {
        name: rest(line, toks, k + 4)?.to_string(),
        time,
        flags: if dir { FLAG_DIR } else { 0 },
        ..Default::default()
    })
}

fn mvs_member(_line: &str, toks: &[Tok<'_>], _now: i64) -> Option<DirEntry> {
    if toks.len() < 8 {
        return None;
    }
    let vvmm = toks[1].s;
    let (vv, mm) = vvmm.split_once('.')?;
    if !all_digits(vv) || !all_digits(mm) {
        return None;
    }
    ymd_date(toks[2].s, '/')?;
    let (y, mo, d) = ymd_date(toks[3].s, '/')?;
    let t = time_of_day(toks[4].s)?;
    let size = uint(toks[5].s)?;
    if !all_digits(toks[6].s) || !all_digits(toks[7].s) {
        return None;
    }
    Some(DirEntry {
        name: toks[0].s.to_string(),
        size,
        time: make_dt(y, mo, d, Some(t))?,
        ..Default::default()
    })
}

fn mvs_only(line: &str, toks: &[Tok<'_>]) -> Option<DirEntry> {
    let name = if toks.len() >= 2 && toks[0].s.eq_ignore_ascii_case("Migrated") {
        rest(line, toks, 1)?
    } else if toks.len() >= 3 && toks[1].s.eq_ignore_ascii_case("Tape") {
        rest(line, toks, 2)?
    } else if toks.len() == 1 {
        // PDS member list with names only.
        toks[0].s
    } else {
        return load_module(toks);
    };
    Some(DirEntry {
        name: name.to_string(),
        ..Default::default()
    })
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.len() <= 15 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Load-module PDS member: `name size ttr alias attributes... amode rmode`
/// with hexadecimal size/ttr/alias; amode and rmode are numeric or `ANY`.
fn load_module(toks: &[Tok<'_>]) -> Option<DirEntry> {
    let n = toks.len();
    if n < 6 || !is_hex(toks[1].s) || !is_hex(toks[2].s) || !is_hex(toks[3].s) {
        return None;
    }
    let mode = |s: &str| all_digits(s) || s.eq_ignore_ascii_case("ANY");
    if !mode(toks[n - 1].s) || !mode(toks[n - 2].s) {
        return None;
    }
    if !toks[4..n - 2]
        .iter()
        .all(|t| t.s.chars().all(|c| c.is_ascii_alphanumeric()))
    {
        return None;
    }
    Some(DirEntry {
        name: toks[0].s.to_string(),
        size: i64::from_str_radix(toks[1].s, 16).ok()?,
        ..Default::default()
    })
}

// ---------------------------------------------------------------- z/VM

fn zvm(toks: &[Tok<'_>]) -> Option<DirEntry> {
    if toks.len() < 8 {
        return None;
    }
    let fmt = toks[2].s;
    if !(fmt.eq_ignore_ascii_case("F") || fmt.eq_ignore_ascii_case("V")) {
        return None;
    }
    let lrecl = uint(toks[3].s)?;
    let records = uint(toks[4].s)?;
    uint(toks[5].s)?;
    let (y, mo, d) = numeric_date(toks[6].s)?;
    let t = time_of_day(toks[7].s)?;
    Some(DirEntry {
        name: format!("{}.{}", toks[0].s, toks[1].s),
        size: lrecl.checked_mul(records)?,
        time: make_dt(y, mo, d, Some(t))?,
        owner_group: toks.get(8).map(|t| t.s.to_string()).unwrap_or_default(),
        ..Default::default()
    })
}

// ---------------------------------------------------------------- HP NonStop

fn hp_nonstop(line: &str, toks: &[Tok<'_>]) -> Option<DirEntry> {
    if toks.len() < 6 {
        return None;
    }
    uint(toks[1].s)?;
    let size = uint(toks[2].s)?;
    let (y, mo, d) = day_mon_year(toks[3].s)?;
    let t = time_of_day(toks[4].s)?;
    let o = toks[5].s;
    let (owner, next) = if o.ends_with(',') && toks.get(6).is_some_and(|t| all_digits(t.s)) {
        (line.get(toks[5].start..toks[6].end())?, 7)
    } else if o.contains(',') {
        (o, 6)
    } else {
        return None;
    };
    Some(DirEntry {
        name: toks[0].s.to_string(),
        size,
        time: make_dt(y, mo, d, Some(t))?,
        owner_group: owner.to_string(),
        permissions: rest(line, toks, next).unwrap_or("").to_string(),
        ..Default::default()
    })
}

// ---------------------------------------------------------------- OS-9

fn os9(line: &str, toks: &[Tok<'_>], _now: i64) -> Option<DirEntry> {
    if toks.len() < 7 {
        return None;
    }
    let (g, u) = toks[0].s.split_once('.')?;
    if !all_digits(g) || !all_digits(u) {
        return None;
    }
    let mut parts = toks[1].s.split('/');
    let (yy, mm, dd) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || yy.len() != 2 || mm.len() != 2 || dd.len() != 2 {
        return None;
    }
    let y = expand_year(uint(yy)?, 2);
    let (mo, d) = (uint(mm)? as u32, uint(dd)? as u32);
    if toks[2].s.len() != 4 || !all_digits(toks[2].s) {
        return None;
    }
    let attrs = toks[3].s;
    if !toks[4].s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let size = uint(toks[5].s)?;
    Some(DirEntry {
        name: rest(line, toks, 6)?.to_string(),
        size,
        permissions: attrs.to_string(),
        owner_group: toks[0].s.to_string(),
        time: make_dt(y, mo, d, None)?,
        flags: if attrs.starts_with('d') { FLAG_DIR } else { 0 },
        ..Default::default()
    })
}
