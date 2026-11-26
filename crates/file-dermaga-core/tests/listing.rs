//! Integration tests for the listing parser: every example of the
//! specification, plus multi-line, streaming, limit, timezone, encoding,
//! failure and randomized robustness tests.

use file_dermaga_core::*;

const DAY_MS: i64 = 86_400_000;

fn entries_st(data: &[u8], st: ServerType) -> Vec<DirEntry> {
    let l = parse_listing(data, st);
    assert!(!l.failed);
    l.entries
}

fn one_st(line: &str, st: ServerType) -> DirEntry {
    let v = entries_st(format!("{line}\r\n").as_bytes(), st);
    assert_eq!(
        v.len(),
        1,
        "line {line:?} gave {:?}",
        v.iter().map(DirEntry::dump).collect::<Vec<_>>()
    );
    v.into_iter().next().unwrap()
}

fn one(line: &str) -> DirEntry {
    one_st(line, ServerType::Unix)
}

fn none_st(line: &str, st: ServerType) {
    let v = entries_st(format!("{line}\r\n").as_bytes(), st);
    assert!(
        v.is_empty(),
        "line {line:?} gave {:?}",
        v.iter().map(DirEntry::dump).collect::<Vec<_>>()
    );
}

fn none(line: &str) {
    none_st(line, ServerType::Unix)
}

/// Expected timestamp for a `Mon DD HH:MM` entry, using the year rule of
/// the specification: current UTC year unless more than a day ahead.
fn recent(mo: i64, d: i64, h: i64, mi: i64) -> DateTime {
    let now = DateTime::now();
    let now_ms = now.unix_millis().unwrap();
    let year = now.to_tm().unwrap().year;
    let build = |y: i64| {
        if h == 24 {
            let mut t = DateTime::ymd_hm(y, mo, d, 0, 0);
            t.add_millis(DAY_MS);
            t
        } else {
            DateTime::ymd_hm(y, mo, d, h, mi)
        }
    };
    let c = build(year);
    if !c.is_empty() && c.unix_millis().unwrap() <= now_ms + DAY_MS {
        c
    } else {
        build(year - 1)
    }
}

fn file(name: &str) -> DirEntry {
    DirEntry {
        name: name.into(),
        ..Default::default()
    }
}

// ===================================================================== A. MLSD

#[test]
fn mlsd_file_with_unix_facts() {
    let e = one("type=file;size=48213;modify=20240314091522;perm=adfrw;UNIX.mode=0644;UNIX.owner=alice;UNIX.group=staff; quarterly report.pdf");
    assert_eq!(
        e,
        DirEntry {
            name: "quarterly report.pdf".into(),
            size: 48213,
            permissions: "0644".into(),
            owner_group: "alice staff".into(),
            time: DateTime::ymd_hms(2024, 3, 14, 9, 15, 22),
            ..Default::default()
        }
    );
    assert_eq!(e.time.accuracy(), Accuracy::Seconds);
}

#[test]
fn mlsd_dir_mixed_case_facts_millis() {
    let e = one("Type=dir;Modify=20231102170000.250;Perm=flcdmpe; photos");
    assert_eq!(e.name, "photos");
    assert_eq!(e.flags, FLAG_DIR);
    assert_eq!(e.permissions, "flcdmpe");
    assert_eq!(e.time.accuracy(), Accuracy::Milliseconds);
    assert_eq!(e.time.to_string(), "2023-11-02 17:00:00.250");
    assert_eq!(e.size, -1);
}

#[test]
fn mlsd_cdir_not_returned() {
    none("type=cdir;modify=20240101000000; /home/alice");
    none("type=pdir;modify=20240101000000; /home");
}

#[test]
fn mlsd_symlink() {
    let e = one("type=OS.unix=slink:/srv/data;modify=20240505121212; data-link");
    assert_eq!(e.name, "data-link");
    assert_eq!(e.flags, FLAG_LINK | FLAG_DIR);
    assert_eq!(e.target.as_deref(), Some("/srv/data"));
    assert_eq!(e.time, DateTime::ymd_hms(2024, 5, 5, 12, 12, 12));
    let e = one("type=OS.unix=symlink:../x;size=3; other-link");
    assert_eq!(e.flags, FLAG_LINK | FLAG_DIR);
    assert_eq!(e.target.as_deref(), Some("../x"));
}

#[test]
fn mlsd_name_leading_space_kept() {
    let e = one("type=file;size=10;  two spaces.txt");
    assert_eq!(e.name, " two spaces.txt");
    assert_eq!(e.size, 10);
}

#[test]
fn mlsd_invalid_size_dropped() {
    none("type=file;size=12x; broken");
}

#[test]
fn mlsd_extra_rules() {
    // invalid modify invalidates the line
    none("type=file;size=1;modify=2024; bad-time");
    // create is used without modify
    let e = one("type=file;create=20200101120000;UNIX.uid=1000;UNIX.gid=100; created.txt");
    assert_eq!(e.time, DateTime::ymd_hms(2020, 1, 1, 12, 0, 0));
    assert_eq!(e.owner_group, "1000 100");
    // modify wins over create; perm used without UNIX.mode; unknown facts ignored
    let e = one(
        "create=20200101120000;modify=20210101120000;perm=r;x.unknown=1;UNIX.ownername=bob; both",
    );
    assert_eq!(e.time, DateTime::ymd_hms(2021, 1, 1, 12, 0, 0));
    assert_eq!(e.permissions, "r");
    assert_eq!(e.owner_group, "bob");
    // trailing blanks of the name are part of it
    assert_eq!(one("type=file; spaced  ").name, "spaced  ");
    none("type=file;size=1; .");
}

// ===================================================================== B. Unix

#[test]
fn unix_recent_file() {
    let e = one("-rw-r--r--    1 alice    staff        48213 Mar 14 09:15 quarterly report.pdf");
    assert_eq!(
        e,
        DirEntry {
            name: "quarterly report.pdf".into(),
            size: 48213,
            permissions: "-rw-r--r--".into(),
            owner_group: "alice staff".into(),
            time: recent(3, 14, 9, 15),
            ..Default::default()
        }
    );
    assert_eq!(e.time.accuracy(), Accuracy::Minutes);
}

#[test]
fn unix_dir_with_year() {
    let e = one("drwxr-xr-x    5 bob      users         4096 Nov  2  2021 Old Projects");
    assert_eq!(e.name, "Old Projects");
    assert_eq!(e.flags, FLAG_DIR);
    assert_eq!(e.size, 4096);
    assert_eq!(e.time, DateTime::ymd(2021, 11, 2));
    assert_eq!(e.time.accuracy(), Accuracy::Days);
    assert_eq!(e.owner_group, "bob users");
}

#[test]
fn unix_symlink() {
    let e = one("lrwxrwxrwx    1 root     root            11 Jan  7  2023 latest -> releases/v3");
    assert_eq!(e.name, "latest");
    assert_eq!(e.flags, FLAG_LINK | FLAG_DIR);
    assert_eq!(e.target.as_deref(), Some("releases/v3"));
    assert_eq!(e.size, 11);
    assert_eq!(e.time, DateTime::ymd(2023, 1, 7));
}

#[test]
fn unix_no_group() {
    let e = one("-rw-r--r--   1 carol         9876 Feb 29  2024 leap.txt");
    assert_eq!(e.owner_group, "carol");
    assert_eq!(e.size, 9876);
    assert_eq!(e.time, DateTime::ymd(2024, 2, 29));
    assert_eq!(e.name, "leap.txt");
}

#[test]
fn unix_no_link_count() {
    let e = one("-rw-r--r-- dave dev 100 Apr 01 2022 nolinks.txt");
    assert_eq!(e.owner_group, "dave dev");
    assert_eq!(e.size, 100);
    assert_eq!(e.time, DateTime::ymd(2022, 4, 1));
    assert_eq!(e.name, "nolinks.txt");
}

#[test]
fn unix_netware_split_permissions() {
    let e = one("d [RWCEAFMS] admin                512 May 20 14:02 PUBLIC");
    assert_eq!(e.name, "PUBLIC");
    assert_eq!(e.flags, FLAG_DIR);
    assert_eq!(e.permissions, "d [RWCEAFMS]");
    assert_eq!(e.owner_group, "admin");
    assert_eq!(e.size, 512);
    assert_eq!(e.time, recent(5, 20, 14, 2));
}

#[test]
fn unix_day_month_year() {
    let e = one("-rw-r--r--   1 erik  erik   2048 20 Jun 2020 europa.dat");
    assert_eq!(e.time, DateTime::ymd(2020, 6, 20));
    assert_eq!(e.size, 2048);
    assert_eq!(e.name, "europa.dat");
}

#[test]
fn unix_day_month_time_lowercase() {
    let e = one("-rw-r--r--   1 erik  erik   64 20 jun 08:30 nyheter.txt");
    assert_eq!(e.time, recent(6, 20, 8, 30));
    assert_eq!(e.size, 64);
    assert_eq!(e.name, "nyheter.txt");
}

#[test]
fn unix_iso_date() {
    let e = one("-rw-rw-r--   1 fred  fred   333 2023-07-04 16:45 iso.log");
    assert_eq!(e.time, DateTime::ymd_hm(2023, 7, 4, 16, 45));
    assert_eq!(e.name, "iso.log");
    assert_eq!(e.size, 333);
}

#[test]
fn unix_german_month() {
    let e = one("-rw-r--r--   1 gina  gina   1200 Dez 24  2019 weihnachten.txt");
    assert_eq!(e.time, DateTime::ymd(2019, 12, 24));
    assert_eq!(e.name, "weihnachten.txt");
}

#[test]
fn unix_french_month_with_dot() {
    let e = one("-rw-r--r--   1 gina  gina   1200 15 févr. 2021 fichier.txt");
    assert_eq!(e.time, DateTime::ymd(2021, 2, 15));
    assert_eq!(e.name, "fichier.txt");
}

#[test]
fn unix_size_unit_suffix() {
    let e = one("-rw-r--r--   1 hank  hank   1.5M Aug  3  2022 big.iso");
    assert_eq!(e.size, 1_572_864);
    assert_eq!(e.time, DateTime::ymd(2022, 8, 3));
}

#[test]
fn unix_classify_dir_slash() {
    let e = one("drwxr-xr-x   2 ivan  ivan   4096 Sep  9  2021 bin/");
    assert_eq!(e.name, "bin");
    assert!(e.is_dir());
}

#[test]
fn unix_classify_exec_star() {
    let e = one("-rwxr-xr-x   1 ivan  ivan    512 Sep  9  2021 run.sh*");
    assert_eq!(e.name, "run.sh");
    assert!(!e.is_dir());
}

#[test]
fn unix_24_00_is_next_day() {
    let e = one("-rw-r--r--   1 judy  judy      5 Oct 10 24:00 midnight.txt");
    assert_eq!(e.time, recent(10, 10, 24, 0));
    let tm = e.time.to_tm().unwrap();
    assert_eq!((tm.month, tm.day, tm.hour, tm.minute), (10, 11, 0, 0));
}

#[test]
fn unix_group_glued_to_size() {
    let e = one("-rw-r--r--   1 kim   supergroup123456 Dec  1  2020 glued.bin");
    assert_eq!(e.owner_group, "kim supergroup");
    assert_eq!(e.size, 123_456);
    assert_eq!(e.time, DateTime::ymd(2020, 12, 1));
    assert_eq!(e.name, "glued.bin");
}

#[test]
fn unix_korean_date() {
    let e = one("drwxr-xr-x   2 lee   lee    4096 2022년 3월 9일 korea");
    assert_eq!(e.name, "korea");
    assert!(e.is_dir());
    assert_eq!(e.time, DateTime::ymd(2022, 3, 9));
}

#[test]
fn unix_cjk_date() {
    let e = one("-rw-r--r--   1 lee   lee     777 3月 9日 2022年 asia.txt");
    assert_eq!(e.time, DateTime::ymd(2022, 3, 9));
    assert_eq!(e.name, "asia.txt");
    assert_eq!(e.size, 777);
}

#[test]
fn unix_russian_month() {
    let e = one("-rw-r--r--   1 mia   mia     900 ноя 30  2018 otchet.txt");
    assert_eq!(e.time, DateTime::ymd(2018, 11, 30));
    assert_eq!(e.name, "otchet.txt");
}

#[test]
fn unix_name_inner_spaces_trailing_trimmed() {
    let e = one("-rw-r--r--   1 ned   ned      12 Jan  5 12:00 name with  double  spaces ");
    assert_eq!(e.name, "name with  double  spaces");
    assert_eq!(e.time, recent(1, 5, 12, 0));
}

#[test]
fn unix_total_line_dropped() {
    none("total 1234");
}

#[test]
fn unix_extra_variants() {
    // numeric owner/group
    let e = one("-rw-r--r--   1 500 500 1234 Mar 14 2020 numeric-owner");
    assert_eq!(e.owner_group, "500 500");
    // device file with major, minor
    let e = one("crw-rw-rw-   1 root  root   1,   3 Mar 14  2020 null");
    assert_eq!(e.name, "null");
    assert_eq!(e.owner_group, "root root");
    // dot entries
    none("drwxr-xr-x   2 a b 4096 Mar 14  2020 .");
    none("drwxr-xr-x   2 a b 4096 Mar 14  2020 ..");
    // year followed by a time
    let e = one("-rw-r--r--   1 a b 10 Mar 14 2020 10:11 both.txt");
    assert_eq!(e.time, DateTime::ymd_hm(2020, 3, 14, 10, 11));
    assert_eq!(e.name, "both.txt");
    // full ISO time with zone
    let e = one("-rw-r--r--   1 a b 10 2020-03-14 10:11:12.500000000 +0200 zoned.txt");
    assert_eq!(e.time.to_string(), "2020-03-14 08:11:12.500");
    assert_eq!(e.name, "zoned.txt");
    // MM-DD-YY HH:MM
    let e = one("-rw-r--r--   1 a b 10 03-14-20 10:11 us.txt");
    assert_eq!(e.time, DateTime::ymd_hm(2020, 3, 14, 10, 11));
    // link target with spaces
    let e = one("lrwxrwxrwx   1 a b 10 Mar 14  2020 my link -> my target");
    assert_eq!(e.name, "my link");
    assert_eq!(e.target.as_deref(), Some("my target"));
    // ACL marker
    assert_eq!(one("-rw-r--r--+  1 a b 10 Mar 14  2020 acl").name, "acl");
}

// ===================================================================== C. numeric Unix

#[test]
fn numeric_unix_file() {
    let e = one("100644 501 20 48213 1710407722 numeric.txt");
    assert_eq!(
        e,
        DirEntry {
            name: "numeric.txt".into(),
            size: 48213,
            permissions: "100644".into(),
            owner_group: "501 20".into(),
            time: DateTime::from_unix_secs(1_710_407_722, Accuracy::Seconds),
            ..Default::default()
        }
    );
}

#[test]
fn numeric_unix_dir() {
    let e = one("40755 501 20 4096 1700000000 numdir");
    assert_eq!(e.name, "numdir");
    assert!(e.is_dir());
    assert!(one("040755 0 0 0 0 dir2").is_dir());
}

// ===================================================================== D. DOS

#[test]
fn dos_am_file() {
    let e = one("03-14-24  09:15AM               48213 quarterly report.pdf");
    assert_eq!(e.time, DateTime::ymd_hm(2024, 3, 14, 9, 15));
    assert_eq!(e.size, 48213);
    assert_eq!(e.name, "quarterly report.pdf");
}

#[test]
fn dos_pm_dir() {
    let e = one("11-02-2021  05:00PM       <DIR>          Old Projects");
    assert!(e.is_dir());
    assert_eq!(e.name, "Old Projects");
    assert_eq!(e.time, DateTime::ymd_hm(2021, 11, 2, 17, 0));
}

#[test]
fn dos_iso_thousands() {
    let e = one("2023-07-04  16:45            1,234,567 sizes.bin");
    assert_eq!(e.time, DateTime::ymd_hm(2023, 7, 4, 16, 45));
    assert_eq!(e.size, 1_234_567);
}

#[test]
fn dos_dotted_day_first() {
    let e = one("14.03.2024  21:07                  42 dotted.txt");
    assert_eq!(e.time, DateTime::ymd_hm(2024, 3, 14, 21, 7));
    assert_eq!(e.size, 42);
}

#[test]
fn dos_slash_day_first_12am() {
    let e = one("25/12/2022  12:30AM               7 xmas.txt");
    assert_eq!(e.time, DateTime::ymd_hm(2022, 12, 25, 0, 30));
    assert_eq!(e.name, "xmas.txt");
}

#[test]
fn dos_extra_variants() {
    let e = one("03-14-24  12:05PM  1.234 noon.txt");
    assert_eq!(e.time, DateTime::ymd_hm(2024, 3, 14, 12, 5));
    assert_eq!(e.size, 1234);
    let e = one("03/14/2024  09:15:30 PM  5 sep-ampm.txt");
    assert_eq!(e.time, DateTime::ymd_hms(2024, 3, 14, 21, 15, 30));
    none("03-14-24  09:15AM  <DIR>  .");
}

// ===================================================================== E. EPLF

#[test]
fn eplf_file() {
    let e = one("+i8388621.29609,m1710407722,r,s48213,\tquarterly report.pdf");
    assert_eq!(e.name, "quarterly report.pdf");
    assert_eq!(e.size, 48213);
    assert_eq!(
        e.time,
        DateTime::from_unix_secs(1_710_407_722, Accuracy::Seconds)
    );
    assert_eq!(e.time.accuracy(), Accuracy::Seconds);
}

#[test]
fn eplf_dir() {
    let e = one("+/,m1700000000,\tphotos");
    assert_eq!(e.name, "photos");
    assert!(e.is_dir());
    assert_eq!(
        e.time,
        DateTime::from_unix_secs(1_700_000_000, Accuracy::Seconds)
    );
}

#[test]
fn eplf_permissions() {
    let e = one("+r,s10,up644,\tx.txt");
    assert_eq!(e.size, 10);
    assert_eq!(e.permissions, "644");
    assert!(!e.has_date());
}

// ===================================================================== F. OpenVMS

#[test]
fn vms_file() {
    let e = one("REPORT.TXT;3          12/16       14-MAR-2024 09:15:22  [ALICE]  (RWED,RWED,RE,)");
    assert_eq!(
        e,
        DirEntry {
            name: "REPORT.TXT;3".into(),
            size: 6144,
            permissions: "RWED,RWED,RE,".into(),
            owner_group: "ALICE".into(),
            time: DateTime::ymd_hms(2024, 3, 14, 9, 15, 22),
            ..Default::default()
        }
    );
}

#[test]
fn vms_dir() {
    let e = one(
        "ARCHIVE.DIR;1          1/3         2-NOV-2021 17:00:00  [SYSTEM,ALICE]  (RWE,RWE,RE,E)",
    );
    assert_eq!(e.name, "ARCHIVE");
    assert!(e.is_dir());
    assert_eq!(e.owner_group, "SYSTEM,ALICE");
    assert_eq!(e.time, DateTime::ymd_hms(2021, 11, 2, 17, 0, 0));
}

#[test]
fn vms_wrapped_two_lines() {
    let data = "A_VERY_LONG_FILE_NAME_THAT_WRAPS.DAT;12\r\n                     7/8     5-JAN-2023 08:00:00  [BOB]  (RWED,RWED,,)\r\n";
    let v = entries_st(data.as_bytes(), ServerType::Vms);
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].name, "A_VERY_LONG_FILE_NAME_THAT_WRAPS.DAT;12");
    assert_eq!(v[0].size, 3584);
    assert_eq!(v[0].owner_group, "BOB");
    assert_eq!(v[0].time, DateTime::ymd_hms(2023, 1, 5, 8, 0, 0));
    // same with any server type and byte-wise streaming
    let mut p = ListingParser::new(ServerType::Unix);
    for b in data.as_bytes() {
        assert!(p.feed(std::slice::from_ref(b)));
    }
    assert_eq!(p.parse("/").entries, v);
}

#[test]
fn vms_caret_escapes() {
    let e = one("FILE^.WITH^.DOTS.TXT;1   1/1  1-JAN-2020 00:00:00 [X] (RWED,,,)");
    assert_eq!(e.name, "FILE.WITH.DOTS.TXT;1");
    assert_eq!(e.size, 512);
}

#[test]
fn vms_extra_variants() {
    let e = one("SUB.DIR;2  1  2-NOV-2021 17:00 [A] (RWE,,,)");
    assert!(e.is_dir());
    assert_eq!(e.name, "SUB;2");
    assert_eq!(e.time, DateTime::ymd_hm(2021, 11, 2, 17, 0));
    let e = one("LOG.TXT;1  4  2-NOV-2021 17:00:01.50 [SYSTEM, OPER] (RWED,,,)");
    assert_eq!(e.time.to_string(), "2021-11-02 17:00:01.500");
    assert_eq!(e.owner_group, "SYSTEM, OPER");
    // the wrapped name followed by an unrelated line
    let v = entries_st(b"LONELY.TXT;1\r\ntotal 1\r\n", ServerType::Vms);
    assert!(v.is_empty());
}

// ===================================================================== G. IBM i

#[test]
fn ibm_i_file() {
    let e = one("ALICE      48213 03/14/24 09:15:22 *STMF      report.pdf");
    assert_eq!(e.owner_group, "ALICE");
    assert_eq!(e.size, 48213);
    assert_eq!(e.time, DateTime::ymd_hms(2024, 3, 14, 9, 15, 22));
    assert_eq!(e.name, "report.pdf");
    assert!(!e.is_dir());
}

#[test]
fn ibm_i_dir() {
    let e = one("QSYS        8192 11/02/21 17:00:00 *DIR       data/");
    assert_eq!(e.name, "data");
    assert!(e.is_dir());
}

// ===================================================================== H. OS/2, size first

#[test]
fn os2_file() {
    let e = one("     48213           A          03-14-24   09:15  REPORT.PDF");
    assert_eq!(e.size, 48213);
    assert_eq!(e.time, DateTime::ymd_hm(2024, 3, 14, 9, 15));
    assert_eq!(e.name, "REPORT.PDF");
}

#[test]
fn os2_dir() {
    let e = one("         0           DIR        11-02-21   17:00  PROJECTS");
    assert_eq!(e.name, "PROJECTS");
    assert!(e.is_dir());
    assert_eq!(e.time, DateTime::ymd_hm(2021, 11, 2, 17, 0));
}

#[test]
fn size_first_month_name() {
    let e = one("   48213    Mar 14 2024 09:15   report.pdf");
    assert_eq!(e.size, 48213);
    assert_eq!(e.time, DateTime::ymd_hm(2024, 3, 14, 9, 15));
    assert_eq!(e.name, "report.pdf");
}

// ===================================================================== I. MVS

#[test]
fn mvs_dataset_ps() {
    let e = one("WRK001 3390   2024/03/14  1   15  FB      80  27920  PS  ALICE.REPORT.DATA");
    assert_eq!(
        e,
        DirEntry {
            name: "ALICE.REPORT.DATA".into(),
            time: DateTime::ymd(2024, 3, 14),
            ..Default::default()
        }
    );
}

#[test]
fn mvs_dataset_po_is_dir() {
    let e = one("WRK002 3390   2021/11/02  2   45  FB      80  27920  PO  ALICE.SOURCE.COBOL");
    assert_eq!(e.name, "ALICE.SOURCE.COBOL");
    assert!(e.is_dir());
    assert_eq!(e.time, DateTime::ymd(2021, 11, 2));
    assert!(one("WRK002 3390   2021/11/02  2   45  FB      80  27920  PO-E  ALICE.PDSE").is_dir());
}

#[test]
fn mvs_dataset_never_referred() {
    let e = one("WRK003 3390   **NONE**    1    1  FB      80   3120  PS  ALICE.NEVER.USED");
    assert_eq!(e, file("ALICE.NEVER.USED"));
}

#[test]
fn mvs_dataset_header_dropped() {
    none("Volume Unit    Referred Ext Used Recfm Lrecl BlkSz Dsorg Dsname");
}

#[test]
fn mvs_pds_member() {
    let e = one("PAYROLL   01.03 2021/11/02 2024/03/14 09:15   120   100     0 ALICE");
    assert_eq!(e.name, "PAYROLL");
    assert_eq!(e.size, 120);
    assert_eq!(e.time, DateTime::ymd_hm(2024, 3, 14, 9, 15));
}

#[test]
fn mvs_migrated() {
    let e = one_st(
        "Migrated                                         ALICE.OLD.DATA",
        ServerType::Mvs,
    );
    assert_eq!(e, file("ALICE.OLD.DATA"));
    none_st(
        "Migrated                                         ALICE.OLD.DATA",
        ServerType::Unix,
    );
}

#[test]
fn mvs_tape() {
    let e = one_st("VOL001 Tape ALICE.BACKUP.TAPE", ServerType::Mvs);
    assert_eq!(e, file("ALICE.BACKUP.TAPE"));
    none_st("VOL001 Tape ALICE.BACKUP.TAPE", ServerType::Unix);
}

// ===================================================================== J. z/VM

#[test]
fn zvm_file() {
    let line = "PROFILE  EXEC     V         72         15          1 2024-03-14 09:15:22 ALICE";
    let e = one_st(line, ServerType::Zvm);
    assert_eq!(
        e,
        DirEntry {
            name: "PROFILE.EXEC".into(),
            size: 1080,
            owner_group: "ALICE".into(),
            time: DateTime::ymd_hms(2024, 3, 14, 9, 15, 22),
            ..Default::default()
        }
    );
    none_st(line, ServerType::Unix);
}

// ===================================================================== K. HP NonStop

#[test]
fn hp_nonstop_file() {
    let line = r#"REPORT          101       48213 14-Mar-24 09:15:22 255,101  "nunu""#;
    let e = one_st(line, ServerType::HpNonstop);
    assert_eq!(e.name, "REPORT");
    assert_eq!(e.size, 48213);
    assert_eq!(e.time, DateTime::ymd_hms(2024, 3, 14, 9, 15, 22));
    assert_eq!(e.owner_group, "255,101");
    assert_eq!(e.permissions, r#""nunu""#);
    none_st(line, ServerType::Unix);
}

#[test]
fn hp_nonstop_owner_with_space() {
    let e = one_st(
        r#"LOGFILE           0         10 02-Nov-21 17:00:00 255, 101 "oooo""#,
        ServerType::HpNonstop,
    );
    assert_eq!(e.name, "LOGFILE");
    assert_eq!(e.owner_group, "255, 101");
    assert_eq!(e.size, 10);
    assert_eq!(e.time, DateTime::ymd_hms(2021, 11, 2, 17, 0, 0));
    assert_eq!(e.permissions, r#""oooo""#);
}

// ===================================================================== L. OS-9

#[test]
fn os9_dir() {
    let e = one("0.0       24/03/14 0915  d-ewrewr  2A  4096  CMDS");
    assert_eq!(
        e,
        DirEntry {
            name: "CMDS".into(),
            size: 4096,
            permissions: "d-ewrewr".into(),
            owner_group: "0.0".into(),
            time: DateTime::ymd(2024, 3, 14),
            flags: FLAG_DIR,
            ..Default::default()
        }
    );
}

// ===================================================================== M. NLST

#[test]
fn nlst_names() {
    let v = entries_st(b"alpha.txt\r\nbeta.txt\r\n", ServerType::Unix);
    assert_eq!(v, vec![file("alpha.txt"), file("beta.txt")]);
}

#[test]
fn nlst_with_space_gives_nothing() {
    let v = entries_st(b"alpha.txt\r\nnot a name\r\n", ServerType::Unix);
    assert!(v.is_empty());
}

#[test]
fn nlst_extra_rules() {
    let v = entries_st(b".\n..\nx\n", ServerType::Unix);
    assert_eq!(v, vec![file("x")]);
    // one recognised line disables the name-only interpretation
    let v = entries_st(b"alpha.txt\n+/,\tdir\n", ServerType::Unix);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].name, "dir");
    let v = entries_st(b"NAME.TXT;1\nOTHER.TXT;2\n", ServerType::Vms);
    assert_eq!(v, vec![file("NAME.TXT;1"), file("OTHER.TXT;2")]);
}

// ===================================================================== multi-line, streaming

const MIXED: &str = "total 12\r\n\
-rw-r--r--    1 alice    staff        48213 Mar 14 09:15 quarterly report.pdf\r\n\
drwxr-xr-x    5 bob      users         4096 Nov  2  2021 Old Projects\r\n\
\r\n\
   \t \r\n\
type=file;size=10;modify=20240314091522; mlsd.txt\n\
03-14-24  09:15AM               48213 dos.pdf\r\
+/,m1700000000,\tphotos\x00\
A_VERY_LONG_FILE_NAME_THAT_WRAPS.DAT;12\r\n\
                     7/8     5-JAN-2023 08:00:00  [BOB]  (RWED,RWED,,)\r\n\
WRK001 3390   2024/03/14  1   15  FB      80  27920  PS  ALICE.REPORT.DATA\r\n\
drwxr-xr-x    2 bob      users         4096 Nov  2  2021 .\r\n\
-rw-r--r--   1 mia   mia     900 ноя 30  2018 otchet.txt";

fn names(v: &[DirEntry]) -> Vec<&str> {
    v.iter().map(|e| e.name.as_str()).collect()
}

#[test]
fn multi_line_mixed_listing_keeps_order() {
    let v = entries_st(MIXED.as_bytes(), ServerType::Unix);
    assert_eq!(
        names(&v),
        [
            "quarterly report.pdf",
            "Old Projects",
            "mlsd.txt",
            "dos.pdf",
            "photos",
            "A_VERY_LONG_FILE_NAME_THAT_WRAPS.DAT;12",
            "ALICE.REPORT.DATA",
            "otchet.txt",
        ]
    );
}

fn dumps(l: &DirectoryListing) -> Vec<String> {
    l.entries.iter().map(DirEntry::dump).collect()
}

#[test]
fn streaming_one_byte_chunks_equal_one_chunk() {
    let whole = parse_listing(MIXED.as_bytes(), ServerType::Unix);
    let mut p = ListingParser::new(ServerType::Unix);
    for b in MIXED.as_bytes() {
        assert!(p.feed(std::slice::from_ref(b)));
    }
    let streamed = p.parse("/pub");
    assert_eq!(streamed.path, "/pub");
    assert_eq!(dumps(&streamed), dumps(&whole));
    assert_eq!(streamed.entries, whole.entries);
}

#[test]
fn streaming_split_inside_utf8_char() {
    let line = "-rw-r--r--   1 mia   mia     900 ноя 30  2018 отчёт.txt\n".as_bytes();
    let pos = line.iter().position(|&b| b >= 0x80).unwrap() + 1;
    let mut p = ListingParser::new(ServerType::Unix);
    p.feed(&line[..pos]);
    p.feed(&line[pos..]);
    let l = p.parse("");
    assert_eq!(l.entries.len(), 1);
    assert_eq!(l.entries[0].name, "отчёт.txt");
    assert_eq!(l.entries[0].time, DateTime::ymd(2018, 11, 30));
}

// ===================================================================== limit, offset, server type

#[test]
fn limit_truncates() {
    let data = "+s1,\ta\n+s2,\tb\n+s3,\tc\n+s4,\td\n+s5,\te\n";
    let mut p = ListingParser::new(ServerType::Unix);
    p.set_limit(3);
    p.feed(data.as_bytes());
    let l = p.parse("");
    assert_eq!(names(&l.entries), ["a", "b", "c"]);
    assert!(p.truncated());

    let mut p = ListingParser::new(ServerType::Unix);
    p.set_limit(5);
    p.feed(data.as_bytes());
    assert_eq!(p.parse("").len(), 5);
    assert!(!p.truncated());

    let mut p = ListingParser::new(ServerType::Unix);
    p.feed(data.as_bytes());
    assert_eq!(p.parse("").len(), 5);
    assert!(!p.truncated());

    let mut p = ListingParser::new(ServerType::Unix);
    p.set_limit(1);
    p.feed(b"n1\nn2\nn3\n");
    assert_eq!(names(&p.parse("").entries), ["n1"]);
    assert!(p.truncated());
}

#[test]
fn timezone_offset_applied() {
    let data = "type=file;modify=20240314091522; a\n\
                -rw-r--r-- 1 u g 1 2023-07-04 16:45 b\n\
                +r,\tc\n";
    let mut p = ListingParser::new(ServerType::Unix);
    p.set_server_timezone_offset(90);
    p.feed(data.as_bytes());
    let l = p.parse("");
    assert_eq!(
        l.entries[0].time,
        DateTime::ymd_hms(2024, 3, 14, 10, 45, 22)
    );
    assert_eq!(l.entries[1].time, DateTime::ymd_hm(2023, 7, 4, 18, 15));
    assert!(l.entries[2].time.is_empty());

    let mut p = ListingParser::new(ServerType::Unix);
    p.set_server_timezone_offset(-60);
    p.feed(b"-rw-r--r-- 1 u g 1 2024-01-01 00:30 x\n");
    assert_eq!(
        p.parse("").entries[0].time,
        DateTime::ymd_hm(2023, 12, 31, 23, 30)
    );
}

#[test]
fn unix_listing_with_every_server_type() {
    let data = "total 3\n\
        -rw-r--r--    1 alice    staff        48213 Mar 14 2020 a.pdf\n\
        drwxr-xr-x    5 bob      users         4096 Nov  2  2021 Old Projects\n\
        lrwxrwxrwx    1 root     root            11 Jan  7  2023 latest -> releases/v3\n";
    for st in [
        ServerType::Unix,
        ServerType::Vms,
        ServerType::Dos,
        ServerType::Mvs,
        ServerType::VxWorks,
        ServerType::Zvm,
        ServerType::HpNonstop,
        ServerType::DosVirtual,
        ServerType::Cygwin,
        ServerType::DosFwdSlashes,
    ] {
        let v = entries_st(data.as_bytes(), st);
        assert_eq!(names(&v), ["a.pdf", "Old Projects", "latest"], "{st:?}");
        assert_eq!(v[0].size, 48213);
    }
}

#[test]
fn server_type_change_affects_later_lines() {
    let line = b"PROFILE  EXEC     V  72  15  1 2024-03-14 09:15:22 ALICE\n";
    let mut p = ListingParser::new(ServerType::Unix);
    p.feed(line);
    p.set_server_type(ServerType::Zvm);
    p.feed(line);
    let l = p.parse("");
    assert_eq!(names(&l.entries), ["PROFILE.EXEC"]);
}

#[test]
fn reset_starts_new_listing() {
    let mut p = ListingParser::new(ServerType::Unix);
    p.set_limit(1);
    p.feed(b"+r,\ta\n+r,\tb\n");
    assert_eq!(p.parse("").len(), 1);
    assert!(p.truncated());
    p.reset();
    assert!(!p.truncated());
    p.feed(b"+r,\tc\n");
    assert_eq!(names(&p.parse("").entries), ["c"]);
}

// ===================================================================== encodings

fn to_ebcdic(s: &str) -> Vec<u8> {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'i' => 0x81 + (b - b'a'),
            b'j'..=b'r' => 0x91 + (b - b'j'),
            b's'..=b'z' => 0xA2 + (b - b's'),
            b'A'..=b'I' => 0xC1 + (b - b'A'),
            b'J'..=b'R' => 0xD1 + (b - b'J'),
            b'S'..=b'Z' => 0xE2 + (b - b'S'),
            b'0'..=b'9' => 0xF0 + (b - b'0'),
            b' ' => 0x40,
            b'.' => 0x4B,
            b'-' => 0x60,
            b'/' => 0x61,
            b':' => 0x7A,
            b'*' => 0x5C,
            b'\n' => 0x15,
            other => panic!("no EBCDIC mapping in test for {other}"),
        })
        .collect()
}

#[test]
fn ebcdic_listing_detected_and_translated() {
    let text = "WRK001 3390   2024/03/14  1   15  FB      80  27920  PS  ALICE.REPORT.DATA\n\
                WRK002 3390   2021/11/02  2   45  FB      80  27920  PO  ALICE.SOURCE.COBOL\n\
                WRK003 3390   **NONE**    1    1  FB      80   3120  PS  ALICE.NEVER.USED\n";
    let data = to_ebcdic(text);
    let v = entries_st(&data, ServerType::Mvs);
    assert_eq!(
        names(&v),
        [
            "ALICE.REPORT.DATA",
            "ALICE.SOURCE.COBOL",
            "ALICE.NEVER.USED"
        ]
    );
    assert!(v[1].is_dir());

    // longer than the detection window, streamed byte by byte
    let long: String = (0..20)
        .map(|i| {
            format!("WRK{i:03} 3390   2024/03/14  1   15  FB      80  27920  PS  ALICE.DS{i}\n")
        })
        .collect();
    let data = to_ebcdic(&long);
    assert!(data.len() > 512);
    let mut p = ListingParser::new(ServerType::Mvs);
    for b in &data {
        p.feed(std::slice::from_ref(b));
    }
    let l = p.parse("");
    assert_eq!(l.len(), 20);
    assert_eq!(l.entries[19].name, "ALICE.DS19");
    assert_eq!(parse_listing(&data, ServerType::Mvs).entries, l.entries);
}

#[test]
fn ascii_listing_not_taken_for_ebcdic() {
    // Long ASCII listing with only CR line ends (no LF at all).
    let line = "-rw-r--r--    1 alice    staff        48213 Mar 14 2020 a.pdf\r";
    let data = line.repeat(20);
    let v = entries_st(data.as_bytes(), ServerType::Mvs);
    assert_eq!(v.len(), 20);
}

#[test]
fn utf8_bom_removed() {
    let mut data = b"\xEF\xBB\xBF-rw-r--r-- 1 a b 7 Mar 14 2020 bom.txt\r\n".to_vec();
    data.extend_from_slice(b"\xEF\xBB\xBFtype=file;size=3; m.txt\r\n");
    let v = entries_st(&data, ServerType::Unix);
    assert_eq!(names(&v), ["bom.txt", "m.txt"]);
    assert_eq!(v[0].permissions, "-rw-r--r--");
}

#[test]
fn latin1_lines() {
    // French month and name in Latin-1
    let v = entries_st(
        b"-rw-r--r-- 1 a b 7 15 d\xE9c. 2021 caf\xE9.txt\n",
        ServerType::Unix,
    );
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].name, "café.txt");
    assert_eq!(v[0].time, DateTime::ymd(2021, 12, 15));
    // Russian month in Windows-1251 ("ноя")
    let v = entries_st(
        b"-rw-r--r-- 1 a b 7 \xED\xEE\xFF 30 2018 r.txt\n",
        ServerType::Unix,
    );
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].time, DateTime::ymd(2018, 11, 30));
    // Uppercase Windows-1251 ("ДЕК")
    let v = entries_st(
        b"-rw-r--r-- 1 a b 7 \xC4\xC5\xCA 30 2018 r.txt\n",
        ServerType::Unix,
    );
    assert_eq!(v[0].time, DateTime::ymd(2018, 12, 30));
    // UTF-8 month in a line that is not valid UTF-8 overall
    let mut line = "-rw-r--r-- 1 a b 7 15 févr. 2021 ".as_bytes().to_vec();
    line.extend_from_slice(b"x\xFF.txt\n");
    let v = entries_st(&line, ServerType::Unix);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].time, DateTime::ymd(2021, 2, 15));
    assert_eq!(v[0].name, "x\u{FF}.txt");
}

// ===================================================================== failure

#[test]
fn overlong_line_fails() {
    let mut p = ListingParser::new(ServerType::Unix);
    assert!(p.feed(b"+r,\tok\n"));
    let long = vec![b'a'; 10_001];
    assert!(!p.feed(&long));
    assert!(!p.feed(b"\n"));
    let l = p.parse("/x");
    assert!(l.failed);
    assert_eq!(l.path, "/x");
    assert!(p.error().is_some());

    // exactly 10 000 bytes is still fine
    let mut p = ListingParser::new(ServerType::Unix);
    let mut data = vec![b'a'; 10_000];
    data.push(b'\n');
    assert!(p.feed(&data));
    assert!(!p.parse("").failed);
    assert!(p.error().is_none());

    // byte-wise feeding fails as well, even without a line end
    let mut p = ListingParser::new(ServerType::Unix);
    let mut ok = true;
    for _ in 0..10_001 {
        ok = p.feed(b"x");
    }
    assert!(!ok);
    assert!(p.parse("").failed);
    p.reset();
    assert!(p.error().is_none());
    assert!(p.feed(b"+r,\ta\n"));
    assert_eq!(p.parse("").len(), 1);
}

// ===================================================================== randomized robustness

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const EXAMPLES: &[&str] = &[
    "type=file;size=48213;modify=20240314091522;perm=adfrw;UNIX.mode=0644;UNIX.owner=alice;UNIX.group=staff; quarterly report.pdf",
    "type=OS.unix=slink:/srv/data;modify=20240505121212; data-link",
    "-rw-r--r--    1 alice    staff        48213 Mar 14 09:15 quarterly report.pdf",
    "lrwxrwxrwx    1 root     root            11 Jan  7  2023 latest -> releases/v3",
    "d [RWCEAFMS] admin                512 May 20 14:02 PUBLIC",
    "-rw-r--r--   1 kim   supergroup123456 Dec  1  2020 glued.bin",
    "drwxr-xr-x   2 lee   lee    4096 2022년 3월 9일 korea",
    "-rw-r--r--   1 mia   mia     900 ноя 30  2018 otchet.txt",
    "100644 501 20 48213 1710407722 numeric.txt",
    "11-02-2021  05:00PM       <DIR>          Old Projects",
    "+i8388621.29609,m1710407722,r,s48213,\tquarterly report.pdf",
    "REPORT.TXT;3          12/16       14-MAR-2024 09:15:22  [ALICE]  (RWED,RWED,RE,)",
    "ALICE      48213 03/14/24 09:15:22 *STMF      report.pdf",
    "     48213           A          03-14-24   09:15  REPORT.PDF",
    "WRK001 3390   2024/03/14  1   15  FB      80  27920  PS  ALICE.REPORT.DATA",
    "PAYROLL   01.03 2021/11/02 2024/03/14 09:15   120   100     0 ALICE",
    "PROFILE  EXEC     V         72         15          1 2024-03-14 09:15:22 ALICE",
    "REPORT          101       48213 14-Mar-24 09:15:22 255,101  \"nunu\"",
    "0.0       24/03/14 0915  d-ewrewr  2A  4096  CMDS",
    "-rw-r--r--   1 ann  ann   812 11-03 18:20 md-time.txt",
    "drwxr-xr-x 4 uwe dev 2048 3. Mär., 09:10 projekt",
    "-rw-r--r--   1 users CORP bjones 640 Feb  2 11:00 domain.txt",
    "drwxr-xr-x 2 ops ops 1024 no-date-dir",
    "     0  Aug 19, 2004 16:40 tools/",
    "   1536    Apr-09-2001  08:15:44   vx-dir   <DIR>",
    "router.cfg      20480  02/17/05  Thur.   16:44:10",
    "PLAN.DOC;1   [ADMIN]   2/3   12-OCT-2015 08:30  (RWE,RWE,RE,)",
    "VOLA02 3390   2016/09/12 812000 U 0 27998 PO OPS.LOADLIB",
    "type=file;size=99;modify=20200101120000;\tname with tab.txt",
    "type=file;size=5 legacy.txt",
];

const ALL_TYPES: [ServerType; 10] = [
    ServerType::Unix,
    ServerType::Vms,
    ServerType::Dos,
    ServerType::Mvs,
    ServerType::VxWorks,
    ServerType::Zvm,
    ServerType::HpNonstop,
    ServerType::DosVirtual,
    ServerType::Cygwin,
    ServerType::DosFwdSlashes,
];

fn chunked(data: &[u8], st: ServerType, rng: &mut XorShift) -> DirectoryListing {
    let mut p = ListingParser::new(st);
    let mut i = 0;
    while i < data.len() {
        let n = 1 + rng.below(17);
        let end = (i + n).min(data.len());
        p.feed(&data[i..end]);
        i = end;
    }
    p.parse("")
}

#[test]
fn random_bytes_never_panic() {
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    for round in 0..3000 {
        let len = rng.below(400);
        let data: Vec<u8> = (0..len)
            .map(|_| match rng.below(4) {
                0 => b" \t\r\n\x00;:-/.,+"[rng.below(12)],
                1 => b'0' + rng.below(10) as u8,
                _ => rng.next() as u8,
            })
            .collect();
        let st = ALL_TYPES[round % ALL_TYPES.len()];
        let whole = parse_listing(&data, st);
        let parts = chunked(&data, st, &mut rng);
        assert_eq!(dumps(&whole), dumps(&parts), "input {data:?}");
    }
}

#[test]
fn mutated_examples_never_panic() {
    let mut rng = XorShift(0x0123_4567_89AB_CDEF);
    for round in 0..4000 {
        let mut data = EXAMPLES[rng.below(EXAMPLES.len())].as_bytes().to_vec();
        if rng.below(2) == 0 {
            data.push(b'\n');
            data.extend_from_slice(EXAMPLES[rng.below(EXAMPLES.len())].as_bytes());
        }
        for _ in 0..1 + rng.below(4) {
            let pos = rng.below(data.len() + 1);
            match rng.below(3) {
                0 => data.insert(pos, b"\r\n \t\x00"[rng.below(5)]),
                1 if pos < data.len() => {
                    data.remove(pos);
                }
                _ => data.insert(pos, rng.next() as u8),
            }
        }
        let st = ALL_TYPES[round % ALL_TYPES.len()];
        let whole = parse_listing(&data, st);
        let parts = chunked(&data, st, &mut rng);
        assert_eq!(dumps(&whole), dumps(&parts), "input {data:?}");
    }
}

#[test]
fn all_examples_parse_with_matching_type() {
    for (i, ex) in EXAMPLES.iter().enumerate() {
        let st = match i {
            16 => ServerType::Zvm,
            17 => ServerType::HpNonstop,
            _ => ServerType::Unix,
        };
        assert_eq!(entries_st(ex.as_bytes(), st).len(), 1, "{ex}");
    }
}

// ===================================================================== Addendum A

fn one_bytes(line: &[u8], st: ServerType) -> DirEntry {
    let mut data = line.to_vec();
    data.extend_from_slice(b"\r\n");
    let v = entries_st(&data, st);
    assert_eq!(
        v.len(),
        1,
        "line {:?} gave {:?}",
        String::from_utf8_lossy(line),
        v.iter().map(DirEntry::dump).collect::<Vec<_>>()
    );
    v.into_iter().next().unwrap()
}

// --- A1–A8: Unix ls -l additions

#[test]
fn add_a1_numeric_month_day() {
    let e = one("-rw-r--r--   1 ann  ann   812 4 17 06:05 numeric-md.txt");
    assert_eq!(e.time, recent(4, 17, 6, 5));
    assert_eq!(e.name, "numeric-md.txt");
    assert_eq!(e.size, 812);
    assert_eq!(e.owner_group, "ann ann");
    let e = one("-rw-r--r--   1 ann  ann   812 12 31 2019 numeric-mdy.txt");
    assert_eq!(e.time, DateTime::ymd(2019, 12, 31));
}

#[test]
fn add_a2_month_day_year() {
    let e = one("-rw-r--r--   1 ann  ann   812 11-03 2019 md-year.txt");
    assert_eq!(e.time, DateTime::ymd(2019, 11, 3));
    assert_eq!(e.name, "md-year.txt");
}

#[test]
fn add_a2_month_day_time() {
    let e = one("-rw-r--r--   1 ann  ann   812 11-03 18:20 md-time.txt");
    assert_eq!(e.time, recent(11, 3, 18, 20));
    assert_eq!(e.name, "md-time.txt");
    assert_eq!(e.size, 812);
}

#[test]
fn add_a3_slash_iso_date() {
    let e = one("-rw-r--r--   1 ann  ann   812 2018/05/30 07:45 slash.txt");
    assert_eq!(e.time, DateTime::ymd_hm(2018, 5, 30, 7, 45));
    assert_eq!(e.name, "slash.txt");
}

#[test]
fn add_a3_dot_iso_date() {
    let e = one("-rw-r--r--   1 ann  ann   812 2017.02.14 dot.txt");
    assert_eq!(e.time, DateTime::ymd(2017, 2, 14));
    assert_eq!(e.time.accuracy(), Accuracy::Days);
    assert_eq!(e.name, "dot.txt");
    let e = one("-rw-r--r--   1 ann  ann   812 2017.02.14 23:59:58 dot-time.txt");
    assert_eq!(e.time, DateTime::ymd_hms(2017, 2, 14, 23, 59, 58));
}

#[test]
fn add_a4_day_mon_yy() {
    let e = one("-rw-r--r--   1 ann  ann   812 07-aug-98 dmy.txt");
    assert_eq!(e.time, DateTime::ymd(1998, 8, 7));
    assert_eq!(e.name, "dmy.txt");
    let e = one("-rw-r--r--   1 ann  ann   812 07-Aug-2031 dmy4.txt");
    assert_eq!(e.time, DateTime::ymd(2031, 8, 7));
}

#[test]
fn add_a5_german_dots_and_comma() {
    let e = one("drwxr-xr-x 4 uwe dev 2048 3. Mär., 09:10 projekt");
    assert!(e.is_dir());
    assert_eq!(e.name, "projekt");
    assert_eq!(e.time, recent(3, 3, 9, 10));
    assert_eq!(e.size, 2048);
    let e = one("-rw-r--r-- 1 uwe dev 10 24. Dez., 2019 brief.txt");
    assert_eq!(e.time, DateTime::ymd(2019, 12, 24));
}

#[test]
fn add_a6_three_token_owner_group() {
    let e = one("-rw-r--r--   1 users CORP bjones 640 Feb  2 11:00 domain.txt");
    assert_eq!(e.owner_group, "users CORP bjones");
    assert_eq!(e.size, 640);
    assert_eq!(e.time, recent(2, 2, 11, 0));
    assert_eq!(e.name, "domain.txt");
    // without a link count
    let e = one("-rw-r--r-- users CORP bjones 640 Feb  2 2020 nolink.txt");
    assert_eq!(e.owner_group, "users CORP bjones");
    // the decision for a lone numeric token stays: it is the owner
    let e = one("-rw-r--r-- 1 100 Mar 14 2020 lone.txt");
    assert_eq!(e.owner_group, "1");
    assert_eq!(e.size, 100);
}

#[test]
fn add_a7_no_date() {
    let e = one("drwxr-xr-x 2 ops ops 1024 no-date-dir");
    assert_eq!(
        e,
        DirEntry {
            name: "no-date-dir".into(),
            size: 1024,
            permissions: "drwxr-xr-x".into(),
            owner_group: "ops ops".into(),
            flags: FLAG_DIR,
            ..Default::default()
        }
    );
    let e = one("drwxr-xr-x 2 0 0 1024 numeric owners");
    assert_eq!(e.owner_group, "0 0");
    assert_eq!(e.name, "numeric owners");
    let e = one("lrwxrwxrwx 1 ops ops 3 l -> t");
    assert_eq!(e.target.as_deref(), Some("t"));
    assert!(!e.has_date());
    // a dated interpretation wins
    let e = one("drwxr-xr-x 2 ops ops 1024 Mar 14 2020 dated");
    assert_eq!(e.name, "dated");
    assert!(e.has_date());
    none("drwxr-xr-x 2 ops ops");
}

#[test]
fn add_a8_legacy_asian_month_day_year() {
    let e = one_bytes(
        b"-rw-r--r--   1 lee   lee     777 5\x8c\x8e 21\x93\xfa 2006\x94\xf6 sjis-mdy.txt",
        ServerType::Unix,
    );
    assert_eq!(e.time, DateTime::ymd(2006, 5, 21));
    assert_eq!(e.name, "sjis-mdy.txt");
    assert_eq!(e.size, 777);
}

#[test]
fn add_a8_legacy_asian_year_month_day() {
    let e = one_bytes(
        b"drwxr-xr-x   2 lee   lee    4096 2006 5\x8c\x8e 21 sjis-ymd",
        ServerType::Unix,
    );
    assert_eq!(e.time, DateTime::ymd(2006, 5, 21));
    assert_eq!(e.name, "sjis-ymd");
    assert!(e.is_dir());
}

#[test]
fn add_a8_legacy_asian_month_day_time() {
    let e = one_bytes(
        b"-rw-r--r--   1 lee   lee     777 5\x8c\x8e 21 14:02 sjis-mdt.txt",
        ServerType::Unix,
    );
    assert_eq!(e.time, recent(5, 21, 14, 2));
    // EUC-style junk on every number
    let e = one_bytes(
        b"-rw-r--r--   1 lee   lee     777 2006\xc7\xaf 11\xb7\xee 3\xc6\xfc euc.txt",
        ServerType::Unix,
    );
    assert_eq!(e.time, DateTime::ymd(2006, 11, 3));
}

// --- A9–A10: size-first / OS/2

#[test]
fn add_a9_size_first_comma_and_dir_slash() {
    let e = one("     0  Aug 19, 2004 16:40 tools/");
    assert_eq!(e.name, "tools");
    assert!(e.is_dir());
    assert_eq!(e.time, DateTime::ymd_hm(2004, 8, 19, 16, 40));
    let e = one("  2048  Aug 19, 2004 16:40 notes.txt");
    assert_eq!(e.size, 2048);
    assert!(!e.is_dir());
}

#[test]
fn add_a10_os2_three_digit_year() {
    let e = one("  5120      A    06-15-104  13:05  old-os2.txt");
    assert_eq!(e.time, DateTime::ymd_hm(2004, 6, 15, 13, 5));
    assert_eq!(e.size, 5120);
    assert_eq!(e.name, "old-os2.txt");
    let e = one("     0    DIR    12-31-99  23:59  OLDDIR");
    assert_eq!(e.time, DateTime::ymd_hm(1999, 12, 31, 23, 59));
}

// --- A11: VxWorks

#[test]
fn add_a11_vxworks_dir() {
    let e = one("   1536    Apr-09-2001  08:15:44   vx-dir   <DIR>");
    assert_eq!(e.name, "vx-dir");
    assert!(e.is_dir());
    assert_eq!(e.time, DateTime::ymd_hms(2001, 4, 9, 8, 15, 44));
}

#[test]
fn add_a11_vxworks_file() {
    let e = one("   1536    Apr-09-2001  08:15:44   vx-file");
    assert_eq!(e.name, "vx-file");
    assert!(!e.is_dir());
    assert_eq!(e.size, 1536);
    assert_eq!(e.time, DateTime::ymd_hms(2001, 4, 9, 8, 15, 44));
    let e = one("     12    Dec-31-1999  23:59:59   with space.txt");
    assert_eq!(e.name, "with space.txt");
}

// --- A12: Nortel WfFtp

#[test]
fn add_a12_nortel() {
    let e = one("router.cfg      20480  02/17/05  Thur.   16:44:10");
    assert_eq!(
        e,
        DirEntry {
            name: "router.cfg".into(),
            size: 20480,
            time: DateTime::ymd_hms(2005, 2, 17, 16, 44, 10),
            ..Default::default()
        }
    );
    let e = one("boot.img  1024  11/30/99  Tue.  01:02:03");
    assert_eq!(e.time, DateTime::ymd_hms(1999, 11, 30, 1, 2, 3));
    none("boot.img  1024  11/30/99  Xyz.  01:02:03");
}

// --- A13–A16: OpenVMS

#[test]
fn add_a13_vms_no_time_bare_owner() {
    let e = one("NOTES.TXT;2    3/4    9-JUL-2011    OPSUSER   (RWED,RWED,RE,)");
    assert_eq!(e.name, "NOTES.TXT;2");
    assert_eq!(e.size, 1536);
    assert_eq!(e.time, DateTime::ymd(2011, 7, 9));
    assert_eq!(e.time.accuracy(), Accuracy::Days);
    assert_eq!(e.owner_group, "OPSUSER");
    assert_eq!(e.permissions, "RWED,RWED,RE,");
}

#[test]
fn add_a14_vms_owner_before_size() {
    let e = one("PLAN.DOC;1   [ADMIN]   2/3   12-OCT-2015 08:30  (RWE,RWE,RE,)");
    assert_eq!(e.size, 1024);
    assert_eq!(e.time, DateTime::ymd_hm(2015, 10, 12, 8, 30));
    assert_eq!(e.owner_group, "ADMIN");
    assert_eq!(e.permissions, "RWE,RWE,RE,");
}

#[test]
fn add_a15_vms_date_before_size() {
    let e = one("OLDLOG.LOG;5      3-FEB-1997 11:12:13     40/42     (RWED,RWED,R,)");
    assert_eq!(e.name, "OLDLOG.LOG;5");
    assert_eq!(e.size, 20480);
    assert_eq!(e.time, DateTime::ymd_hms(1997, 2, 3, 11, 12, 13));
}

#[test]
fn add_a16_vms_size_unit_and_fraction() {
    let e = one("SMALL.DAT;1 3KB 4-APR-2009 10:00:00.15");
    assert_eq!(e.size, 3072);
    assert_eq!(e.time.accuracy(), Accuracy::Milliseconds);
    assert_eq!(e.time.to_string(), "2009-04-04 10:00:00.150");
    // unrelated trailing words make it not a VMS line
    none("SMALL.DAT;1 3KB 4-APR-2009 10:00:00 %RMS-E-PRV, insufficient privilege");
}

// --- A17–A19: IBM MVS

#[test]
fn add_a17_mvs_unknown_used() {
    let e = one("VOLA01 3390   2016/09/12  1 ????  FB      80  6160  PS  OPS.DAILY.LOG");
    assert_eq!(
        e,
        DirEntry {
            name: "OPS.DAILY.LOG".into(),
            time: DateTime::ymd(2016, 9, 12),
            ..Default::default()
        }
    );
    let e = one("VOLA01 3390   2016/09/12  1 ++++  VB     255  6160  PS  OPS.PLUS");
    assert_eq!(e.name, "OPS.PLUS");
}

#[test]
fn add_a17_mvs_ext_used_run_together() {
    let e = one("VOLA02 3390   2016/09/12 812000 U 0 27998 PO OPS.LOADLIB");
    assert_eq!(e.name, "OPS.LOADLIB");
    assert!(e.is_dir());
    assert_eq!(e.size, -1);
    assert_eq!(e.time, DateTime::ymd(2016, 9, 12));
}

#[test]
fn add_a18_mvs_vsam() {
    let e = one("VOLA03 3390   VSAM OPS.MASTER.KSDS");
    assert_eq!(e, file("OPS.MASTER.KSDS"));
}

#[test]
fn add_a19_mvs_load_module() {
    let e = one_st(
        "PAYCALC 0001F4 00A00C  00 FO        31    ANY",
        ServerType::Mvs,
    );
    assert_eq!(e.name, "PAYCALC");
    assert_eq!(e.size, 500);
    assert!(!e.is_dir());
    let e = one_st(
        "TINYMOD 000040 000102  00 FO  RN    ANY    24",
        ServerType::Mvs,
    );
    assert_eq!(e.name, "TINYMOD");
    assert_eq!(e.size, 64);
    none_st(
        "PAYCALC 0001F4 00A00C  00 FO        31    ANY",
        ServerType::Unix,
    );
}

#[test]
fn add_a19_mvs_member_name_only() {
    let e = one_st("JCLLIB1", ServerType::Mvs);
    assert_eq!(e, file("JCLLIB1"));
    // mixed with a regular member line, both are returned under MVS
    let v = entries_st(
        b"PAYROLL   01.03 2021/11/02 2024/03/14 09:15   120   100     0 ALICE\nJCLLIB1\n",
        ServerType::Mvs,
    );
    assert_eq!(names(&v), ["PAYROLL", "JCLLIB1"]);
    // other server types do not take the bare name once a line matched
    let v = entries_st(
        b"PAYROLL   01.03 2021/11/02 2024/03/14 09:15   120   100     0 ALICE\nJCLLIB1\n",
        ServerType::Unix,
    );
    assert_eq!(names(&v), ["PAYROLL"]);
}

// --- A20–A22: MLSD

#[test]
fn add_a20_mlsd_tab_separator() {
    let e = one("type=file;size=99;modify=20200101120000;\tname with tab.txt");
    assert_eq!(e.name, "name with tab.txt");
    assert_eq!(e.size, 99);
    assert_eq!(e.time, DateTime::ymd_hms(2020, 1, 1, 12, 0, 0));
}

#[test]
fn add_a21_mlsd_without_final_semicolon() {
    let e = one("type=file;size=5 legacy.txt");
    assert_eq!(e.name, "legacy.txt");
    assert_eq!(e.size, 5);
    // a name that looks like facts is not mistaken for them
    let e = one("type=file;size=5; a=b;c=d e");
    assert_eq!(e.name, "a=b;c=d e");
    assert_eq!(e.size, 5);
    let e = one("type=dir;perm=el sub dir");
    assert_eq!(e.name, "sub dir");
    assert!(e.is_dir());
    // the legacy form needs a well-known fact
    none("foo=1;bar=2 something");
}

#[test]
fn add_a22_mlsd_name_with_dates() {
    let e = one("size=987654321;type=file;modify=20210315083000; backup 2021-03-15 08-30-00.tar");
    assert_eq!(e.name, "backup 2021-03-15 08-30-00.tar");
    assert_eq!(e.size, 987_654_321);
    assert_eq!(e.time, DateTime::ymd_hms(2021, 3, 15, 8, 30, 0));
    assert!(!e.is_dir());
}

#[test]
fn add_examples_stream_identically() {
    let mut data: Vec<u8> = Vec::new();
    for line in [
        &b"-rw-r--r--   1 ann  ann   812 4 17 06:05 numeric-md.txt"[..],
        b"drwxr-xr-x 4 uwe dev 2048 3. M\xc3\xa4r., 09:10 projekt",
        b"-rw-r--r--   1 lee   lee     777 5\x8c\x8e 21\x93\xfa 2006\x94\xf6 sjis-mdy.txt",
        b"   1536    Apr-09-2001  08:15:44   vx-dir   <DIR>",
        b"router.cfg      20480  02/17/05  Thur.   16:44:10",
        b"OLDLOG.LOG;5      3-FEB-1997 11:12:13     40/42     (RWED,RWED,R,)",
        b"VOLA03 3390   VSAM OPS.MASTER.KSDS",
        b"type=file;size=5 legacy.txt",
    ] {
        data.extend_from_slice(line);
        data.extend_from_slice(b"\r\n");
    }
    let whole = parse_listing(&data, ServerType::Unix);
    assert_eq!(whole.len(), 8, "{:?}", dumps(&whole));
    let mut p = ListingParser::new(ServerType::Unix);
    for b in &data {
        p.feed(std::slice::from_ref(b));
    }
    assert_eq!(p.parse("").entries, whole.entries);
}
