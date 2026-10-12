//! GNU ddrescue mapfile (logfile) reader and writer.
//!
//! The format is specified in `docs/specs/imaging-engine.md` section 2.1
//! (ddrescue 1.28 `mapfile.cc`). Summary:
//!
//! ```text
//! # Mapfile. Created by GNU ddrescue version 1.28
//! # Command line: ddrescue -d /dev/sdb disk.img disk.map
//! # Start time:   2015-07-21 09:37:44
//! # Current time: 2015-07-21 09:38:19
//! # current_pos  current_status  current_pass
//! 0x00120000     ?               1
//! #      pos        size  status
//! 0x00000000  0x00117000  +
//! 0x00117000  0x00000200  -
//! ```
//!
//! Reading follows ddrescue's rules: `#` at the start of a line or after
//! whitespace starts a comment; blank lines are ignored; the first data line
//! is the status line `pos status [pass]` (pass defaults to 1); every other
//! data line is a block `pos size status`. Numbers are C integer literals
//! (decimal, `0x` hex, leading-`0` octal), positions and sizes are bytes,
//! and blocks must be contiguous and ascending. As an extension for
//! hand-edited files the parser also accepts ddrescue's command-line
//! multiplier suffixes (`k`=1000, `Ki`=1024, `M`, `Mi`, ... `E`, `Ei`, an
//! optional trailing `B`) and a size of `-1` meaning "up to the unknown end".
//! A first block that starts after 0 is preceded by a non-tried block;
//! adjacent blocks with the same status are merged. A file with no status
//! line (empty or comments only) yields an empty, all-non-tried map.
//!
//! Writing produces exactly ddrescue's layout (`0x%08llX` columns) so
//! `ddrescue`, `ddrescuelog` and `ddrescueview` read our files and a rescue
//! can be continued with either tool. Times are written in UTC (ddrescue
//! writes local time; the field is informational and never parsed).
//!
//! [`MapFile::save_atomic`] writes `<map>.tmp`, fsyncs it, keeps the
//! previous map as `<map>.bak` (best effort, like ddrescue) and renames the
//! temporary file over the map, then fsyncs the directory, so a crash leaves
//! either the old or the new map, never a torn one.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use diskrescue_core::time::Timestamp;
use diskrescue_core::{Error, Result};

use crate::status::{BlockStatus, MapBlock, StatusMap};

/// Largest position or end a mapfile can express (`LLONG_MAX`). A last block
/// ending exactly here means "input size unknown".
pub const UNKNOWN_SIZE_END: u64 = i64::MAX as u64;

/// Mapfiles larger than this are rejected rather than read into memory.
const MAX_MAPFILE_BYTES: u64 = 1 << 30;

/// The phase recorded in the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum CurrentStatus {
    /// `?` copying non-tried blocks.
    Copying,
    /// `*` trimming non-trimmed blocks.
    Trimming,
    /// `/` scraping non-scraped blocks (DiskRescue's dividing pass is
    /// recorded as scraping pass 1, the sector scrape as pass 2).
    Scraping,
    /// `-` retrying bad sectors.
    Retrying,
    /// `F` filling (ddrescue fill mode; never written by DiskRescue).
    Filling,
    /// `G` generating an approximate mapfile (ddrescue generate mode).
    Generating,
    /// `+` finished.
    Finished,
}

impl CurrentStatus {
    /// The status-line character.
    pub fn as_char(self) -> char {
        match self {
            CurrentStatus::Copying => '?',
            CurrentStatus::Trimming => '*',
            CurrentStatus::Scraping => '/',
            CurrentStatus::Retrying => '-',
            CurrentStatus::Filling => 'F',
            CurrentStatus::Generating => 'G',
            CurrentStatus::Finished => '+',
        }
    }

    /// Parse a status-line character.
    pub fn from_char(c: char) -> Option<Self> {
        Some(match c {
            '?' => CurrentStatus::Copying,
            '*' => CurrentStatus::Trimming,
            '/' => CurrentStatus::Scraping,
            '-' => CurrentStatus::Retrying,
            'F' => CurrentStatus::Filling,
            'G' => CurrentStatus::Generating,
            '+' => CurrentStatus::Finished,
            _ => return None,
        })
    }
}

/// A parsed (or to-be-written) ddrescue mapfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapFile {
    /// Extent covered by the blocks in the file (`None` when the file had
    /// no blocks). [`UNKNOWN_SIZE_END`] means ddrescue did not know the
    /// input size.
    pub size_hint: Option<u64>,
    /// Position being worked on (block start in a forward pass, block end
    /// in a backward pass).
    pub current_pos: u64,
    /// Phase being worked on.
    pub current_status: CurrentStatus,
    /// Pass number inside the phase (>= 1).
    pub current_pass: u32,
    /// The block map.
    pub map: StatusMap,
    /// The `# Command line:` header, if any.
    pub command_line: Option<String>,
    /// Full-line comments found while parsing (without the leading `#`,
    /// trimmed), excluding the standard header lines that
    /// [`MapFile::to_text`] regenerates. They are informational and are
    /// not written back.
    pub comments: Vec<String>,
    /// Start of the run that last wrote this map (written as
    /// `# Start time:`); `None` after parsing. Not part of ddrescue's
    /// parsed state.
    pub start_time: Option<Timestamp>,
}

impl Default for MapFile {
    fn default() -> Self {
        MapFile::new(0)
    }
}

impl MapFile {
    /// A fresh mapfile for `size` bytes: everything non-tried, copying pass 1
    /// at position 0.
    pub fn new(size: u64) -> Self {
        MapFile::from_map(StatusMap::new(size))
    }

    /// Wrap an existing map with a fresh status line.
    pub fn from_map(map: StatusMap) -> Self {
        MapFile {
            size_hint: (map.size() > 0).then_some(map.size()),
            current_pos: 0,
            current_status: CurrentStatus::Copying,
            current_pass: 1,
            map,
            command_line: None,
            comments: Vec::new(),
            start_time: None,
        }
    }

    /// True when the last block ends at [`UNKNOWN_SIZE_END`] (ddrescue's way
    /// of saying the input size was unknown).
    pub fn size_is_unknown(&self) -> bool {
        self.size_hint == Some(UNKNOWN_SIZE_END)
    }

    /// Parse mapfile text. See the module documentation for the grammar.
    pub fn parse(text: &str) -> Result<Self> {
        let mut mf = MapFile::new(0);
        mf.size_hint = None;
        let mut have_status = false;
        let mut blocks: Vec<MapBlock> = Vec::new();
        for (idx, raw) in text.split('\n').enumerate() {
            let lineno = idx + 1;
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            let (content, comment) = split_comment(line);
            let tokens: Vec<&str> = content.split_whitespace().collect();
            if tokens.is_empty() {
                if let Some(c) = comment {
                    mf.record_comment(c);
                }
                continue;
            }
            if !have_status {
                mf.parse_status_line(&tokens, lineno)?;
                have_status = true;
                continue;
            }
            if tokens.len() < 3 {
                return Err(bad_line(
                    lineno,
                    format!(
                        "expected '<pos> <size> <status>', found {:?}",
                        content.trim()
                    ),
                ));
            }
            let pos = parse_number(tokens[0])
                .map_err(|e| bad_line(lineno, format!("bad position {:?}: {e}", tokens[0])))?;
            let status = parse_status_char(tokens[2])
                .and_then(BlockStatus::from_char)
                .ok_or_else(|| {
                    bad_line(
                        lineno,
                        format!(
                            "bad block status {:?} (expected one of ? * / - +)",
                            tokens[2]
                        ),
                    )
                })?;
            let mut size = if tokens[1] == "-1" {
                UNKNOWN_SIZE_END.saturating_sub(pos)
            } else {
                parse_number(tokens[1])
                    .map_err(|e| bad_line(lineno, format!("bad size {:?}: {e}", tokens[1])))?
            };
            if size == 0 {
                if pos == 0 && blocks.is_empty() {
                    // ddrescue's representation of an empty map.
                    continue;
                }
                return Err(bad_line(lineno, "block of size 0".to_string()));
            }
            // ddrescue's Block::fix_size: pos + size never exceeds LLONG_MAX.
            if pos.saturating_add(size) > UNKNOWN_SIZE_END {
                size = UNKNOWN_SIZE_END - pos;
                if size == 0 {
                    return Err(bad_line(
                        lineno,
                        "block starts at the maximum position".into(),
                    ));
                }
            }
            if let Some(prev) = blocks.last() {
                let prev_end = prev.end();
                if pos < prev_end {
                    return Err(bad_line(
                        lineno,
                        format!(
                            "block at 0x{pos:X} overlaps the previous block, which ends at \
                             0x{prev_end:X} (blocks must be contiguous and ascending)"
                        ),
                    ));
                }
                if pos > prev_end {
                    return Err(bad_line(
                        lineno,
                        format!(
                            "gap between 0x{prev_end:X} and 0x{pos:X}: blocks are not contiguous"
                        ),
                    ));
                }
            } else if pos > 0 {
                blocks.push(MapBlock {
                    pos: 0,
                    size: pos,
                    status: BlockStatus::NonTried,
                });
            }
            blocks.push(MapBlock { pos, size, status });
        }
        mf.map = StatusMap::from_contiguous(blocks);
        mf.size_hint = (mf.map.size() > 0).then_some(mf.map.size());
        Ok(mf)
    }

    fn parse_status_line(&mut self, tokens: &[&str], lineno: usize) -> Result<()> {
        if tokens.len() < 2 {
            return Err(bad_line(
                lineno,
                "bad or missing status line (expected '<pos> <status> [pass]')".into(),
            ));
        }
        let pos = parse_number(tokens[0])
            .map_err(|e| bad_line(lineno, format!("bad current position {:?}: {e}", tokens[0])))?;
        let status = parse_status_char(tokens[1])
            .and_then(CurrentStatus::from_char)
            .ok_or_else(|| {
                bad_line(
                    lineno,
                    format!(
                        "bad or missing status line: status {:?} is not one of ? * / - F G +",
                        tokens[1]
                    ),
                )
            })?;
        let pass = match tokens.get(2) {
            None => 1,
            Some(t) => match t.parse::<u32>() {
                Ok(p) if p >= 1 && p <= i32::MAX as u32 => p,
                _ => {
                    return Err(bad_line(
                        lineno,
                        format!("bad current pass {t:?} (expected a decimal number >= 1)"),
                    ))
                }
            },
        };
        self.current_pos = pos;
        self.current_status = status;
        self.current_pass = pass;
        Ok(())
    }

    fn record_comment(&mut self, comment: &str) {
        let c = comment.trim();
        if let Some(cl) = c.strip_prefix("Command line:") {
            let cl = cl.trim();
            if !cl.is_empty() {
                self.command_line = Some(cl.to_string());
            }
            return;
        }
        let standard = c.is_empty()
            || c.starts_with("Mapfile. Created by")
            || c.starts_with("Logfile. Created by")
            || c.starts_with("Start time:")
            || c.starts_with("Current time:")
            || c.starts_with("current_pos")
            || (c.starts_with("pos") && c.ends_with("status"));
        if !standard && self.comments.len() < 1000 {
            self.comments.push(c.to_string());
        }
    }

    /// Render in ddrescue's exact layout. `cmdline` overrides
    /// [`MapFile::command_line`] for the `# Command line:` header. The start
    /// time is [`MapFile::start_time`] (or `now` when unset).
    pub fn to_text(
        &self,
        tool_name: &str,
        version: &str,
        cmdline: Option<&str>,
        now: Timestamp,
    ) -> String {
        let mut s = String::with_capacity(320 + self.map.len() * 32);
        let _ = writeln!(
            s,
            "# Mapfile. Created by {} version {}",
            one_line(tool_name),
            one_line(version)
        );
        if let Some(c) = cmdline.or(self.command_line.as_deref()) {
            if !c.trim().is_empty() {
                let _ = writeln!(s, "# Command line: {}", one_line(c));
            }
        }
        let _ = writeln!(
            s,
            "# Start time:   {}",
            format_time(self.start_time.unwrap_or(now))
        );
        let _ = writeln!(s, "# Current time: {}", format_time(now));
        s.push_str("# current_pos  current_status  current_pass\n");
        let _ = writeln!(
            s,
            "0x{:08X}     {}               {}",
            self.current_pos,
            self.current_status.as_char(),
            self.current_pass.clamp(1, i32::MAX as u32)
        );
        s.push_str("#      pos        size  status\n");
        for b in self.map.iter() {
            let _ = writeln!(
                s,
                "0x{:08X}  0x{:08X}  {}",
                b.pos,
                b.size,
                b.status.as_char()
            );
        }
        s
    }

    /// Read and parse a mapfile. A missing file is an error here (use
    /// `Path::exists` first when "absent means new" is wanted).
    pub fn load(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path).map_err(|e| io_ctx(e, path))?;
        if meta.len() > MAX_MAPFILE_BYTES {
            return Err(Error::corrupt(
                "mapfile",
                format!(
                    "{} is {} bytes; refusing to load a mapfile larger than {} bytes",
                    path.display(),
                    meta.len(),
                    MAX_MAPFILE_BYTES
                ),
            ));
        }
        let bytes = fs::read(path).map_err(|e| io_ctx(e, path))?;
        let text = String::from_utf8_lossy(&bytes);
        MapFile::parse(&text).map_err(|e| match e {
            Error::Corrupt { what, detail } => Error::Corrupt {
                what,
                detail: format!("{}: {detail}", path.display()),
            },
            other => other,
        })
    }

    /// Write the map atomically (see the module documentation). The
    /// command-line header is [`MapFile::command_line`].
    pub fn save_atomic(
        &self,
        path: &Path,
        tool_name: &str,
        version: &str,
        now: Timestamp,
    ) -> Result<()> {
        let text = self.to_text(tool_name, version, None, now);
        write_atomic(path, text.as_bytes(), true)
    }
}

/// `<path><suffix>` (the suffix is appended to the file name).
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Write `bytes` to `path` via `<path>.tmp` + fsync + rename + directory
/// fsync. With `backup`, the previous file is kept as `<path>.bak` (hard
/// link; skipped silently where links are unsupported).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], backup: bool) -> Result<()> {
    let tmp = with_suffix(path, ".tmp");
    {
        let mut f = File::create(&tmp).map_err(|e| io_ctx(e, &tmp))?;
        f.write_all(bytes).map_err(|e| io_ctx(e, &tmp))?;
        f.sync_all().map_err(|e| io_ctx(e, &tmp))?;
    }
    if backup && path.exists() {
        let bak = with_suffix(path, ".bak");
        let _ = fs::remove_file(&bak);
        if let Err(e) = fs::hard_link(path, &bak) {
            log::debug!("could not keep {} as a backup: {e}", bak.display());
        }
    }
    fs::rename(&tmp, path).map_err(|e| io_ctx(e, path))?;
    sync_parent_dir(path);
    Ok(())
}

/// fsync the directory containing `path` so a rename is durable. Best
/// effort: failures are logged, and Windows has no equivalent.
pub(crate) fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    {
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        match File::open(&dir) {
            Ok(d) => {
                if let Err(e) = d.sync_all() {
                    log::debug!("fsync of directory {} failed: {e}", dir.display());
                }
            }
            Err(e) => log::debug!("cannot open directory {} for fsync: {e}", dir.display()),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

pub(crate) fn io_ctx(e: std::io::Error, path: &Path) -> Error {
    Error::Io(std::io::Error::new(
        e.kind(),
        format!("{}: {e}", path.display()),
    ))
}

fn bad_line(lineno: usize, detail: String) -> Error {
    Error::corrupt("mapfile", format!("line {lineno}: {detail}"))
}

/// Split a line into data and comment: `#` at the start or after
/// whitespace starts a comment (ddrescue `my_fgets`).
fn split_comment(line: &str) -> (&str, Option<&str>) {
    let mut prev_ws = true;
    for (i, c) in line.char_indices() {
        if c == '#' && prev_ws {
            return (&line[..i], Some(&line[i + 1..]));
        }
        prev_ws = c.is_whitespace();
    }
    (line, None)
}

/// The single status character of a token, or `None` when the token is
/// not exactly one character.
fn parse_status_char(token: &str) -> Option<char> {
    let mut it = token.chars();
    let c = it.next()?;
    if it.next().is_some() {
        return None;
    }
    Some(c)
}

/// Replace line breaks so a header value cannot break the file format.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

/// `YYYY-MM-DD HH:MM:SS` (UTC), ddrescue's timestamp layout.
pub fn format_time(ts: Timestamp) -> String {
    let (y, m, d, hh, mm, ss) = ts.civil();
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

/// Parse a mapfile number: decimal, `0x` hex or leading-`0` octal (C
/// `%lli`), optionally followed by a multiplier suffix (`k`/`K` = 1000,
/// `Ki` = 1024, `M`, `Mi`, `G`, `Gi`, `T`, `Ti`, `P`, `Pi`, `E`, `Ei`, with
/// an optional trailing `B`). The value must not exceed `i64::MAX`.
pub fn parse_number(token: &str) -> std::result::Result<u64, String> {
    let (radix, digits_start) = if token.starts_with("0x") || token.starts_with("0X") {
        (16u32, 2usize)
    } else if token.len() > 1 && token.starts_with('0') && token.as_bytes()[1].is_ascii_digit() {
        (8, 1)
    } else {
        (10, 0)
    };
    let rest = &token[digits_start..];
    let ndigits = rest
        .bytes()
        .take_while(|b| (*b as char).is_digit(radix))
        .count();
    if ndigits == 0 {
        return Err("no digits".into());
    }
    let (digits, suffix) = rest.split_at(ndigits);
    if radix == 8 && suffix.starts_with(['8', '9']) {
        return Err("invalid octal digit".into());
    }
    let mut value: u64 = 0;
    for b in digits.bytes() {
        let d = (b as char).to_digit(radix).ok_or("invalid digit")? as u64;
        value = value
            .checked_mul(radix as u64)
            .and_then(|v| v.checked_add(d))
            .ok_or("number too large")?;
    }
    let mult = parse_suffix(suffix)?;
    let v = value.checked_mul(mult).ok_or("number too large")?;
    if v > UNKNOWN_SIZE_END {
        return Err("number larger than 2^63-1".into());
    }
    Ok(v)
}

fn parse_suffix(s: &str) -> std::result::Result<u64, String> {
    if s.is_empty() {
        return Ok(1);
    }
    let core = s.strip_suffix('B').unwrap_or(s);
    if core.is_empty() {
        return Ok(1);
    }
    let bytes = core.as_bytes();
    let (letter, binary) = match bytes.len() {
        1 => (bytes[0], false),
        2 if bytes[1] == b'i' => (bytes[0], true),
        _ => return Err(format!("unknown suffix {s:?}")),
    };
    let exp = match letter.to_ascii_uppercase() {
        b'K' => 1,
        b'M' => 2,
        b'G' => 3,
        b'T' => 4,
        b'P' => 5,
        b'E' => 6,
        _ => return Err(format!("unknown suffix {s:?}")),
    };
    let base: u64 = if binary { 1024 } else { 1000 };
    base.checked_pow(exp)
        .ok_or_else(|| "suffix overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    #[test]
    fn numbers() {
        assert_eq!(parse_number("0"), Ok(0));
        assert_eq!(parse_number("123"), Ok(123));
        assert_eq!(parse_number("0x1F"), Ok(31));
        assert_eq!(parse_number("0X00117000"), Ok(0x117000));
        assert_eq!(parse_number("010"), Ok(8));
        assert!(parse_number("09").is_err());
        assert_eq!(parse_number("4k"), Ok(4000));
        assert_eq!(parse_number("4Ki"), Ok(4096));
        assert_eq!(parse_number("4KiB"), Ok(4096));
        assert_eq!(parse_number("3Mi"), Ok(3 << 20));
        assert_eq!(parse_number("2G"), Ok(2_000_000_000));
        assert_eq!(parse_number("0x10M"), Ok(16_000_000));
        assert_eq!(parse_number("512B"), Ok(512));
        assert_eq!(parse_number("7Ei"), Ok(7 << 60));
        assert!(parse_number("8Ei").is_err());
        assert!(parse_number("9223372036854775808").is_err());
        assert_eq!(parse_number("9223372036854775807"), Ok(i64::MAX as u64));
        assert!(parse_number("99999999999999999999999").is_err());
        assert!(parse_number("").is_err());
        assert!(parse_number("x").is_err());
        assert!(parse_number("0x").is_err());
        assert!(parse_number("12q").is_err());
        assert!(parse_number("-5").is_err());
        assert!(parse_number("1Kib").is_err());
    }

    #[test]
    fn comments_after_whitespace_only() {
        assert_eq!(split_comment("0x0 ? 1 # hi"), ("0x0 ? 1 ", Some(" hi")));
        assert_eq!(split_comment("# all"), ("", Some(" all")));
        assert_eq!(split_comment("0x10#x"), ("0x10#x", None));
    }

    #[test]
    fn empty_and_comment_only_are_all_non_tried() {
        for text in ["", "\n\n", "# just a comment\n", "  \t\n# x\n"] {
            let m = MapFile::parse(text).unwrap();
            assert!(m.map.is_empty());
            assert_eq!(m.size_hint, None);
            assert_eq!(m.current_status, CurrentStatus::Copying);
            assert_eq!(m.current_pass, 1);
        }
    }

    #[test]
    fn errors_are_reported_with_line_numbers() {
        let cases = [
            ("0x0 X 1\n", "line 1"),
            ("0x0\n", "line 1"),
            ("0x0 ? 0\n", "line 1"),
            ("0x0 ? 1\n0x0 0x10 +\n0x20 0x10 -\n", "not contiguous"),
            ("0x0 ? 1\n0x0 0x10 +\n0x8 0x10 -\n", "overlaps"),
            ("0x0 ? 1\n0x0 0x10 x\n", "line 2"),
            ("0x0 ? 1\n0x0 0x10\n", "line 2"),
            ("0x0 ? 1\n0x0 0x10 +\n0x10 0 +\n", "size 0"),
            ("0x0 ? 1\n0x10 0x10 F\n", "line 2"),
        ];
        for (text, needle) in cases {
            let err = MapFile::parse(text).unwrap_err().to_string();
            assert!(err.contains(needle), "{text:?}: {err}");
        }
    }

    #[test]
    fn merges_equal_neighbours_and_fills_leading_gap() {
        let m = MapFile::parse("0 + 2\n0x100 0x100 +\n0x200 0x100 +\n0x300 0x100 -\n").unwrap();
        let v: Vec<_> = m
            .map
            .iter()
            .map(|b| (b.pos, b.size, b.status.as_char()))
            .collect();
        assert_eq!(
            v,
            vec![(0, 0x100, '?'), (0x100, 0x200, '+'), (0x300, 0x100, '-')]
        );
        assert_eq!(m.current_status, CurrentStatus::Finished);
        assert_eq!(m.current_pass, 2);
        assert_eq!(m.size_hint, Some(0x400));
    }

    #[test]
    fn unknown_size_marker() {
        let m = MapFile::parse("0 ? 1\n0 0x1000 +\n0x1000 -1 ?\n").unwrap();
        assert!(m.size_is_unknown());
        let m2 = MapFile::parse("0 ? 1\n0 0x7FFFFFFFFFFFFFFF ?\n").unwrap();
        assert!(m2.size_is_unknown());
        // Oversized blocks are clamped like ddrescue's fix_size.
        let m3 = MapFile::parse("0 ? 1\n0x10 0x7FFFFFFFFFFFFFFF ?\n").unwrap();
        assert!(m3.size_is_unknown());
    }

    #[test]
    fn writer_layout_and_roundtrip() {
        let mut m = MapFile::new(0x10000);
        m.map.set(
            diskrescue_core::Range::new(0, 0x8000),
            BlockStatus::Finished,
        );
        m.current_pos = 0x8000;
        m.command_line = Some("diskrescue image /dev/sdb\nx.img".into());
        let now = Timestamp::from_unix(1_700_000_000);
        let t = m.to_text("DiskRescue", "0.1.0", None, now);
        let expected = "# Mapfile. Created by DiskRescue version 0.1.0\n\
                        # Command line: diskrescue image /dev/sdb x.img\n\
                        # Start time:   2023-11-14 22:13:20\n\
                        # Current time: 2023-11-14 22:13:20\n\
                        # current_pos  current_status  current_pass\n\
                        0x00008000     ?               1\n\
                        #      pos        size  status\n\
                        0x00000000  0x00008000  +\n\
                        0x00008000  0x00008000  ?\n";
        assert_eq!(t, expected);
        let back = MapFile::parse(&t).unwrap();
        assert_eq!(back.map, m.map);
        assert_eq!(back.current_pos, 0x8000);
        assert_eq!(
            back.command_line.as_deref(),
            Some("diskrescue image /dev/sdb x.img")
        );
    }

    #[test]
    fn save_atomic_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("disk.map");
        let mut m = MapFile::new(4096);
        m.map.set(
            diskrescue_core::Range::new(512, 1024),
            BlockStatus::BadSector,
        );
        m.save_atomic(&p, "DiskRescue", "0.1.0", Timestamp::from_unix(0))
            .unwrap();
        m.map
            .set(diskrescue_core::Range::new(0, 512), BlockStatus::Finished);
        m.save_atomic(&p, "DiskRescue", "0.1.0", Timestamp::from_unix(0))
            .unwrap();
        assert!(!with_suffix(&p, ".tmp").exists());
        let back = MapFile::load(&p).unwrap();
        assert_eq!(back.map, m.map);
        // The previous version is kept as .bak where hard links work.
        if let Ok(bak) = MapFile::load(&with_suffix(&p, ".bak")) {
            assert_eq!(bak.map.get(0), Some(BlockStatus::NonTried));
        }
        assert!(MapFile::load(&dir.path().join("missing.map")).is_err());
    }

    #[test]
    fn random_garbage_never_panics() {
        let mut rng = StdRng::seed_from_u64(77);
        let alphabet = b"0123456789abcdefxX?*/-+FG# \t\n\rkKMiB";
        for _ in 0..3000 {
            let len = rng.gen_range(0..200);
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    if rng.gen_bool(0.9) {
                        alphabet[rng.gen_range(0..alphabet.len())]
                    } else {
                        rng.gen()
                    }
                })
                .collect();
            let text = String::from_utf8_lossy(&bytes);
            if let Ok(m) = MapFile::parse(&text) {
                m.map.validate().unwrap();
                let again =
                    MapFile::parse(&m.to_text("t", "v", None, Timestamp::from_unix(0))).unwrap();
                assert_eq!(again.map, m.map);
            }
        }
        // Truncations of a valid file never panic either.
        let good = "# c\n0x100 * 3\n0x0 0x100 +\n0x100 0x200 *\n0x300 0x10 -\n";
        for cut in 0..good.len() {
            let _ = MapFile::parse(&good[..cut]);
        }
    }
}
