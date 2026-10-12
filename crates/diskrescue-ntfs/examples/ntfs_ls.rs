//! List the entries of an NTFS image: `cargo run -p diskrescue-ntfs
//! --example ntfs_ls -- IMAGE [--tree] [--live] [--extract ID OUT]`.
//!
//! `--tree` walks only the directory indexes, `--live` hides deleted
//! entries, `--extract` writes the content of the entry with the given id.

use std::sync::Arc;

use diskrescue_core::fs::{collect_entries, FileSystem};
use diskrescue_core::source::FileSource;
use diskrescue_ntfs::{open, NtfsOptions};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: ntfs_ls IMAGE [--tree] [--live] [--extract ID OUT]");
        std::process::exit(2);
    };
    let opts = NtfsOptions {
        walk_whole_mft: !args.iter().any(|a| a == "--tree"),
        include_deleted: !args.iter().any(|a| a == "--live"),
        ..Default::default()
    };
    let src = match FileSource::open(path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    let fs = match open(src, &opts) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("open: {e}");
            std::process::exit(1);
        }
    };
    let info = fs.info();
    println!(
        "label={:?} serial={:?} cluster={} size={} records={}",
        info.label,
        info.serial,
        info.cluster_size,
        info.total_size,
        fs.record_count()
    );
    for w in &info.warnings {
        println!("warning: {w}");
    }
    let entries = match collect_entries(&fs) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("walk: {e}");
            std::process::exit(1);
        }
    };
    if let Some(i) = args.iter().position(|a| a == "--extract") {
        let id: u64 = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let out = args.get(i + 2).cloned().unwrap_or_else(|| "out.bin".into());
        let Some(e) = entries.iter().find(|e| e.id == id) else {
            eprintln!("no entry {id}");
            std::process::exit(1);
        };
        let mut f = std::fs::File::create(&out).expect("create output");
        match fs.read_file(e, &mut f) {
            Ok(r) => println!("{r:?}"),
            Err(err) => eprintln!("read: {err}"),
        }
        return;
    }
    for e in &entries {
        println!(
            "{:>20} {:>10} {}{} {:?} {:?} extents={} {}",
            e.id,
            e.size,
            if e.is_dir { "d" } else { "-" },
            if e.deleted { "D" } else { "-" },
            e.confidence,
            e.flags,
            e.extents.len(),
            e.path
        );
    }
}
