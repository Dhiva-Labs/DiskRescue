//! Text rendering of a status map, ddrescueview style, for the CLI/TUI.
//!
//! Each output character is one cell covering an equal slice of the map
//! (the last cells absorb the rounding). A cell shows the most alarming
//! status found in its slice, so a single bad sector in a huge cell is
//! still visible: bad sector `-` > non-scraped `/` > non-trimmed `*` >
//! non-tried `?` > finished `+`.

use diskrescue_core::Range;

use crate::status::{BlockStatus, StatusMap};

/// Render the whole map in `columns` cells. Empty for an empty map or zero
/// columns.
pub fn render_map(map: &StatusMap, columns: usize) -> String {
    render_window(map, Range::new(0, map.size()), columns)
}

/// Render the part of the map inside `window` in `columns` cells.
pub fn render_window(map: &StatusMap, window: Range, columns: usize) -> String {
    let start = window.start.min(map.size());
    let end = window.end.min(map.size());
    if columns == 0 || start >= end {
        return String::new();
    }
    let span = (end - start) as u128;
    let cols = columns as u128;
    let blocks = map.blocks();
    let mut bi = map.index_of(start).unwrap_or(blocks.len());
    let mut out = String::with_capacity(columns);
    for c in 0..cols {
        let cs = start + (c * span / cols) as u64;
        let mut ce = start + ((c + 1) * span / cols) as u64;
        if ce <= cs {
            ce = cs + 1;
        }
        let ce = ce.min(end);
        while bi < blocks.len() && blocks[bi].end() <= cs {
            bi += 1;
        }
        let mut worst: Option<BlockStatus> = None;
        let mut j = bi;
        while j < blocks.len() && blocks[j].pos < ce {
            let s = blocks[j].status;
            if worst.is_none_or(|w| s.severity() > w.severity()) {
                worst = Some(s);
            }
            j += 1;
        }
        out.push(worst.map_or(' ', BlockStatus::as_char));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    #[test]
    fn worst_status_wins() {
        let mut m = StatusMap::new(1000);
        m.set(Range::new(0, 500), BlockStatus::Finished);
        m.set(Range::new(100, 101), BlockStatus::BadSector);
        m.set(Range::new(700, 800), BlockStatus::NonScraped);
        assert_eq!(render_map(&m, 4), "-+//");
        assert_eq!(render_map(&m, 0), "");
        assert_eq!(render_map(&StatusMap::new(0), 10), "");
        assert_eq!(render_map(&m, 1), "-");
        assert_eq!(render_window(&m, Range::new(0, 500), 2), "-+");
    }

    #[test]
    fn more_columns_than_bytes() {
        let mut m = StatusMap::new(3);
        m.set(Range::new(1, 2), BlockStatus::Finished);
        let s = render_map(&m, 7);
        assert_eq!(s.chars().count(), 7);
        assert!(s.contains('+') && s.contains('?'));
        assert!(!s.contains(' '));
    }

    #[test]
    fn random_maps_render_exact_width() {
        let mut rng = StdRng::seed_from_u64(9);
        for _ in 0..300 {
            let size = rng.gen_range(1..100_000u64);
            let mut m = StatusMap::new(size);
            for _ in 0..rng.gen_range(0..30) {
                let a = rng.gen_range(0..size);
                let b = rng.gen_range(a..=size);
                m.set(Range::new(a, b), BlockStatus::ALL[rng.gen_range(0..5)]);
            }
            let cols = rng.gen_range(1..300);
            let s = render_map(&m, cols);
            assert_eq!(s.chars().count(), cols);
            assert!(!s.contains(' '));
            // A bad sector anywhere is visible somewhere.
            assert_eq!(s.contains('-'), m.stats().bad_sectors > 0);
        }
    }
}
