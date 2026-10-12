//! `diskrescue map`: show the progress map of an image.

use std::path::Path;

use diskrescue_core::util::human;
use diskrescue_imaging::{render_map, BlockStatus, MapFile, StatusMap};
use serde_json::json;

use crate::args::MapArgs;
use crate::error::{CliError, CmdResult};
use crate::term::{self, Paint};
use crate::util::{percent, plural};

/// Longest picture we draw, whatever the terminal says.
const MAX_WIDTH: usize = 240;

/// The mapfile to read for the argument: the argument itself, or
/// `<argument>.map` when the argument is an image that has one.
pub use diskrescue_app::source::resolve_mapfile as resolve;

fn row(p: &Paint, label: &str, status: BlockStatus, bytes: u64, total: u64) -> String {
    let glyph = status.as_char().to_string();
    let g = match status {
        BlockStatus::Finished => p.green(&glyph),
        BlockStatus::BadSector => p.red(&glyph),
        BlockStatus::NonTried => p.dim(&glyph),
        _ => p.yellow(&glyph),
    };
    format!(
        "  {g} {label:<24}{:>12}  {:>7}\n",
        human::bytes(bytes),
        percent(bytes, total)
    )
}

/// Render the numbers and the picture.
pub fn render(mf: &MapFile, source: &Path, width: usize, p: &Paint) -> String {
    let map: &StatusMap = &mf.map;
    let st = map.stats();
    let total = st.total();
    let mut out = String::new();
    out.push_str(&format!("{} {}\n", p.bold("Mapfile:"), source.display()));
    if total == 0 {
        out.push_str("  The mapfile does not describe any data yet.\n");
        return out;
    }
    out.push_str(&format!(
        "  Device size: {} ({} bytes)\n",
        human::bytes(total),
        human::count(total)
    ));
    out.push_str(&format!(
        "  Copied:      {} of {}\n\n",
        p.bold(&percent(st.rescued, total)),
        human::bytes(total)
    ));
    out.push_str(&row(p, "copied", BlockStatus::Finished, st.rescued, total));
    out.push_str(&row(
        p,
        "not read yet",
        BlockStatus::NonTried,
        st.non_tried,
        total,
    ));
    out.push_str(&row(
        p,
        "failed, to examine",
        BlockStatus::NonTrimmed,
        st.non_trimmed,
        total,
    ));
    out.push_str(&row(
        p,
        "failed, to retry",
        BlockStatus::NonScraped,
        st.non_scraped,
        total,
    ));
    out.push_str(&row(
        p,
        "unreadable",
        BlockStatus::BadSector,
        st.bad_sectors,
        total,
    ));
    if st.bad_sectors > 0 {
        out.push_str(&format!(
            "    in {}\n",
            plural(st.bad_areas, "area", "areas")
        ));
    }
    out.push('\n');
    let cols = width.clamp(10, MAX_WIDTH);
    let picture = render_map(map, cols);
    for ch in picture.chars() {
        let s = ch.to_string();
        out.push_str(&match BlockStatus::from_char(ch) {
            Some(BlockStatus::Finished) => p.green(&s),
            Some(BlockStatus::BadSector) => p.red(&s),
            Some(BlockStatus::NonTried) => p.dim(&s),
            Some(_) => p.yellow(&s),
            None => s,
        });
    }
    out.push('\n');
    out.push_str(&format!(
        "  each character is about {}; the worst status in it is shown\n",
        human::bytes(total / cols as u64)
    ));
    out.push_str(
        "  + copied   ? not read yet   * failed, to examine   / failed, to retry   - unreadable\n",
    );

    let unfinished: Vec<_> = map
        .blocks()
        .iter()
        .filter(|b| b.status != BlockStatus::Finished)
        .collect();
    if !unfinished.is_empty() {
        out.push('\n');
        out.push_str(&p.bold("Areas that are not copied"));
        out.push('\n');
        let shown = unfinished.len().min(10);
        for b in unfinished.iter().take(shown) {
            out.push_str(&format!(
                "  {:#012x}  {:>10}  {}\n",
                b.pos,
                human::bytes(b.size),
                b.status.name()
            ));
        }
        if unfinished.len() > shown {
            out.push_str(&format!("  ... and {} more\n", unfinished.len() - shown));
        }
    }
    if let Some(c) = &mf.command_line {
        out.push_str(&format!("\nMade by: {c}\n"));
    }
    out
}

/// Run the command.
pub fn run(args: &MapArgs) -> CmdResult {
    let path = resolve(&args.mapfile);
    if !path.exists() {
        return Err(CliError::failed(format!(
            "{} does not exist. The progress map is the file next to the image, named IMAGE.map.",
            path.display()
        )));
    }
    let mf = MapFile::load(&path).map_err(|e| {
        CliError::failed(format!("{} is not a readable mapfile: {e}", path.display()))
    })?;
    let st = mf.map.stats();
    if args.json {
        let doc = json!({
            "mapfile": path,
            "total": st.total(),
            "rescued": st.rescued,
            "non_tried": st.non_tried,
            "non_trimmed": st.non_trimmed,
            "non_scraped": st.non_scraped,
            "bad": st.bad_sectors,
            "bad_areas": st.bad_areas,
            "percent_rescued": st.rescued_fraction() * 100.0,
            "current_pos": mf.current_pos,
            "current_status": mf.current_status.as_char().to_string(),
            "current_pass": mf.current_pass,
            "command_line": mf.command_line,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&doc)
                .map_err(|e| CliError::failed(format!("cannot format the result: {e}")))?
        );
        return Ok(0);
    }
    let width = args.width.unwrap_or_else(term::width);
    print!("{}", render(&mf, &path, width, &term::out()));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP: &str = "# Mapfile. Created by GNU ddrescue version 1.27\n\
# Command line: ddrescue -d /dev/sdb card.img card.map\n\
# Start time:   2024-05-01 10:00:00\n\
# Current time: 2024-05-01 10:05:00\n\
# Copying non-tried blocks... Pass 1 (forwards)\n\
# current_pos  current_status  current_pass\n\
0x00000200     ?               1\n\
#      pos        size  status\n\
0x00000000  0x00000190  +\n\
0x00000190  0x00000064  -\n\
0x000001F4  0x000001F4  ?\n";

    #[test]
    fn percentages_from_a_hand_written_mapfile() {
        let mf = MapFile::parse(MAP).unwrap();
        let s = render(&mf, Path::new("card.map"), 50, &Paint::plain());
        assert!(s.contains("Copied:      40.0% of 1000 B"), "{s}");
        assert!(s.contains("copied") && s.contains("40.0%"), "{s}");
        assert!(s.contains("unreadable") && s.contains("10.0%"), "{s}");
        assert!(s.contains("not read yet") && s.contains("50.0%"), "{s}");
        assert!(s.contains("in 1 area"), "{s}");
        assert!(s.contains("Made by: ddrescue -d /dev/sdb"), "{s}");
        // The picture is exactly as wide as asked.
        let pic = s
            .lines()
            .find(|l| l.starts_with('+') || l.starts_with('?') || l.starts_with('-'))
            .unwrap();
        assert_eq!(pic.chars().count(), 50);
    }

    #[test]
    fn resolve_prefers_the_map_next_to_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.img");
        std::fs::write(&img, b"x").unwrap();
        assert_eq!(resolve(&img), img);
        let map = dir.path().join("a.img.map");
        std::fs::write(&map, MAP).unwrap();
        assert_eq!(resolve(&img), map);
        assert_eq!(resolve(&map), map);
    }

    #[test]
    fn empty_map_is_explained() {
        let mf = MapFile::new(0);
        let s = render(&mf, Path::new("x.map"), 40, &Paint::plain());
        assert!(s.contains("does not describe any data"), "{s}");
    }
}
