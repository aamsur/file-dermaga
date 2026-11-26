//! Server description types: protocol, server operating system family,
//! passive-mode preference and the [`Server`] record that bundles them with
//! host, port, user and free-form extra parameters.

use std::fmt;
use std::str::FromStr;

/// Operating system family of the remote server; selects listing quirks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ServerType {
    #[default]
    Unix,
    Vms,
    Dos,
    Mvs,
    VxWorks,
    Zvm,
    HpNonstop,
    DosVirtual,
    Cygwin,
    DosFwdSlashes,
}

/// Passive/active data connection preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PasvMode {
    #[default]
    Default,
    Active,
    Passive,
}

/// Transfer protocol.
///
/// `Ftp` is plain FTP that upgrades with `AUTH TLS` when offered; `Ftpes`
/// requires explicit TLS; `Ftps` is implicit TLS; `InsecureFtp` never uses
/// TLS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Ftp,
    Ftpes,
    Ftps,
    InsecureFtp,
    Sftp,
    S3,
    Webdav,
    InsecureWebdav,
    Http,
    Https,
}

impl Protocol {
    /// Well-known port for the protocol.
    pub fn default_port(self) -> u16 {
        match self {
            Protocol::Ftp | Protocol::Ftpes | Protocol::InsecureFtp => 21,
            Protocol::Ftps => 990,
            Protocol::Sftp => 22,
            Protocol::Http | Protocol::InsecureWebdav => 80,
            Protocol::S3 | Protocol::Webdav | Protocol::Https => 443,
        }
    }

    /// URL scheme used for this protocol.
    pub fn prefix(self) -> &'static str {
        match self {
            Protocol::Ftp | Protocol::InsecureFtp => "ftp",
            Protocol::Ftpes => "ftpes",
            Protocol::Ftps => "ftps",
            Protocol::Sftp => "sftp",
            Protocol::S3 => "s3",
            Protocol::Webdav => "davs",
            Protocol::InsecureWebdav => "dav",
            Protocol::Http => "http",
            Protocol::Https => "https",
        }
    }

    /// True for the FTP variants (plain, explicit/implicit TLS, insecure).
    pub fn is_ftp_family(self) -> bool {
        matches!(
            self,
            Protocol::Ftp | Protocol::Ftpes | Protocol::Ftps | Protocol::InsecureFtp
        )
    }
}

impl FromStr for Protocol {
    type Err = String;

    /// Parses a URL scheme (case-insensitive). `"ftp"` maps to
    /// [`Protocol::Ftp`]; `"webdav"` is accepted as an alias of `"davs"`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.trim().to_ascii_lowercase();
        Ok(match lower.as_str() {
            "ftp" => Protocol::Ftp,
            "ftpes" => Protocol::Ftpes,
            "ftps" => Protocol::Ftps,
            "sftp" => Protocol::Sftp,
            "s3" => Protocol::S3,
            "davs" | "webdav" => Protocol::Webdav,
            "dav" => Protocol::InsecureWebdav,
            "http" => Protocol::Http,
            "https" => Protocol::Https,
            _ => return Err(format!("unknown protocol: {s:?}")),
        })
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.prefix())
    }
}

/// A remote server and how to talk to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub protocol: Protocol,
    pub server_type: ServerType,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Correction added to server times, in minutes.
    pub timezone_offset: i32,
    pub pasv_mode: PasvMode,
    /// Free-form protocol specific parameters, in insertion order.
    pub extra: Vec<(String, String)>,
}

impl Server {
    /// A server with default settings; `None` selects the protocol's default port.
    pub fn new(protocol: Protocol, host: impl Into<String>, port: Option<u16>) -> Self {
        Server {
            protocol,
            server_type: ServerType::default(),
            host: host.into(),
            port: port.unwrap_or_else(|| protocol.default_port()),
            user: String::new(),
            timezone_offset: 0,
            pasv_mode: PasvMode::default(),
            extra: Vec::new(),
        }
    }

    /// Value of an extra parameter (exact key match).
    pub fn extra(&self, key: &str) -> Option<&str> {
        self.extra
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Sets an extra parameter, replacing an existing value for the same key.
    pub fn set_extra(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        if let Some(slot) = self.extra.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value;
        } else {
            self.extra.push((key.to_string(), value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Protocol; 10] = [
        Protocol::Ftp,
        Protocol::Ftpes,
        Protocol::Ftps,
        Protocol::InsecureFtp,
        Protocol::Sftp,
        Protocol::S3,
        Protocol::Webdav,
        Protocol::InsecureWebdav,
        Protocol::Http,
        Protocol::Https,
    ];

    #[test]
    fn default_ports() {
        let ports: Vec<u16> = ALL.iter().map(|p| p.default_port()).collect();
        assert_eq!(ports, [21, 21, 990, 21, 22, 443, 443, 80, 80, 443]);
    }

    #[test]
    fn prefixes_and_display() {
        let prefixes: Vec<&str> = ALL.iter().map(|p| p.prefix()).collect();
        assert_eq!(
            prefixes,
            ["ftp", "ftpes", "ftps", "ftp", "sftp", "s3", "davs", "dav", "http", "https"]
        );
        for p in ALL {
            assert_eq!(p.to_string(), p.prefix());
        }
    }

    #[test]
    fn ftp_family() {
        let fam: Vec<bool> = ALL.iter().map(|p| p.is_ftp_family()).collect();
        assert_eq!(
            fam,
            [true, true, true, true, false, false, false, false, false, false]
        );
    }

    #[test]
    fn from_str_roundtrip() {
        for p in ALL {
            let parsed: Protocol = p.prefix().parse().unwrap();
            if p == Protocol::InsecureFtp {
                assert_eq!(parsed, Protocol::Ftp);
            } else {
                assert_eq!(parsed, p);
            }
        }
        assert_eq!("FTPES".parse::<Protocol>(), Ok(Protocol::Ftpes));
        assert_eq!("WebDav".parse::<Protocol>(), Ok(Protocol::Webdav));
        assert_eq!("Dav".parse::<Protocol>(), Ok(Protocol::InsecureWebdav));
        assert!("gopher".parse::<Protocol>().is_err());
        assert!("".parse::<Protocol>().is_err());
    }

    #[test]
    fn server_new_and_extra() {
        let s = Server::new(Protocol::Ftps, "example.org", None);
        assert_eq!(s.port, 990);
        assert_eq!(s.host, "example.org");
        assert_eq!(s.server_type, ServerType::Unix);
        assert_eq!(s.pasv_mode, PasvMode::Default);
        assert_eq!(s.timezone_offset, 0);
        assert!(s.user.is_empty());
        let mut s = Server::new(Protocol::Sftp, String::from("h"), Some(2222));
        assert_eq!(s.port, 2222);
        assert_eq!(s.extra("region"), None);
        s.set_extra("region", "eu-west-1");
        s.set_extra("bucket", String::from("b"));
        s.set_extra("region", "us-east-1");
        assert_eq!(s.extra("region"), Some("us-east-1"));
        assert_eq!(s.extra("bucket"), Some("b"));
        assert_eq!(s.extra.len(), 2);
        assert_eq!(s.extra[0].0, "region");
        let t = s.clone();
        assert_eq!(s, t);
    }
}
