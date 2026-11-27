//! Import and export of the site list as XML files.
//!
//! Two formats, each in its own module:
//! - [`native`]: FileDermaga's own format (root element `<file-dermaga-sites>`).
//! - [`compat`]: the Site Manager XML used by another widely used FTP client, so
//!   people can bring their sites over and take them back.
//!
//! Import recognises the format from the file's structure. Secrets read from a
//! file are handed to the Site Manager, which stores them in the OS keyring on
//! save, like typed passwords.

mod compat;
mod native;

use std::path::Path;

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use serde::Serialize;

use crate::{blocking, err, secret_get, CmdResult, Site, SECRET_PASSPHRASE, SECRET_PASSWORD, SECRET_S3};

/// Site list files are small; refuse anything absurdly large.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Minimal XML tree
// ---------------------------------------------------------------------------

/// An element with its attributes, its own text and its child elements.
#[derive(Debug, Default)]
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Trimmed text of the first child element `name` (None when missing or empty).
    fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim()).filter(|t| !t.is_empty())
    }

    /// Exact text of the first child element `name`, for secrets where leading
    /// or trailing spaces are part of the value.
    fn raw_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str()).filter(|t| !t.is_empty())
    }
}

fn parse_xml(xml: &str) -> Result<Node, String> {
    // No trim_text: text around entities arrives in pieces, and trimming each
    // piece would eat the spaces in "a & b". Values are trimmed where used.
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<Node> = vec![Node::default()]; // [document, ..., current]
    let open = |e: &quick_xml::events::BytesStart| -> Result<Node, String> {
        let mut attrs = Vec::new();
        for a in e.attributes() {
            let a = a.map_err(|e| format!("invalid XML attribute: {e}"))?;
            let v = a.normalized_value(XmlVersion::Implicit1_0).map_err(|e| format!("invalid XML attribute: {e}"))?;
            attrs.push((a.key.as_ref().to_owned(), v.into_owned()));
        }
        Ok(Node { name: e.name().as_ref().to_owned(), attrs, ..Default::default() })
    };
    loop {
        let ev = reader.read_event().map_err(|e| format!("invalid XML at byte {}: {e}", reader.buffer_position()))?;
        match ev {
            Event::Start(e) => stack.push(open(&e)?),
            Event::Empty(e) => {
                let n = open(&e)?;
                stack.last_mut().expect("document node").children.push(n);
            }
            Event::Text(t) => stack.last_mut().expect("document node").text.push_str(&t.xml10_content()),
            Event::CData(c) => stack.last_mut().expect("document node").text.push_str(&c.into_inner()),
            Event::GeneralRef(r) => {
                let name = r.into_inner().into_owned();
                let s = entity(&name).ok_or_else(|| format!("unknown XML entity &{name};"))?;
                stack.last_mut().expect("document node").text.push_str(&s);
            }
            Event::End(_) => {
                let n = stack.pop().expect("open element");
                stack.last_mut().ok_or("unbalanced XML")?.children.push(n);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err("the XML file is incomplete (an element is not closed)".into());
    }
    stack.pop().unwrap().children.into_iter().next().ok_or_else(|| "the file contains no XML element".into())
}

fn entity(name: &str) -> Option<String> {
    Some(match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        _ => {
            let code = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => name.strip_prefix('#')?.parse().ok()?,
            };
            char::from_u32(code)?.to_string()
        }
    })
}

/// Indented XML writer.
struct XmlOut {
    buf: String,
    depth: usize,
}

impl XmlOut {
    fn new(prolog: &str) -> Self {
        Self { buf: format!("{prolog}\n"), depth: 0 }
    }

    fn attrs(attrs: &[(&str, &str)]) -> String {
        attrs.iter().map(|(k, v)| format!(" {k}=\"{}\"", quick_xml::escape::escape(*v))).collect()
    }

    fn open(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.buf += &format!("{}<{name}{}>\n", "  ".repeat(self.depth), Self::attrs(attrs));
        self.depth += 1;
    }

    fn close(&mut self, name: &str) {
        self.depth -= 1;
        self.buf += &format!("{}</{name}>\n", "  ".repeat(self.depth));
    }

    fn leaf(&mut self, name: &str, attrs: &[(&str, &str)], text: &str) {
        self.buf += &format!("{}<{name}{}>{}</{name}>\n", "  ".repeat(self.depth), Self::attrs(attrs), quick_xml::escape::escape(text));
    }

    fn comment(&mut self, text: &str) {
        self.buf += &format!("{}<!-- {} -->\n", "  ".repeat(self.depth), text.replace("--", "- -"));
    }
}

// ---------------------------------------------------------------------------
// Base64 (passwords in the compat format)
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace()) {
        if c == b'=' {
            break;
        }
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// A site from a file that could not be imported, and why.
#[derive(Debug, Serialize)]
struct Skipped {
    name: String,
    reason: String,
}

/// What a reader found in a file.
#[derive(Debug, Default)]
struct Imported {
    sites: Vec<Site>,
    skipped: Vec<Skipped>,
    notes: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportReport {
    /// "native" | "compat"
    format: &'static str,
    /// Sites as the Site Manager edits them, secrets included (the site list
    /// type never serialises secrets, so they are added here explicitly).
    sites: Vec<serde_json::Value>,
    skipped: Vec<Skipped>,
    notes: Vec<String>,
}

fn with_secrets(s: &Site) -> serde_json::Value {
    let mut v = serde_json::to_value(s).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        for (k, val) in [("password", &s.password), ("secretKey", &s.secret_key), ("keyPassphrase", &s.key_passphrase)] {
            if !val.is_empty() {
                o.insert(k.into(), val.clone().into());
            }
        }
    }
    v
}

fn read_file(path: &Path) -> CmdResult<String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("{} is too large for a site list", path.display()));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| "the file is not UTF-8 text".into())
}

fn import_str(xml: &str) -> CmdResult<(&'static str, Imported)> {
    let root = parse_xml(xml)?;
    if root.name == native::ROOT {
        Ok(("native", native::read(&root)?))
    } else if compat::looks_like(&root) {
        Ok(("compat", compat::read(&root)))
    } else {
        Err("This is not a site list FileDermaga can read. Supported: FileDermaga site lists and Site Manager XML exports of other FTP clients.".into())
    }
}

/// Read sites from an XML file (format detected from its content).
#[tauri::command]
pub(crate) async fn sites_import(path: String) -> CmdResult<ImportReport> {
    blocking(move || {
        let xml = read_file(Path::new(&path))?;
        let (format, imp) = import_str(&xml)?;
        Ok(ImportReport { format, sites: imp.sites.iter().map(with_secrets).collect(), skipped: imp.skipped, notes: imp.notes })
    })
    .await?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportReport {
    count: usize,
    notes: Vec<String>,
}

/// Write `sites` to `path` in `format` ("native" | "compat"). Saved secrets are
/// looked up in the keyring only when `include_passwords` is set.
#[tauri::command]
pub(crate) async fn sites_export(path: String, format: String, sites: Vec<Site>, include_passwords: bool) -> CmdResult<ExportReport> {
    blocking(move || {
        let mut sites = sites;
        for s in &mut sites {
            if include_passwords {
                let from_keyring = |kind: &str| if s.id.is_empty() { None } else { secret_get(&s.id, kind) };
                if s.password.is_empty() {
                    s.password = from_keyring(SECRET_PASSWORD).unwrap_or_default();
                }
                if s.secret_key.is_empty() {
                    s.secret_key = from_keyring(SECRET_S3).unwrap_or_default();
                }
                if s.key_passphrase.is_empty() {
                    s.key_passphrase = from_keyring(SECRET_PASSPHRASE).unwrap_or_default();
                }
            } else {
                s.password.clear();
                s.secret_key.clear();
                s.key_passphrase.clear();
            }
        }
        let (xml, notes) = match format.as_str() {
            "native" => (native::write(&sites), Vec::new()),
            "compat" => compat::write(&sites),
            other => return Err(format!("unknown export format {other:?}")),
        };
        write_file(Path::new(&path), &xml, include_passwords)?;
        Ok(ExportReport { count: sites.len(), notes })
    })
    .await?
}

/// Atomic write; a file with passwords is readable by the owner only.
fn write_file(path: &Path, data: &str, private: bool) -> CmdResult<()> {
    let tmp = path.with_extension("xml.tmp");
    std::fs::write(&tmp, data).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(err)?;
    }
    #[cfg(not(unix))]
    let _ = private;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        for s in ["", "a", "ab", "abc", "abcd", "pässwörd!", "x/y+z="] {
            assert_eq!(b64_decode(&b64_encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert_eq!(b64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(b64_decode("aGVs\nbG8=").unwrap(), b"hello");
        assert!(b64_decode("a*b").is_none());
    }

    #[test]
    fn xml_tree_and_entities() {
        let n = parse_xml(r#"<?xml version="1.0"?><a x="1 &amp; 2"><b>t &lt;3&#x21;</b><c/></a>"#).unwrap();
        assert_eq!(n.name, "a");
        assert_eq!(n.attr("x"), Some("1 & 2"));
        assert_eq!(n.text_of("b"), Some("t <3!"));
        assert!(n.child("c").is_some());
        assert!(parse_xml("<a><b></a>").is_err());
        assert!(parse_xml("not xml at all").is_err());
    }

    #[test]
    fn unknown_files_are_refused() {
        assert!(import_str("<shopping-list><item>milk</item></shopping-list>").is_err());
    }
}
