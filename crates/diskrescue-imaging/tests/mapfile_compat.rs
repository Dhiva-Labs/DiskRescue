//! Mapfile compatibility with GNU ddrescue (test 8 of the imaging brief):
//! the two mapfile examples of `docs/specs/imaging-engine.md` (embedded
//! verbatim, the spec's `<-` annotations included) and a hand-written file
//! with hex/octal/suffixed numbers parse, and our writer reproduces
//! ddrescue's exact column layout.

use diskrescue_core::time::Timestamp;
use diskrescue_core::Range;
use diskrescue_imaging::{BlockStatus, CurrentStatus, MapFile};

/// Spec section 2.1.3, verbatim.
const SPEC_EXAMPLE_WRITER: &str = r##"# Mapfile. Created by GNU ddrescue version 1.28            <- "# %s. Created by %s version %s"
# Command line: ddrescue -d -c18 /dev/fd0 fdimage mapfile
# Start time:   2015-07-21 09:37:44                        <- "%Y-%m-%d %H:%M:%S" local time
# Current time: 2015-07-21 09:38:19                        <- written on every save
# Copying non-tried blocks... Pass 1 (forwards)            <- current_msg (screen status), optional
# current_pos  current_status  current_pass
0x00120000     ?               1
#      pos        size  status
0x00000000  0x00117000  +
0x00117000  0x00000200  -
0x00117200  0x00001000  /
0x00118200  0x00007E00  *
0x00120000  0x00048000  ?
"##;

/// Spec section 5 (fault map), verbatim.
const SPEC_EXAMPLE_FAULTS: &str = r##"# faults: one bad cluster, one 3 MiB bad area with a good island, one slow zone
0x00000000 ? 1
0x00000000  0x00100000  +
0x00100000  0x00000200  -          # single bad sector -> must end as one 512 B '-' block
0x00100200  0x002FFE00  +
0x00400000  0x00100000  -          # 1 MiB bad
0x00500000  0x00001000  +          # 4 KiB good island (scraping must find it)
0x00501000  0x00200000  -
0x00701000  0x00400000  /          # slow (3 s per read): pass 1 must skip it, pass 3 must come back
0x00B01000  0x034FF000  +
"##;

const HAND_WRITTEN: &str = "# hand-written: decimal, hex, octal and suffixed numbers\r
1Mi  *  3\r
0          1Mi       +\r
1Mi        0x200     -     # one bad sector\r
0x100200   01000     /\r
1049600    4Ki       /\r
\t0x101400 0xFEC00   ?\r
2Mi        1000k     -\r
3097152    0x10BDC0  +\r
";

fn blocks(m: &MapFile) -> Vec<(u64, u64, char)> {
    m.map
        .iter()
        .map(|b| (b.pos, b.size, b.status.as_char()))
        .collect()
}

fn assert_lines_eq(got: &str, want: &str) {
    let g: Vec<&str> = got.lines().collect();
    let w: Vec<&str> = want.lines().collect();
    for (i, (a, b)) in g.iter().zip(&w).enumerate() {
        assert_eq!(a, b, "line {}", i + 1);
    }
    assert_eq!(g.len(), w.len(), "line count\n{got}");
    assert_eq!(got, want);
}

#[test]
fn spec_writer_example_parses_and_is_reproduced_line_by_line() {
    let m = MapFile::parse(SPEC_EXAMPLE_WRITER).unwrap();
    assert_eq!(m.current_pos, 0x120000);
    assert_eq!(m.current_status, CurrentStatus::Copying);
    assert_eq!(m.current_pass, 1);
    assert_eq!(
        m.command_line.as_deref(),
        Some("ddrescue -d -c18 /dev/fd0 fdimage mapfile")
    );
    assert_eq!(
        blocks(&m),
        vec![
            (0x0, 0x117000, '+'),
            (0x117000, 0x200, '-'),
            (0x117200, 0x1000, '/'),
            (0x118200, 0x7E00, '*'),
            (0x120000, 0x48000, '?'),
        ]
    );
    assert_eq!(m.size_hint, Some(0x168000));
    assert!(m
        .comments
        .iter()
        .any(|c| c.starts_with("Copying non-tried blocks... Pass 1")));
    let st = m.map.stats();
    assert_eq!(
        (st.rescued, st.bad_sectors, st.bad_areas),
        (0x117000, 0x200, 1)
    );

    // ddrescue's own writer output for this state (we do not write the
    // optional status-message comment line).
    let mut m2 = m.clone();
    m2.start_time = Some(Timestamp::from_unix(1_437_471_464));
    let text = m2.to_text(
        "GNU ddrescue",
        "1.28",
        None,
        Timestamp::from_unix(1_437_471_499),
    );
    let want = "\
# Mapfile. Created by GNU ddrescue version 1.28
# Command line: ddrescue -d -c18 /dev/fd0 fdimage mapfile
# Start time:   2015-07-21 09:37:44
# Current time: 2015-07-21 09:38:19
# current_pos  current_status  current_pass
0x00120000     ?               1
#      pos        size  status
0x00000000  0x00117000  +
0x00117000  0x00000200  -
0x00117200  0x00001000  /
0x00118200  0x00007E00  *
0x00120000  0x00048000  ?
";
    assert_lines_eq(&text, want);
    // The data lines are exactly the spec example's data lines.
    for line in want.lines().filter(|l| !l.starts_with('#')) {
        assert!(SPEC_EXAMPLE_WRITER.lines().any(|l| l == line), "{line}");
    }
    assert_eq!(MapFile::parse(&text).unwrap().map, m.map);
}

#[test]
fn spec_fault_map_example_parses_and_roundtrips() {
    let m = MapFile::parse(SPEC_EXAMPLE_FAULTS).unwrap();
    assert_eq!(m.current_pos, 0);
    assert_eq!(m.current_pass, 1);
    assert_eq!(m.size_hint, Some(64 << 20));
    assert_eq!(
        m.comments,
        vec!["faults: one bad cluster, one 3 MiB bad area with a good island, one slow zone"]
    );
    assert_eq!(
        blocks(&m),
        vec![
            (0x0, 0x100000, '+'),
            (0x100000, 0x200, '-'),
            (0x100200, 0x2FFE00, '+'),
            (0x400000, 0x100000, '-'),
            (0x500000, 0x1000, '+'),
            (0x501000, 0x200000, '-'),
            (0x701000, 0x400000, '/'),
            (0xB01000, 0x34FF000, '+'),
        ]
    );
    assert_eq!(m.map.stats().bad_areas, 3);
    let now = Timestamp::from_unix(1_760_000_000);
    let text = m.to_text("DiskRescue", "0.1.0", Some("diskrescue image a b"), now);
    let want = "\
# Mapfile. Created by DiskRescue version 0.1.0
# Command line: diskrescue image a b
# Start time:   2025-10-09 08:53:20
# Current time: 2025-10-09 08:53:20
# current_pos  current_status  current_pass
0x00000000     ?               1
#      pos        size  status
0x00000000  0x00100000  +
0x00100000  0x00000200  -
0x00100200  0x002FFE00  +
0x00400000  0x00100000  -
0x00500000  0x00001000  +
0x00501000  0x00200000  -
0x00701000  0x00400000  /
0x00B01000  0x034FF000  +
";
    assert_lines_eq(&text, want);
    let back = MapFile::parse(&text).unwrap();
    assert_eq!(back.map, m.map);
    assert_eq!(back.command_line.as_deref(), Some("diskrescue image a b"));
}

#[test]
fn hand_written_numbers_and_merging() {
    let m = MapFile::parse(HAND_WRITTEN).unwrap();
    assert_eq!(m.current_pos, 1 << 20);
    assert_eq!(m.current_status, CurrentStatus::Trimming);
    assert_eq!(m.current_pass, 3);
    let text = m.to_text("DiskRescue", "0.1.0", None, Timestamp::from_unix(0));
    let want = "\
# Mapfile. Created by DiskRescue version 0.1.0
# Command line:\x20
# Start time:   1970-01-01 00:00:00
# Current time: 1970-01-01 00:00:00
# current_pos  current_status  current_pass
0x00100000     *               3
#      pos        size  status
0x00000000  0x00100000  +
0x00100000  0x00000200  -
0x00100200  0x00001200  /
0x00101400  0x000FEC00  ?
0x00200000  0x000F4240  -
0x002F4240  0x0010BDC0  +
";
    assert_lines_eq(&text, want);
    assert_eq!(MapFile::parse(&text).unwrap().map, m.map);
}

#[test]
fn wide_positions_use_more_than_eight_digits() {
    let size = 0x1D1C0BEC000u64; // about 2 TB
    let mut m = MapFile::new(size);
    m.map
        .set(Range::new(0x1D1C0BEB000, size), BlockStatus::Finished);
    m.current_pos = 0x1D1C0BEB000;
    let text = m.to_text("DiskRescue", "0.1.0", None, Timestamp::from_unix(0));
    assert!(
        text.contains("\n0x1D1C0BEB000     ?               1\n"),
        "{text}"
    );
    assert!(text.contains("\n0x00000000  0x1D1C0BEB000  ?\n"), "{text}");
    assert!(text.contains("\n0x1D1C0BEB000  0x00001000  +\n"), "{text}");
    assert_eq!(MapFile::parse(&text).unwrap().map, m.map);
}

#[test]
fn older_and_odd_mapfiles_are_accepted() {
    // Logfile header, no pass field, decimal numbers, comments after
    // whitespace, tabs, CRLF, an unknown size written as -1.
    let text = "# Rescue Logfile. Created by GNU ddrescue version 1.16\r\n\
                # current_pos  current_status\r\n\
                4096\t+\r\n\
                #      pos        size  status\r\n\
                0  4096  +   # first sector\r\n\
                4096\t-1\t?\r\n";
    let m = MapFile::parse(text).unwrap();
    assert_eq!(m.current_pass, 1);
    assert_eq!(m.current_status, CurrentStatus::Finished);
    assert!(m.size_is_unknown());
    assert_eq!(m.map.get(0), Some(BlockStatus::Finished));
    assert_eq!(m.map.get(4096), Some(BlockStatus::NonTried));
}
