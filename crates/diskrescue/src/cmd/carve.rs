//! `diskrescue carve`: search the raw data only.

use std::sync::Arc;

use crate::args::CarveArgs;
use crate::cmd::recover::{run_recovery, RecoveryJob};
use crate::error::CmdResult;
use crate::signal::Interrupt;
use crate::util;

/// Run the command.
pub fn run(args: &CarveArgs, interrupt: &Arc<Interrupt>) -> CmdResult {
    let command_line = util::command_line();
    let plan = diskrescue_app::plan::CarvePlan {
        source: args.source.clone(),
        output: args.output.clone(),
        categories: super::categories(&args.categories),
        free_only: args.free_only,
        repair: super::repair_options(&args.repair),
        jobs: args.jobs,
        map: args.map.clone(),
        allow_mounted: args.allow_mounted,
        command_line: Some(command_line.clone()),
    };
    let rj = RecoveryJob {
        job: plan.job(),
        no_tui: false,
        json: args.json,
        command_line,
    };
    run_recovery(&rj, interrupt)
}
