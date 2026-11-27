//! Site Manager XML of another widely used FTP client (its "Export…" file and
//! its site manager file have the same layout).
//!
//! Layout, as seen in such files:
//!
//! ```xml
//! <ROOT version="…" platform="…">
//!   <Servers>
//!     <Folder expanded="1">Customers
//!       <Server>…</Server>
//!     </Folder>
//!     <Server>
//!       <Host>files.example.org</Host> <Port>22</Port> <Protocol>1</Protocol>
//!       <Type>0</Type> <User>alice</User> <Pass encoding="base64">…</Pass>
//!       <Logontype>1</Logontype> <Name>Example</Name>
//!       <RemoteDir>1 0 4 home 5 alice</RemoteDir> <LocalDir>/home/alice</LocalDir>
//!       <Parameter Name="region">eu-west-1</Parameter>
//!     </Server>
//!   </Servers>
//! </ROOT>
//! ```
//!
//! The file is recognised by `<Servers>`/`<Server>`, not by the root element's
//! name; the name is only written on export because the other client needs it.

use super::{b64_decode, b64_encode, Imported, Node, Skipped, XmlOut};
use crate::{new_site_id, Site};

/// Root element the other client expects in files it imports.
const EXPORT_ROOT: &str = "FileZilla3";

/// Protocol codes used in the `<Protocol>` element.
mod code {
    pub const FTP: u32 = 0; // TLS when the server offers it
    pub const SFTP: u32 = 1;
    pub const FTPS_IMPLICIT: u32 = 3;
    pub const FTPS_EXPLICIT: u32 = 4;
    pub const FTP_PLAIN: u32 = 6;
    pub const S3: u32 = 7;
    pub const WEBDAV_HTTPS: u32 = 9;
    pub const GOOGLE_DRIVE: u32 = 14;
    pub const B2_NATIVE: u32 = 17;
    pub const WEBDAV_HTTP: u32 = 19;
}

/// `<Logontype>` values.
mod logon {
    pub const ANONYMOUS: u32 = 0;
    pub const NORMAL: u32 = 1;
    pub const ASK: u32 = 2;
    pub const KEY_FILE: u32 = 5;
}

pub(super) fn looks_like(root: &Node) -> bool {
    root.child("Servers").is_some()
}

pub(super) fn read(root: &Node) -> Imported {
    let mut imp = Imported::default();
    if let Some(servers) = root.child("Servers") {
        walk(servers, "", &mut imp);
    }
    if imp.sites.iter().any(|s| s.protocol == "s3" && !s.endpoint.starts_with("https://") && !s.endpoint.is_empty()) {
        imp.notes.push("S3 endpoints on a port other than 443 were set to plain http://; change them to https:// where the service uses TLS.".into());
    }
    imp
}

/// Sites inside folders keep the folder path in their name: "Customers/Example".
fn walk(node: &Node, folder: &str, imp: &mut Imported) {
    for c in &node.children {
        match c.name.as_str() {
            "Server" => server(c, folder, imp),
            "Folder" => {
                let name = c.text.trim();
                let path = match (folder.is_empty(), name.is_empty()) {
                    (_, true) => folder.to_owned(),
                    (true, false) => name.to_owned(),
                    (false, false) => format!("{folder}/{name}"),
                };
                walk(c, &path, imp);
            }
            _ => {}
        }
    }
}

fn server(n: &Node, folder: &str, imp: &mut Imported) {
    let host = n.text_of("Host").unwrap_or("").to_owned();
    let base = n.text_of("Name").map(str::to_owned).unwrap_or_else(|| if host.is_empty() { "Unnamed site".into() } else { host.clone() });
    let name = if folder.is_empty() { base } else { format!("{folder}/{base}") };
    let skip = |imp: &mut Imported, reason: String| imp.skipped.push(Skipped { name: name.clone(), reason });

    let proto_code = n.text_of("Protocol").and_then(|p| p.parse::<u32>().ok()).unwrap_or(code::FTP);
    let protocol = match proto_code {
        code::FTP => "ftp",
        code::SFTP => "sftp",
        code::FTPS_IMPLICIT => "ftps",
        code::FTPS_EXPLICIT => "ftpes",
        code::FTP_PLAIN => "ftp-plain",
        code::S3 => "s3",
        code::WEBDAV_HTTPS => "davs",
        code::WEBDAV_HTTP => "dav",
        code::GOOGLE_DRIVE => return skip(imp, "Google Drive is not supported".into()),
        code::B2_NATIVE => return skip(imp, "Backblaze B2's own API is not supported; add the bucket as S3 with endpoint https://s3.<region>.backblazeb2.com".into()),
        other => return skip(imp, format!("protocol (code {other}) is not supported")),
    };
    let port = match n.text_of("Port").map(|p| p.parse::<u16>()) {
        None => None,
        Some(Ok(p)) if p > 0 => Some(p),
        Some(_) => return skip(imp, "invalid port".into()),
    };
    let logon = n.text_of("Logontype").and_then(|l| l.parse::<u32>().ok()).unwrap_or(logon::NORMAL);
    let mut user = n.text_of("User").unwrap_or("").to_owned();

    // Password: base64 or plain text; encrypted ones cannot be read.
    let mut password = String::new();
    if let Some(p) = n.child("Pass").filter(|p| !p.text.is_empty()) {
        match p.attr("encoding") {
            None | Some("") | Some("plain") => password = p.text.clone(),
            Some("base64") => match b64_decode(p.text.trim()).and_then(|b| String::from_utf8(b).ok()) {
                Some(pw) => password = pw,
                None => imp.notes.push(format!("{name}: the saved password could not be decoded; it will be asked when connecting.")),
            },
            Some(_) => imp.notes.push(format!("{name}: the password is protected by a master password and was not imported; it will be asked when connecting.")),
        }
    }
    if logon == logon::ANONYMOUS {
        if user.is_empty() {
            user = "anonymous".into();
        }
        password.clear();
    } else if logon != logon::NORMAL && logon != 4 {
        password.clear(); // ask / interactive / key file: nothing to save
    }

    let mut site = Site {
        id: new_site_id(),
        name,
        protocol: protocol.into(),
        port,
        remote_dir: n.text_of("RemoteDir").and_then(decode_remote_dir).unwrap_or_default(),
        local_dir: n.text_of("LocalDir").unwrap_or("").to_owned(),
        save_password: logon != logon::ASK,
        ..Default::default()
    };
    if protocol == "s3" {
        // Host = endpoint host, User = access key, Pass = secret key.
        site.access_key = user;
        site.secret_key = password;
        site.region = n.children.iter().find(|c| c.name == "Parameter" && c.attr("Name") == Some("region")).map(|c| c.text.trim().to_owned()).unwrap_or_default();
        if host.is_empty() || host.ends_with("amazonaws.com") {
            site.endpoint = String::new(); // AWS itself
            if site.region.is_empty() {
                site.region = aws_region_from_host(&host).unwrap_or_default();
            }
            if site.region.is_empty() {
                imp.notes.push(format!("{}: no AWS region in the file; set it in the Site Manager if the buckets are not in us-east-1.", site.name));
            }
        } else {
            site.endpoint = match port {
                None | Some(443) => format!("https://{host}"),
                Some(p) => format!("http://{host}:{p}"),
            };
            site.path_style = true;
        }
        site.port = None;
    } else {
        site.host = host;
        site.user = user;
        site.password = password;
        if logon == logon::KEY_FILE {
            site.key_file = n.text_of("Keyfile").unwrap_or("").to_owned();
        }
    }
    imp.sites.push(site);
}

/// "s3.eu-west-1.amazonaws.com" or "s3-eu-west-1.amazonaws.com" -> "eu-west-1".
fn aws_region_from_host(host: &str) -> Option<String> {
    let first = host.strip_suffix(".amazonaws.com")?;
    let region = first.strip_prefix("s3.").or_else(|| first.strip_prefix("s3-"))?;
    let region = region.strip_prefix("dualstack.").unwrap_or(region);
    (region.split('-').count() >= 3 && region.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| region.to_owned())
}

/// `RemoteDir` holds a server type, a prefix and length-prefixed path segments:
/// "1 0 4 home 5 alice" is /home/alice. Lengths count characters, so segments
/// may contain spaces.
fn decode_remote_dir(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let number = |i: &mut usize| -> Option<usize> {
        while *i < chars.len() && chars[*i] == ' ' {
            *i += 1;
        }
        let start = *i;
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            *i += 1;
        }
        chars[start..*i].iter().collect::<String>().parse().ok()
    };
    let _server_type = number(&mut i)?;
    let prefix_len = number(&mut i)?;
    if prefix_len > 0 {
        i += 1 + prefix_len; // prefixes (e.g. VMS devices) are not used here
    }
    let mut segments = Vec::new();
    while i < chars.len() {
        let len = number(&mut i)?;
        if i >= chars.len() || chars[i] != ' ' || i + 1 + len > chars.len() {
            return None;
        }
        segments.push(chars[i + 1..i + 1 + len].iter().collect::<String>());
        i += 1 + len;
    }
    Some(format!("/{}", segments.join("/")))
}

fn encode_remote_dir(path: &str) -> String {
    let mut out = String::from("1 0");
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        out += &format!(" {} {seg}", seg.chars().count());
    }
    out
}

/// Returns the XML and notes about settings that format cannot hold.
pub(super) fn write(sites: &[Site]) -> (String, Vec<String>) {
    let mut notes = Vec::new();
    let mut x = XmlOut::new(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?>"#);
    x.open(EXPORT_ROOT, &[("version", "3"), ("platform", std::env::consts::OS)]);
    x.open("Servers", &[]);
    for s in sites {
        let protocol = s.protocol.as_str();
        let code = match protocol {
            "" | "ftp" => code::FTP,
            "sftp" => code::SFTP,
            "ftps" => code::FTPS_IMPLICIT,
            "ftpes" => code::FTPS_EXPLICIT,
            "ftp-plain" => code::FTP_PLAIN,
            "s3" => code::S3,
            "davs" => code::WEBDAV_HTTPS,
            "dav" => code::WEBDAV_HTTP,
            other => {
                notes.push(format!("{}: protocol \"{other}\" cannot be exported and was left out.", s.name));
                continue;
            }
        };
        let (host, port, user, pass) = if protocol == "s3" {
            let (h, p) = endpoint_host_port(&s.endpoint);
            if !s.profile.is_empty() && s.access_key.is_empty() {
                notes.push(format!("{}: uses AWS profile \"{}\", which that format cannot store; add the access key there.", s.name, s.profile));
            }
            (h, p, s.access_key.clone(), s.secret_key.clone())
        } else {
            let default = match protocol {
                "sftp" => 22,
                "ftps" => 990,
                "dav" => 80,
                "davs" => 443,
                _ => 21,
            };
            (s.host.clone(), s.port.unwrap_or(default), s.user.clone(), s.password.clone())
        };
        let logon = if protocol == "sftp" && !s.key_file.is_empty() {
            logon::KEY_FILE
        } else if protocol != "s3" && (user.is_empty() || user == "anonymous") {
            logon::ANONYMOUS
        } else if !pass.is_empty() {
            logon::NORMAL
        } else {
            logon::ASK
        };
        let remote_dir = if s.remote_dir.is_empty() && protocol.starts_with("dav") { &s.base_path } else { &s.remote_dir };
        if protocol.starts_with("dav") && !s.base_path.is_empty() && !s.remote_dir.is_empty() {
            notes.push(format!("{}: the WebDAV root path cannot be stored in that format; only the start directory was exported.", s.name));
        }

        x.open("Server", &[]);
        x.leaf("Host", &[], &host);
        x.leaf("Port", &[], &port.to_string());
        x.leaf("Protocol", &[], &code.to_string());
        x.leaf("Type", &[], "0");
        if logon != logon::ANONYMOUS {
            x.leaf("User", &[], &user);
        }
        if logon == logon::NORMAL {
            x.leaf("Pass", &[("encoding", "base64")], &b64_encode(pass.as_bytes()));
        }
        if logon == logon::KEY_FILE {
            x.leaf("Keyfile", &[], &s.key_file);
        }
        x.leaf("Logontype", &[], &logon.to_string());
        x.leaf("EncodingType", &[], "Auto");
        x.leaf("BypassProxy", &[], "0");
        x.leaf("Name", &[], &s.name);
        if !remote_dir.is_empty() {
            x.leaf("RemoteDir", &[], &encode_remote_dir(remote_dir));
        }
        if !s.local_dir.is_empty() {
            x.leaf("LocalDir", &[], &s.local_dir);
        }
        x.leaf("SyncBrowsing", &[], "0");
        x.leaf("DirectoryComparison", &[], "0");
        if protocol == "s3" && !s.region.is_empty() {
            x.leaf("Parameter", &[("Name", "region")], &s.region);
        }
        x.close("Server");
    }
    x.close("Servers");
    x.close(EXPORT_ROOT);
    (x.buf, notes)
}

/// "https://s3.example.net:9000/" -> ("s3.example.net", 9000); empty = AWS.
fn endpoint_host_port(endpoint: &str) -> (String, u16) {
    if endpoint.is_empty() {
        return ("s3.amazonaws.com".into(), 443);
    }
    let (scheme, rest) = endpoint.split_once("://").unwrap_or(("https", endpoint));
    let hostport = rest.split('/').next().unwrap_or(rest);
    match hostport.rsplit_once(':').and_then(|(h, p)| Some((h, p.parse::<u16>().ok()?))) {
        Some((h, p)) => (h.to_owned(), p),
        None => (hostport.to_owned(), if scheme.eq_ignore_ascii_case("http") { 80 } else { 443 }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{b64_encode, import_str};
    use super::*;

    /// Invented sample covering every case the importer handles.
    fn sample() -> String {
        let pw = b64_encode("s3cret pass".as_bytes());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?>
<SomeClient version="9.9" platform="unix">
  <Servers>
    <Server>
      <Host>files.example.org</Host><Port>2222</Port><Protocol>1</Protocol><Type>0</Type>
      <User>alice</User><Pass encoding="base64">{pw}</Pass><Logontype>1</Logontype>
      <Name>Example &amp; Co</Name><RemoteDir>1 0 4 home 8 my files</RemoteDir><LocalDir>/home/alice</LocalDir>
    </Server>
    <Folder expanded="1">Customers
      <Server><Host>ftp.example.com</Host><Port>21</Port><Protocol>4</Protocol><User>bob</User><Logontype>2</Logontype><Name>Bob</Name></Server>
      <Folder>Archive
        <Server><Host>old.example.com</Host><Protocol>6</Protocol><Logontype>0</Logontype><Name>Old</Name></Server>
      </Folder>
    </Folder>
    <Server><Host>s3.amazonaws.com</Host><Port>443</Port><Protocol>7</Protocol><User>AKIAEXAMPLE</User><Pass encoding="base64">{pw}</Pass><Logontype>1</Logontype><Name>AWS</Name><Parameter Name="region">eu-west-1</Parameter></Server>
    <Server><Host>10.0.0.5</Host><Port>8333</Port><Protocol>7</Protocol><User>key</User><Pass>plainsecret</Pass><Logontype>1</Logontype><Name>Lab S3</Name></Server>
    <Server><Host>10.0.0.6</Host><Port>7333</Port><Protocol>19</Protocol><Logontype>0</Logontype><Name>Lab DAV</Name></Server>
    <Server><Host>dav.example.com</Host><Port>443</Port><Protocol>9</Protocol><User>me</User><Pass encoding="crypt">xxxx</Pass><Logontype>1</Logontype><Name>Locked</Name></Server>
    <Server><Host>sftp.example.com</Host><Protocol>1</Protocol><User>deploy</User><Logontype>5</Logontype><Keyfile>/home/me/.ssh/deploy</Keyfile><Name>Key</Name></Server>
    <Server><Host>www.googleapis.com</Host><Port>443</Port><Protocol>14</Protocol><Logontype>3</Logontype><Name>Drive</Name></Server>
    <Server><Host>api.backblazeb2.com</Host><Port>443</Port><Protocol>17</Protocol><User>k</User><Logontype>1</Logontype><Name>B2</Name></Server>
  </Servers>
</SomeClient>"#
        )
    }

    fn by_name<'a>(imp: &'a Imported, name: &str) -> &'a Site {
        imp.sites.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("{name} missing"))
    }

    #[test]
    fn reads_every_supported_kind() {
        let (format, imp) = import_str(&sample()).unwrap();
        assert_eq!(format, "compat");
        assert_eq!(imp.sites.len(), 8);

        let a = by_name(&imp, "Example & Co");
        assert_eq!((a.protocol.as_str(), a.host.as_str(), a.port, a.user.as_str(), a.password.as_str()), ("sftp", "files.example.org", Some(2222), "alice", "s3cret pass"));
        assert_eq!((a.remote_dir.as_str(), a.local_dir.as_str()), ("/home/my files", "/home/alice"));
        assert!(a.save_password);

        let b = by_name(&imp, "Customers/Bob");
        assert_eq!((b.protocol.as_str(), b.password.as_str()), ("ftpes", ""));
        assert!(!b.save_password, "ask-for-password stays asking");

        let old = by_name(&imp, "Customers/Archive/Old");
        assert_eq!((old.protocol.as_str(), old.user.as_str()), ("ftp-plain", "anonymous"));

        let aws = by_name(&imp, "AWS");
        assert_eq!((aws.protocol.as_str(), aws.endpoint.as_str(), aws.region.as_str()), ("s3", "", "eu-west-1"));
        assert_eq!((aws.access_key.as_str(), aws.secret_key.as_str(), aws.host.as_str()), ("AKIAEXAMPLE", "s3cret pass", ""));

        let lab = by_name(&imp, "Lab S3");
        assert_eq!((lab.endpoint.as_str(), lab.secret_key.as_str()), ("http://10.0.0.5:8333", "plainsecret"));
        assert!(lab.path_style);

        let dav = by_name(&imp, "Lab DAV");
        assert_eq!((dav.protocol.as_str(), dav.port), ("dav", Some(7333)));

        let locked = by_name(&imp, "Locked");
        assert_eq!((locked.protocol.as_str(), locked.password.as_str()), ("davs", ""));

        let key = by_name(&imp, "Key");
        assert_eq!((key.key_file.as_str(), key.password.as_str()), ("/home/me/.ssh/deploy", ""));

        let skipped: Vec<_> = imp.skipped.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(skipped, ["Drive", "B2"]);
        assert!(imp.notes.iter().any(|n| n.contains("Locked") && n.contains("master password")));
        assert!(imp.notes.iter().any(|n| n.contains("http://")));
    }

    #[test]
    fn remote_dir_codec() {
        assert_eq!(decode_remote_dir("1 0 4 home 5 alice").as_deref(), Some("/home/alice"));
        assert_eq!(decode_remote_dir("1 0").as_deref(), Some("/"));
        assert_eq!(decode_remote_dir("1 0 9 a b c d e").as_deref(), Some("/a b c d e"));
        assert_eq!(decode_remote_dir("1 0 3 äöü").as_deref(), Some("/äöü"));
        assert_eq!(decode_remote_dir("1 0 50 short"), None);
        assert_eq!(decode_remote_dir("garbage"), None);
        for p in ["/", "/home/alice", "/with space/äöü/x"] {
            assert_eq!(decode_remote_dir(&encode_remote_dir(p)).as_deref(), Some(p));
        }
    }

    #[test]
    fn export_then_import_round_trip() {
        let (_, imp) = import_str(&sample()).unwrap();
        let (xml, notes) = write(&imp.sites);
        assert!(xml.contains(&format!("<{EXPORT_ROOT} ")) && xml.contains("<Servers>"));
        assert!(notes.is_empty(), "{notes:?}");
        let (format, again) = import_str(&xml).unwrap();
        assert_eq!(format, "compat");
        assert_eq!(again.sites.len(), imp.sites.len());
        for (a, b) in imp.sites.iter().zip(&again.sites) {
            assert_eq!((&a.name, &a.protocol, &a.host, &a.user, &a.password), (&b.name, &b.protocol, &b.host, &b.user, &b.password));
            assert_eq!((&a.endpoint, &a.region, &a.access_key, &a.secret_key), (&b.endpoint, &b.region, &b.access_key, &b.secret_key));
            assert_eq!((&a.remote_dir, &a.local_dir, &a.key_file), (&b.remote_dir, &b.local_dir, &b.key_file));
            if !a.password.is_empty() || !a.secret_key.is_empty() {
                assert_eq!(a.save_password, b.save_password, "{}", a.name);
            }
        }
    }

    #[test]
    fn aws_region_in_host() {
        assert_eq!(aws_region_from_host("s3.eu-west-1.amazonaws.com").as_deref(), Some("eu-west-1"));
        assert_eq!(aws_region_from_host("s3-ap-southeast-2.amazonaws.com").as_deref(), Some("ap-southeast-2"));
        assert_eq!(aws_region_from_host("s3.dualstack.us-east-2.amazonaws.com").as_deref(), Some("us-east-2"));
        assert_eq!(aws_region_from_host("s3.amazonaws.com"), None);
        assert_eq!(aws_region_from_host("example.com"), None);
    }

    #[test]
    fn endpoint_split() {
        assert_eq!(endpoint_host_port(""), ("s3.amazonaws.com".into(), 443));
        assert_eq!(endpoint_host_port("https://s3.example.net"), ("s3.example.net".into(), 443));
        assert_eq!(endpoint_host_port("http://10.0.0.5:8333/"), ("10.0.0.5".into(), 8333));
        assert_eq!(endpoint_host_port("http://minio.local"), ("minio.local".into(), 80));
    }
}
