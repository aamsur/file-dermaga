//! FileDermaga's own site list format.
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <file-dermaga-sites version="1">
//!   <site>
//!     <name>Office NAS</name>
//!     <protocol>sftp</protocol>
//!     <host>nas.example.org</host>
//!     <port>2222</port>
//!     <user>backup</user>
//!     <password>only when exported with passwords</password>
//!     <remote-dir>/volume1/backup</remote-dir>
//!   </site>
//! </file-dermaga-sites>
//! ```
//!
//! One element per setting; settings that are empty or off are left out.
//! Unknown elements are ignored, so newer files still load in older versions.

use super::{Imported, Node, Skipped, XmlOut};
use crate::{new_site_id, Site};

pub(super) const ROOT: &str = "file-dermaga-sites";
const VERSION: u32 = 1;

/// Protocols the site list understands.
const PROTOCOLS: &[&str] = &["ftp", "ftpes", "ftps", "ftp-plain", "sftp", "dav", "davs", "s3"];

pub(super) fn read(root: &Node) -> Result<Imported, String> {
    let mut imp = Imported::default();
    if let Some(v) = root.attr("version").and_then(|v| v.parse::<u32>().ok()) {
        if v > VERSION {
            imp.notes.push(format!("The file was written by a newer FileDermaga (format version {v}); settings this version does not know were ignored."));
        }
    }
    for (i, n) in root.children.iter().filter(|c| c.name == "site").enumerate() {
        let text = |name: &str| n.text_of(name).unwrap_or("").to_owned();
        let flag = |name: &str| matches!(n.text_of(name), Some("true" | "1" | "yes"));
        let name = n.text_of("name").map(str::to_owned).unwrap_or_else(|| format!("Site {}", i + 1));
        let protocol = text("protocol").to_ascii_lowercase();
        let protocol = if protocol.is_empty() { "ftp".to_owned() } else { protocol };
        if !PROTOCOLS.contains(&protocol.as_str()) {
            imp.skipped.push(Skipped { name, reason: format!("unknown protocol \"{protocol}\"") });
            continue;
        }
        let port = match n.text_of("port") {
            None => None,
            Some(p) => match p.parse::<u16>() {
                Ok(p) if p > 0 => Some(p),
                _ => {
                    imp.skipped.push(Skipped { name, reason: format!("invalid port \"{p}\"") });
                    continue;
                }
            },
        };
        let secret = |name: &str| n.raw_of(name).unwrap_or("").to_owned();
        let password = secret("password");
        let secret_key = secret("s3-secret-key");
        let key_passphrase = secret("key-passphrase");
        imp.sites.push(Site {
            id: new_site_id(),
            save_password: !(password.is_empty() && secret_key.is_empty() && key_passphrase.is_empty()) || !flag("ask-password"),
            name,
            protocol,
            host: text("host"),
            port,
            user: text("user"),
            password,
            insecure: flag("tls-accept-invalid"),
            remote_dir: text("remote-dir"),
            local_dir: text("local-dir"),
            key_file: text("key-file"),
            key_passphrase,
            base_path: text("webdav-base-path"),
            endpoint: text("s3-endpoint"),
            region: text("s3-region"),
            profile: text("s3-profile"),
            access_key: text("s3-access-key"),
            secret_key,
            path_style: flag("s3-path-style"),
            storage_class: text("s3-storage-class"),
            sse: text("s3-sse"),
            ..Default::default()
        });
    }
    Ok(imp)
}

pub(super) fn write(sites: &[Site]) -> String {
    let mut x = XmlOut::new(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    let version = VERSION.to_string();
    x.open(ROOT, &[("version", &version)]);
    if sites.iter().any(|s| !(s.password.is_empty() && s.secret_key.is_empty() && s.key_passphrase.is_empty())) {
        x.comment("This file contains passwords in plain text. Keep it private.");
    }
    for s in sites {
        x.open("site", &[]);
        let mut put = |name: &str, value: &str| {
            if !value.is_empty() {
                x.leaf(name, &[], value);
            }
        };
        put("name", &s.name);
        put("protocol", if s.protocol.is_empty() { "ftp" } else { &s.protocol });
        put("host", &s.host);
        put("port", &s.port.map(|p| p.to_string()).unwrap_or_default());
        put("user", &s.user);
        put("password", &s.password);
        put("ask-password", if s.save_password { "" } else { "true" });
        put("tls-accept-invalid", if s.insecure { "true" } else { "" });
        put("remote-dir", &s.remote_dir);
        put("local-dir", &s.local_dir);
        put("key-file", &s.key_file);
        put("key-passphrase", &s.key_passphrase);
        put("webdav-base-path", &s.base_path);
        put("s3-endpoint", &s.endpoint);
        put("s3-region", &s.region);
        put("s3-profile", &s.profile);
        put("s3-access-key", &s.access_key);
        put("s3-secret-key", &s.secret_key);
        put("s3-path-style", if s.path_style { "true" } else { "" });
        put("s3-storage-class", &s.storage_class);
        put("s3-sse", &s.sse);
        x.close("site");
    }
    x.close(ROOT);
    x.buf
}

#[cfg(test)]
mod tests {
    use super::super::import_str;
    use super::*;

    fn sample() -> Vec<Site> {
        vec![
            Site {
                id: "abc".into(),
                name: "Office NAS <main> & co".into(),
                protocol: "sftp".into(),
                host: "nas.example.org".into(),
                port: Some(2222),
                user: "backup".into(),
                password: " p<a>ss & \"word' ".into(),
                remote_dir: "/volume1/backup".into(),
                key_file: "~/.ssh/id_ed25519".into(),
                save_password: true,
                ..Default::default()
            },
            Site {
                name: "Bucket".into(),
                protocol: "s3".into(),
                endpoint: "https://s3.example.net".into(),
                region: "eu-central-1".into(),
                access_key: "AKIAEXAMPLE".into(),
                secret_key: "s3cr3t/+=".into(),
                path_style: true,
                save_password: true,
                ..Default::default()
            },
            Site { name: "Docs".into(), protocol: "davs".into(), host: "dav.example.com".into(), base_path: "/remote.php/dav/files/me".into(), insecure: true, ..Default::default() },
        ]
    }

    #[test]
    fn round_trip_keeps_every_setting() {
        let xml = write(&sample());
        assert!(xml.contains("<file-dermaga-sites version=\"1\">"));
        assert!(xml.contains("plain text"));
        let (format, imp) = import_str(&xml).unwrap();
        assert_eq!(format, "native");
        assert!(imp.skipped.is_empty() && imp.notes.is_empty());
        let [a, b, c] = &imp.sites[..] else { panic!("3 sites expected") };
        assert_eq!((a.name.as_str(), a.protocol.as_str(), a.host.as_str(), a.port), ("Office NAS <main> & co", "sftp", "nas.example.org", Some(2222)));
        assert_eq!((a.user.as_str(), a.password.as_str(), a.remote_dir.as_str(), a.key_file.as_str()), ("backup", " p<a>ss & \"word' ", "/volume1/backup", "~/.ssh/id_ed25519"));
        assert_ne!(a.id, "abc", "imported sites get fresh ids");
        assert!(a.save_password);
        assert_eq!((b.endpoint.as_str(), b.region.as_str(), b.access_key.as_str(), b.secret_key.as_str()), ("https://s3.example.net", "eu-central-1", "AKIAEXAMPLE", "s3cr3t/+="));
        assert!(b.path_style);
        assert_eq!((c.protocol.as_str(), c.base_path.as_str()), ("davs", "/remote.php/dav/files/me"));
        assert!(c.insecure && !c.save_password, "ask-password survives the round trip");
    }

    #[test]
    fn without_passwords_no_secret_elements() {
        let mut sites = sample();
        for s in &mut sites {
            s.password.clear();
            s.secret_key.clear();
        }
        let xml = write(&sites);
        assert!(!xml.contains("<password>") && !xml.contains("s3-secret-key") && !xml.contains("plain text"));
    }

    #[test]
    fn bad_entries_are_reported_not_fatal() {
        let xml = r#"<file-dermaga-sites version="7">
            <site><name>ok</name><host>h</host></site>
            <site><name>odd</name><protocol>gopher</protocol></site>
            <site><name>port</name><port>99999</port></site>
            <site><host>noname</host><future-thing>x</future-thing></site>
        </file-dermaga-sites>"#;
        let (_, imp) = import_str(xml).unwrap();
        assert_eq!(imp.sites.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["ok", "Site 4"]);
        assert_eq!(imp.sites[0].protocol, "ftp");
        assert_eq!(imp.skipped.len(), 2);
        assert_eq!(imp.notes.len(), 1, "newer format version is mentioned");
    }
}
