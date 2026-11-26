# file-dermaga-core — specification

`file-dermaga-core` is the foundation crate of FileDermaga, a Rust FTP/FTPS/S3 client.
It provides:

1. a timestamp type that remembers its precision (`DateTime`),
2. directory entry / listing types (`DirEntry`, `DirectoryListing`),
3. server description types (`ServerType`, `Protocol`, `Server`),
4. a parser that turns the raw bytes of an FTP `LIST`/`MLSD`/`NLST` data
   connection into `DirEntry` values (`ListingParser`).

Other crates (FTP client, S3 backend, CLI, desktop GUI) already depend on the
public API in section 1, so names and signatures there are **fixed**. Everything
else (internal structure, algorithms, helper types) is up to you.

The crate must only use `std` (no dependencies), edition 2021, and must never
panic on any input.

---

## 1. Public API (fixed)

```rust
// lib.rs re-exports:
pub use datetime::{Accuracy, DateTime, Tm};
pub use direntry::{DirEntry, DirectoryListing, FLAG_DIR, FLAG_LINK, FLAG_UNSURE};
pub use listing_parser::{parse_listing, ListingParser};
pub use server::{PasvMode, Protocol, Server, ServerType};
```

Modules `datetime`, `direntry`, `listing_parser`, `server` must be `pub mod`.

### 1.1 `datetime`

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Accuracy { Days, Hours, Minutes, Seconds, Milliseconds } // ordered low → high

#[derive(Clone, Copy, PartialEq, Eq, Hash)]   // plus Default (= empty), Debug, Display
pub struct DateTime { /* private */ }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tm { pub year: i64, pub month: u32, pub day: u32, pub hour: u32,
                pub minute: u32, pub second: u32, pub millisecond: u32 }

impl DateTime {
    pub const fn empty() -> Self;
    pub fn is_empty(&self) -> bool;
    pub fn clear(&mut self);
    pub fn accuracy(&self) -> Accuracy;
    /// Milliseconds since 1970-01-01T00:00:00Z; None when empty.
    pub fn unix_millis(&self) -> Option<i64>;
    pub fn now() -> Self;                                       // Milliseconds accuracy
    pub fn from_unix_secs(secs: i64, acc: Accuracy) -> Self;
    pub fn from_unix_millis(ms: i64, acc: Accuracy) -> Self;
    /// Interpreted as UTC. `None` fields define the accuracy: no hour → Days,
    /// hour only → Hours, hour+minute → Minutes, all three → Seconds.
    /// Returns None for invalid dates/times (e.g. month 13, Feb 30, minute 60).
    pub fn from_utc(year: i64, month: i64, day: i64,
                    hour: Option<i64>, minute: Option<i64>, second: Option<i64>) -> Option<Self>;
    pub fn ymd(year: i64, month: i64, day: i64) -> Self;          // empty if invalid
    pub fn ymd_hm(year: i64, month: i64, day: i64, h: i64, mi: i64) -> Self;
    pub fn ymd_hms(year: i64, month: i64, day: i64, h: i64, mi: i64, s: i64) -> Self;
    /// `YYYYMMDD`, `YYYYMMDDHH`, `YYYYMMDDHHMM`, `YYYYMMDDHHMMSS`, optionally
    /// followed by `.` and 1–3 fraction digits (→ Milliseconds). UTC.
    /// Used for MLSD `modify=` and FTP `MDTM` replies. None if malformed.
    pub fn parse_compact(s: &str) -> Option<Self>;
    pub fn to_tm(&self) -> Option<Tm>;                           // UTC breakdown
    /// Compare at the lower of the two accuracies (2024-03-14 at Days accuracy
    /// equals 2024-03-14 09:15 at Minutes accuracy). Empty sorts first.
    pub fn compare(&self, other: &Self) -> std::cmp::Ordering;
    /// Shift by a duration; no-op when empty.
    pub fn add_millis(&mut self, ms: i64);
}
```

`Display`: `YYYY-MM-DD` (Days), `YYYY-MM-DD HHh` (Hours), `YYYY-MM-DD HH:MM`
(Minutes), `YYYY-MM-DD HH:MM:SS` (Seconds), `YYYY-MM-DD HH:MM:SS.mmm`
(Milliseconds), `<none>` when empty. `Debug` may print the same.

Derived `PartialEq` compares instant **and** accuracy. Dates must work for
years before 1970 and far in the future (proleptic Gregorian calendar).

### 1.2 `direntry`

```rust
pub const FLAG_DIR: u32 = 1;
pub const FLAG_LINK: u32 = 2;
pub const FLAG_UNSURE: u32 = 4;

#[derive(Clone, Debug, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub size: i64,               // -1 = unknown
    pub permissions: String,     // "" = unknown
    pub owner_group: String,     // "owner group", "owner", or ""
    pub target: Option<String>,  // symlink target if known
    pub time: DateTime,          // empty = unknown
    pub flags: u32,
}
impl Default for DirEntry { /* size = -1, everything else empty/0 */ }
impl DirEntry {
    pub fn is_dir(&self) -> bool; pub fn is_link(&self) -> bool; pub fn is_unsure(&self) -> bool;
    pub fn has_date(&self) -> bool;     // time not empty
    pub fn has_time(&self) -> bool;     // accuracy >= Hours
    pub fn has_seconds(&self) -> bool;  // accuracy >= Seconds
    pub fn dump(&self) -> String;       // one-line human readable, for test messages
}

#[derive(Clone, Debug, Default)]
pub struct DirectoryListing { pub path: String, pub entries: Vec<DirEntry>, pub failed: bool }
impl DirectoryListing { pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool; }
```

### 1.3 `server`

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ServerType { #[default] Unix, Vms, Dos, Mvs, VxWorks, Zvm, HpNonstop, DosVirtual, Cygwin, DosFwdSlashes }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PasvMode { #[default] Default, Active, Passive }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol { Ftp, Ftpes, Ftps, InsecureFtp, Sftp, S3, Webdav, InsecureWebdav, Http, Https }
// Ftp = plain FTP that upgrades with AUTH TLS when offered; Ftpes = explicit TLS required;
// Ftps = implicit TLS; InsecureFtp = never TLS.
impl Protocol {
    pub fn default_port(self) -> u16;   // 21 for FTP variants except Ftps=990, Sftp=22, Http/InsecureWebdav=80, others 443
    pub fn prefix(self) -> &'static str; // URL scheme: "ftp","ftpes","ftps","ftp","sftp","s3","davs","dav","http","https"
    pub fn is_ftp_family(self) -> bool;
}
impl std::str::FromStr for Protocol { type Err = String; } // parses the prefixes above (case-insensitive), "webdav" = Webdav
impl std::fmt::Display for Protocol {}                    // prints prefix()

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub protocol: Protocol, pub server_type: ServerType, pub host: String, pub port: u16,
    pub user: String, pub timezone_offset: i32 /* minutes */, pub pasv_mode: PasvMode,
    pub extra: Vec<(String, String)>,
}
impl Server {
    pub fn new(protocol: Protocol, host: impl Into<String>, port: Option<u16>) -> Self; // None → default port
    pub fn extra(&self, key: &str) -> Option<&str>;
    pub fn set_extra(&mut self, key: &str, value: impl Into<String>);
}
```

### 1.4 `listing_parser`

```rust
pub struct ListingParser { /* private */ }
impl ListingParser {
    pub fn new(server_type: ServerType) -> Self;
    pub fn set_server_type(&mut self, t: ServerType);        // affects lines fed afterwards
    pub fn set_server_timezone_offset(&mut self, minutes: i32);
    pub fn set_limit(&mut self, max_entries: usize);         // default: unlimited
    pub fn truncated(&self) -> bool;                         // true if entries were dropped by the limit
    /// Append raw bytes from the data connection. May be called many times
    /// with arbitrary chunk boundaries (even inside a line or a UTF-8 char).
    /// Returns false once a fatal error occurred (see 2.1).
    pub fn feed(&mut self, data: &[u8]) -> bool;
    /// Finish: parse what is left and return all entries in listing order.
    /// `path` is copied into `DirectoryListing::path`.
    pub fn parse(&mut self, path: &str) -> DirectoryListing;
    pub fn error(&self) -> Option<&str>;                    // message for a failed listing
    pub fn reset(&mut self);                                 // ready for a new listing
}
pub fn parse_listing(data: &[u8], server_type: ServerType) -> DirectoryListing; // new + feed + parse("")
```

Results must not depend on how the input was split across `feed` calls.

---

## 2. Listing parser behaviour

### 2.1 Input handling

- Lines end with LF, CR, CRLF, or NUL. Empty and whitespace-only lines are ignored.
- A UTF-8 BOM at the start of a line is removed.
- Each line is decoded as UTF-8 when valid; otherwise each byte maps to the
  Unicode code point of the same value (Latin-1). Matching of non-ASCII month
  names must work with both decodings (see 2.4).
- Some mainframes send EBCDIC. Detect it from the first part of the data
  (e.g. many bytes in the EBCDIC letter/digit ranges, 0x40 — the EBCDIC
  space — being frequent, EBCDIC line ends 0x15/0x25 present and no 0x0A) and
  then translate EBCDIC (code page 037: letters, digits, space, common
  punctuation) to ASCII before parsing. Plain ASCII/UTF-8 listings must never
  be detected as EBCDIC. It is fine to wait for ~512 bytes (or the end of the
  data) before deciding.
- A single line longer than 10 000 bytes is a fatal error: `feed` returns
  false and `parse` returns a listing with `failed = true`; `error()` explains why.
- Times without an explicit zone (everything except MLSD, EPLF and numeric
  epoch formats) are stored **as if they were UTC** — i.e. the server's wall
  clock is kept unchanged. The GUI relies on this.
- `set_server_timezone_offset(m)`: add `m` minutes to every parsed time that is
  not empty (user correction for servers with wrong clocks).
- Entries named `.` and `..` are never returned.
- Entries keep the order of the listing.
- Lines that match no format are dropped silently, e.g. `total 1234` headers.

### 2.2 Results conventions

- Directories: `FLAG_DIR`, `size` usually `-1` unless the format gives one.
- Symbolic links in Unix-style listings: `FLAG_LINK | FLAG_DIR` (the target
  might be a directory; the client finds out when it tries to enter it), and
  `target` set when the line shows `name -> target`.
- Precision: date only → `Days`; `HH:MM` → `Minutes`; `HH:MM:SS` → `Seconds`;
  fractional seconds → `Milliseconds`; Unix epoch values → `Seconds`.
- 24:00 means midnight at the **end** of that day (next day 00:00).
- Two-digit years: 00–49 → 2000–2049, 50–99 → 1950–1999.
- Recent files in `ls -l` style show `Mon DD HH:MM` without a year: use the
  current year (UTC), unless that date would be more than one day in the
  future, in which case use the previous year.

### 2.3 Server type gating

Some exotic formats are ambiguous with others. Formats marked *(MVS only)*,
*(z/VM only)* or *(HP NonStop only)* are only tried when the parser's server
type is `Mvs`, `Zvm` or `HpNonstop` respectively. All other formats are always
tried, and a typical Unix listing must parse correctly with any server type.

### 2.4 Month names

Month tokens are matched case-insensitively, ignoring a trailing `.` or `,`.
Support English names and common abbreviations (`Jan`, `January`, `Sept`, …),
plus at least the usual abbreviations and full names of: German, French
(with and without accents: `févr`/`fevr`, `août`/`aout`, `déc`/`dec`),
Italian, Spanish, Portuguese, Dutch, Polish, Finnish, Hungarian, Icelandic,
Lithuanian, Swedish, Danish/Norwegian and Russian (`янв`, `фев`, …; Russian
servers may also send Windows-1251 bytes, which per 2.1 arrive as Latin-1
code points — match those too). Numeric months `1`–`12` / `01`–`12` are
accepted where a format allows a numeric month. Where a month number carries a
non-ASCII suffix (CJK `月`, Korean `월`), strip the suffix.

### 2.5 Formats

Each subsection lists inputs and the expected entry. Fields not mentioned are
default (`size -1`, empty strings, `target None`, empty time, flags 0).
`Y` means the inferred year from 2.2.

#### A. MLSD / MLST (RFC 3659)

`fact=value;fact=value; name` — facts end with `;`, then exactly one space,
then the name (which may itself start with or contain spaces; keep it exactly,
only the line terminator is removed). Fact names are case-insensitive.
Relevant facts:

- `type`: `file`; `dir`; `cdir`/`pdir` (current/parent dir: recognised but
  **not** returned); `OS.unix=slink:<target>` or `OS.unix=symlink:<target>`
  (→ `FLAG_LINK|FLAG_DIR`, `target`).
- `size`: decimal bytes; anything else makes the line invalid.
- `modify`: compact time (see `parse_compact`); invalid → line invalid.
  `create` is used only if there is no `modify`.
- `perm` and `UNIX.mode`: `permissions` = `UNIX.mode` if present, else `perm`.
- owner: first of `UNIX.ownername`, `UNIX.owner`, `UNIX.user`, `UNIX.uid`;
  group: first of `UNIX.groupname`, `UNIX.group`, `UNIX.gid`;
  `owner_group` = `"owner group"` (or whichever exists).
- Unknown facts are ignored.

| input | expected |
|---|---|
| `type=file;size=48213;modify=20240314091522;perm=adfrw;UNIX.mode=0644;UNIX.owner=alice;UNIX.group=staff; quarterly report.pdf` | name `quarterly report.pdf`, size 48213, time 2024-03-14 09:15:22 (Seconds), permissions `0644`, owner_group `alice staff` |
| `Type=dir;Modify=20231102170000.250;Perm=flcdmpe; photos` | dir `photos`, time 2023-11-02 17:00:00.250 (Milliseconds), permissions `flcdmpe` |
| `type=cdir;modify=20240101000000; /home/alice` | no entry |
| `type=OS.unix=slink:/srv/data;modify=20240505121212; data-link` | `data-link`, flags LINK\|DIR, target `/srv/data` |
| `type=file;size=10;  two spaces.txt` | name ` two spaces.txt` (leading space kept) |
| `type=file;size=12x; broken` | no entry |

#### B. Unix `ls -l` and relatives

Fields: permissions, [link count], owner, [group], size, date, name.

- Permissions token starts with one of `- d l b c p s` (`d` → dir, `l` → link).
- Link count may be missing. Group may be missing. Some Netware servers split
  the permissions into two tokens: `d [RWCEAFMS]` → permissions `d [RWCEAFMS]`.
- If the group column overflows, the group name and the size may be glued
  together (`supergroup123456` → group `supergroup`, size 123456).
- Size may use a unit suffix: `512`, `1.5M`, `33K`, `2G`, `4kB` (powers of 1024,
  fractional part allowed, truncate the result to an integer).
- Date forms (case-insensitive month names, see 2.4):
  `Mon DD HH:MM`, `Mon DD YYYY`, `DD Mon HH:MM`, `DD Mon YYYY`,
  `YYYY-MM-DD HH:MM`, `YYYY-MM-DD`, `MM-DD-YY HH:MM`, and Asian forms where
  numbers carry suffixes such as `年 月 日` or `년 월 일` in either
  month-day-year or year-month-day order.
- Name: rest of the line after the date (first non-blank char to the end,
  trailing blanks removed). For links split `name -> target`.
  A single trailing `/`, `*` or `|` (from `ls -F`) is removed.
- `owner_group`: owner and group joined by one space (or just the owner).
- Also accept the same lines without the leading permissions-only quirks, i.e.
  be tolerant, but never mis-parse the examples below.

| input | expected |
|---|---|
| `-rw-r--r--    1 alice    staff        48213 Mar 14 09:15 quarterly report.pdf` | size 48213, perms `-rw-r--r--`, owner_group `alice staff`, time Y-03-14 09:15 (Minutes) |
| `drwxr-xr-x    5 bob      users         4096 Nov  2  2021 Old Projects` | dir `Old Projects`, size 4096, time 2021-11-02 (Days) |
| `lrwxrwxrwx    1 root     root            11 Jan  7  2023 latest -> releases/v3` | `latest`, flags LINK\|DIR, target `releases/v3`, size 11 |
| `-rw-r--r--   1 carol         9876 Feb 29  2024 leap.txt` | owner_group `carol`, size 9876, 2024-02-29 |
| `-rw-r--r-- dave dev 100 Apr 01 2022 nolinks.txt` | owner_group `dave dev`, size 100, 2022-04-01 |
| `d [RWCEAFMS] admin                512 May 20 14:02 PUBLIC` | dir `PUBLIC`, perms `d [RWCEAFMS]`, owner_group `admin`, size 512, Y-05-20 14:02 |
| `-rw-r--r--   1 erik  erik   2048 20 Jun 2020 europa.dat` | 2020-06-20 (Days) |
| `-rw-r--r--   1 erik  erik   64 20 jun 08:30 nyheter.txt` | Y-06-20 08:30 |
| `-rw-rw-r--   1 fred  fred   333 2023-07-04 16:45 iso.log` | 2023-07-04 16:45 |
| `-rw-r--r--   1 gina  gina   1200 Dez 24  2019 weihnachten.txt` | 2019-12-24 |
| `-rw-r--r--   1 gina  gina   1200 15 févr. 2021 fichier.txt` | 2021-02-15 |
| `-rw-r--r--   1 hank  hank   1.5M Aug  3  2022 big.iso` | size 1572864 |
| `drwxr-xr-x   2 ivan  ivan   4096 Sep  9  2021 bin/` | dir `bin` |
| `-rwxr-xr-x   1 ivan  ivan    512 Sep  9  2021 run.sh*` | `run.sh` |
| `-rw-r--r--   1 judy  judy      5 Oct 10 24:00 midnight.txt` | Y-10-11 00:00 |
| `-rw-r--r--   1 kim   supergroup123456 Dec  1  2020 glued.bin` | owner_group `kim supergroup`, size 123456 |
| `drwxr-xr-x   2 lee   lee    4096 2022년 3월 9일 korea` | dir `korea`, 2022-03-09 |
| `-rw-r--r--   1 lee   lee     777 3月 9日 2022年 asia.txt` | 2022-03-09 |
| `-rw-r--r--   1 mia   mia     900 ноя 30  2018 otchet.txt` | 2018-11-30 |
| `-rw-r--r--   1 ned   ned      12 Jan  5 12:00 name with  double  spaces ` | name `name with  double  spaces` |
| `total 1234` | no entry |

#### C. Numeric Unix variant

`<octal mode> <uid> <gid> <size> <unix seconds> <name>` — directory when the
file-type part of the mode is directory (`40755`, `040755`).

| input | expected |
|---|---|
| `100644 501 20 48213 1710407722 numeric.txt` | size 48213, perms `100644`, owner_group `501 20`, time from epoch (Seconds) |
| `40755 501 20 4096 1700000000 numdir` | dir |

#### D. DOS / Windows IIS

`<date> <time> <DIR>|<size> <name>`. Date `MM-DD-YY`, `MM-DD-YYYY`,
`YYYY-MM-DD`, `DD.MM.YYYY` (dot = day first), also with `/`. If the first
number is > 12 it must be the day. Time `HH:MM` 24h or with `AM`/`PM`
(12AM = 00, 12PM = 12), optional seconds. Size may contain `,` or `.` thousand separators.

| input | expected |
|---|---|
| `03-14-24  09:15AM               48213 quarterly report.pdf` | 2024-03-14 09:15, size 48213 |
| `11-02-2021  05:00PM       <DIR>          Old Projects` | dir, 2021-11-02 17:00 |
| `2023-07-04  16:45            1,234,567 sizes.bin` | 2023-07-04 16:45, size 1234567 |
| `14.03.2024  21:07                  42 dotted.txt` | 2024-03-14 21:07 |
| `25/12/2022  12:30AM               7 xmas.txt` | 2022-12-25 00:30 |

#### E. EPLF (Bernstein)

`+<facts>\t<name>`; facts comma separated: `/` dir, `r` retrievable, `s<bytes>`,
`m<unix seconds>`, `up<octal perms>`, `i<id>` (ignored).

| input | expected |
|---|---|
| `+i8388621.29609,m1710407722,r,s48213,\tquarterly report.pdf` | size 48213, epoch time (Seconds) |
| `+/,m1700000000,\tphotos` | dir |
| `+r,s10,up644,\tx.txt` | size 10, permissions `644` |

#### F. OpenVMS

`NAME.EXT;VERSION  <size>  <date> [<time>]  [owner]  (permissions)`.
Size is in 512-byte blocks, either `used` or `used/allocated`. Date
`DD-MON-YYYY`, time `HH:MM[:SS[.cc]]`. Directories are `NAME.DIR;version` →
dir named `NAME` (drop `.DIR` and `;1`; keep other versions). `^` escapes the
next character in names (`A^.B.TXT;1` → `A.B.TXT;1`). Owner in `[...]`
(may contain a comma, keep as is), permissions in `(...)`.
Long names often wrap: the name is alone on one line and the rest of the
entry follows on the next line — such pairs must yield one entry.

| input | expected |
|---|---|
| `REPORT.TXT;3          12/16       14-MAR-2024 09:15:22  [ALICE]  (RWED,RWED,RE,)` | name `REPORT.TXT;3`, size 6144, 2024-03-14 09:15:22, owner_group `ALICE`, perms `RWED,RWED,RE,` |
| `ARCHIVE.DIR;1          1/3         2-NOV-2021 17:00:00  [SYSTEM,ALICE]  (RWE,RWE,RE,E)` | dir `ARCHIVE`, owner_group `SYSTEM,ALICE` |
| two lines: `A_VERY_LONG_FILE_NAME_THAT_WRAPS.DAT;12` / `                     7/8     5-JAN-2023 08:00:00  [BOB]  (RWED,RWED,,)` | one entry `A_VERY_LONG_FILE_NAME_THAT_WRAPS.DAT;12`, size 3584 |
| `FILE^.WITH^.DOTS.TXT;1   1/1  1-JAN-2020 00:00:00 [X] (RWED,,,)` | name `FILE.WITH.DOTS.TXT;1` |

#### G. IBM i (AS/400)

`OWNER  <size>  MM/DD/YY  HH:MM:SS  <object type>  <name>`; names ending in `/`
are directories (strip the `/`).

| input | expected |
|---|---|
| `ALICE      48213 03/14/24 09:15:22 *STMF      report.pdf` | owner_group `ALICE`, size 48213, 2024-03-14 09:15:22 |
| `QSYS        8192 11/02/21 17:00:00 *DIR       data/` | dir `data` |

#### H. OS/2 and size-first formats

| input | expected |
|---|---|
| `     48213           A          03-14-24   09:15  REPORT.PDF` | size 48213, 2024-03-14 09:15 |
| `         0           DIR        11-02-21   17:00  PROJECTS` | dir `PROJECTS` |
| `   48213    Mar 14 2024 09:15   report.pdf` | size 48213, 2024-03-14 09:15 |

#### I. IBM MVS / z/OS

Dataset list lines (always tried):
`Volume Unit Referred Ext Used Recfm Lrecl BlkSz Dsorg Dsname` — `Dsorg` `PO`
or `PO-E` → directory (partitioned dataset), otherwise file (size unknown, -1).
Date from the `Referred` column (`YYYY/MM/DD`; `**NONE**` = no date).

| input | expected |
|---|---|
| `WRK001 3390   2024/03/14  1   15  FB      80  27920  PS  ALICE.REPORT.DATA` | file `ALICE.REPORT.DATA`, 2024-03-14 |
| `WRK002 3390   2021/11/02  2   45  FB      80  27920  PO  ALICE.SOURCE.COBOL` | dir |
| `WRK003 3390   **NONE**    1    1  FB      80   3120  PS  ALICE.NEVER.USED` | file, no date |

Partitioned dataset member lines (always tried):
`NAME  VV.MM  <created YYYY/MM/DD>  <changed YYYY/MM/DD>  <HH:MM>  <size> <init> <mod>  <id>`

| input | expected |
|---|---|
| `PAYROLL   01.03 2021/11/02 2024/03/14 09:15   120   100     0 ALICE` | `PAYROLL`, size 120, 2024-03-14 09:15 |

*(MVS only)*: `Migrated <dsname>` → file; tape datasets `<volume> Tape <dsname>` → file.

| input | expected (server type Mvs) |
|---|---|
| `Migrated                                         ALICE.OLD.DATA` | file `ALICE.OLD.DATA` |
| `VOL001 Tape ALICE.BACKUP.TAPE` | file `ALICE.BACKUP.TAPE` |

#### J. z/VM CMS *(z/VM only)*

`NAME EXT F|V <lrecl> <records> <blocks> YYYY-MM-DD HH:MM:SS <owner>` →
name `NAME.EXT`, size = lrecl × records.

| input | expected (server type Zvm) |
|---|---|
| `PROFILE  EXEC     V         72         15          1 2024-03-14 09:15:22 ALICE` | `PROFILE.EXEC`, size 1080, 2024-03-14 09:15:22, owner_group `ALICE` |

#### K. HP NonStop (Tandem) *(HP NonStop only)*

`NAME <code> <size> DD-Mon-YY HH:MM:SS <group,user> <"security">` — the owner
may also be written `group, user` (space after the comma).

| input | expected (server type HpNonstop) |
|---|---|
| `REPORT          101       48213 14-Mar-24 09:15:22 255,101  "nunu"` | `REPORT`, size 48213, 2024-03-14 09:15:22, owner_group `255,101`, perms `"nunu"` |
| `LOGFILE           0         10 02-Nov-21 17:00:00 255, 101 "oooo"` | owner_group `255, 101` |

#### L. OS-9

`<group.user>  YY/MM/DD  <hhmm>  <attrs>  <sector>  <size>  <name>` —
directory if attrs start with `d`; date only (Days).

| input | expected |
|---|---|
| `0.0       24/03/14 0915  d-ewrewr  2A  4096  CMDS` | dir `CMDS`, 2024-03-14, owner_group `0.0`, perms `d-ewrewr`, size 4096 |

#### M. Name-only listings (NLST)

If no line of the whole listing matched any format and none of the lines
contains a space, every line is a file name (flags 0, size -1, no time).

| input (whole listing) | expected |
|---|---|
| `alpha.txt\r\nbeta.txt\r\n` | entries `alpha.txt`, `beta.txt` |
| `alpha.txt\r\nnot a name\r\n` | no entries |

### 2.6 Robustness

- Never panic, never loop forever, for any byte sequence (test with random
  bytes and with random CR/LF/space/tab/NUL inserted into the examples).
- Feeding a listing in 1-byte chunks gives the same result as one chunk.
- Mixed listings (lines of several formats in one listing) parse each line
  independently (except the wrapped-line case).

---

## 3. Tests you must write

- `tests/listing.rs` (integration tests): every example table row above, as
  individual named test cases, plus multi-line, streaming (1-byte chunks),
  limit/truncation, timezone offset, EBCDIC, BOM, Latin-1, 10 000-byte line
  failure, and randomized no-panic tests (use a small deterministic PRNG; no
  external crates).
- Unit tests for `DateTime` (calendar edge cases, pre-1970, leap years,
  `parse_compact`, `compare`, `Display`) and `Protocol`/`Server`.
- For rows with `Y` compute the expected year with the rule in 2.2 inside the
  test (the tests must pass on any date).

Everything must pass `cargo test` and `cargo clippy --all-targets` without warnings.

---

### 2.5 Addendum A — further real-world variants

Same conventions as section 2.5 (`Y` = inferred year per 2.2; unmentioned
fields default). The following earlier decisions stay unchanged: MVS dataset
size is -1; OpenVMS fractional seconds give Milliseconds accuracy; an
ambiguous lone numeric token before the size is the owner.

#### Unix `ls -l` additions

1. Numeric month and day as separate tokens (month first):
   `-rw-r--r--   1 ann  ann   812 4 17 06:05 numeric-md.txt` → Y-04-17 06:05.
2. `MM-DD` followed by a year or a time:
   `... 812 11-03 2019 md-year.txt` → 2019-11-03;
   `... 812 11-03 18:20 md-time.txt` → Y-11-03 18:20.
3. Slash and dot ISO-style dates:
   `... 812 2018/05/30 07:45 slash.txt` → 2018-05-30 07:45;
   `... 812 2017.02.14 dot.txt` → 2017-02-14 (Days, no time).
4. `DD-mon-YY`: `... 812 07-aug-98 dmy.txt` → 1998-08-07.
5. German style with dots and a comma, and a time instead of a year:
   `drwxr-xr-x 4 uwe dev 2048 3. Mär., 09:10 projekt` → dir, Y-03-03 09:10.
6. Owner/group made of up to three tokens (Windows domain accounts):
   `-rw-r--r--   1 users CORP bjones 640 Feb  2 11:00 domain.txt` →
   owner_group `users CORP bjones`, size 640, Y-02-02 11:00.
7. Lines without any date: `drwxr-xr-x 2 ops ops 1024 no-date-dir` → dir
   `no-date-dir`, size 1024, no time. Only accepted when no dated
   interpretation works.
8. Numbers carrying non-ASCII suffix characters from any legacy Asian
   encoding. The suffix bytes arrive as arbitrary Latin-1 characters when the
   line is not valid UTF-8, so "digits followed only by non-ASCII characters"
   is a number with a suffix. Forms:
   - month-day-year: `<5 + junk> <21 + junk> <2006 + junk>`
   - year month day: `2006 <5 + junk> 21`
   - month day time: `<5 + junk> 21 14:02`

   Tested with raw bytes such as `b"5\x8c\x8e"` and `b"21\x93\xfa"` in
   otherwise normal `ls -l` lines.

#### Size-first / VShell / OS/2

9. `Mon DD, YYYY HH:MM` with a comma after the day, and a trailing `/`
   marking a directory: `     0  Aug 19, 2004 16:40 tools/` → dir `tools`,
   2004-08-19 16:40.
10. OS/2 three-digit years count from 1900:
    `  5120      A    06-15-104  13:05  old-os2.txt` → 2004-06-15 13:05.

#### VxWorks (always tried)

`size Mon-DD-YYYY HH:MM:SS name [<DIR>]`

11. `   1536    Apr-09-2001  08:15:44   vx-dir   <DIR>` → dir `vx-dir`,
    2001-04-09 08:15:44. Without `<DIR>` it is a file.

#### Nortel WfFtp (always tried)

`name size MM/DD/YY weekday-abbrev. HH:MM:SS`

12. `router.cfg      20480  02/17/05  Thur.   16:44:10` → `router.cfg`,
    size 20480, 2005-02-17 16:44:10.

#### OpenVMS additions

13. No time, owner without brackets:
    `NOTES.TXT;2    3/4    9-JUL-2011    OPSUSER   (RWED,RWED,RE,)` →
    size 1536, 2011-07-09 (Days), owner_group `OPSUSER`.
14. Owner before size:
    `PLAN.DOC;1   [ADMIN]   2/3   12-OCT-2015 08:30  (RWE,RWE,RE,)` →
    size 1024, 2015-10-12 08:30, owner_group `ADMIN`.
15. Date and time before size:
    `OLDLOG.LOG;5      3-FEB-1997 11:12:13     40/42     (RWED,RWED,R,)` →
    size 20480, 1997-02-03 11:12:13.
16. Size with a unit: `SMALL.DAT;1 3KB 4-APR-2009 10:00:00.15` → size 3072,
    2009-04-04 10:00:00.150.

#### IBM MVS additions

17. `Used` may be `????` or `++++` (unknown), and the Used/Ext columns may
    run together into one large number:
    `VOLA01 3390   2016/09/12  1 ????  FB      80  6160  PS  OPS.DAILY.LOG` →
    file `OPS.DAILY.LOG`, 2016-09-12;
    `VOLA02 3390   2016/09/12 812000 U 0 27998 PO OPS.LOADLIB` → dir
    `OPS.LOADLIB`.
18. VSAM clusters have no dates: `VOLA03 3390   VSAM OPS.MASTER.KSDS` → file
    `OPS.MASTER.KSDS`, no time.
19. *(MVS only)* Load-module PDS member lines:
    `name hex-size hex-ttr hex-alias attributes... amode rmode`, where the
    last two tokens are numeric or `ANY` and the size is hexadecimal.
    `PAYCALC 0001F4 00A00C  00 FO        31    ANY` → file `PAYCALC`, size 500.
    `TINYMOD 000040 000102  00 FO  RN    ANY    24` → size 64.
    A line with only a member name, such as `JCLLIB1`, gives file `JCLLIB1`
    under MVS.

#### MLSD additions

20. The separator between the facts and the name may be a tab:
    `type=file;size=99;modify=20200101120000;\tname with tab.txt` → name
    `name with tab.txt`.
21. Facts may lack the final `;` (older drafts): `type=file;size=5 legacy.txt`
    → file `legacy.txt`, size 5. This must not break lines whose name starts
    with something that looks like a fact.
22. An MLSD line with a large size and a name containing dates must still
    parse as MLSD:
    `size=987654321;type=file;modify=20210315083000; backup 2021-03-15 08-30-00.tar`
    → name `backup 2021-03-15 08-30-00.tar`, size 987654321.
