//! `diskrescue carve`: search the raw data only.

use std::sync::Arc;

use diskrescue_recover::CarvePolicy;

use crate::args::CarveArgs;
use crate::args::StateArgs;
use crate::cmd::recover::{run_recovery, RecoveryJob};
use crate::cmd::selection;
use crate::error::CmdResult;
use crate::signal::Interrupt;
use crate::util;

/// Run the command.
pub fn run(args: &CarveArgs, interrupt: &Arc<Interrupt>) -> CmdResult {
    let mut sel = selection(&args.categories, &StateArgs::default());
    let (carve, use_filesystems) = if args.free_only {
        // Read the filesystems only to learn which space is free; do not
        // extract the files they list.
        sel.existing = false;
        sel.deleted = false;
        (CarvePolicy::FreeSpace, true)
    } else {
        (CarvePolicy::Everything, false)
    };
    let job = RecoveryJob {
        source: &args.source,
        output: &args.output,
        map: args.map.as_deref(),
        selection: sel,
        carve,
        use_filesystems,
        repair: &args.repair,
        jobs: args.jobs.filter(|j| *j > 0),
        allow_mounted: args.allow_mounted,
        json: args.json,
        image_to: None,
        command_line: util::command_line(),
    };
    run_recovery(&job, interrupt)
}
