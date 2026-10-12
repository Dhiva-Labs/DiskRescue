//! DiskRescue recovery pipeline.
//!
//! Ties the parsers, the carver and the repair modules together:
//! [`pipeline::recover`] runs the whole thing on a source and writes an
//! organised output tree plus a report; [`pipeline::repair_files`] and
//! [`pipeline::verify_files`] work on loose files; [`pipeline::survey`]
//! lists what could be recovered without writing anything.

pub mod dedup;
pub mod donors;
pub mod filters;
pub mod fsopen;
pub mod naming;
pub mod output;
pub mod pipeline;
pub mod registry;
pub mod report;
pub mod safety;
pub mod sink;

pub use filters::Selection;
pub use pipeline::{
    recover, repair_files, survey, verify_files, CarvePolicy, Event, NoopObserver, Observer,
    RecoverOptions, Stage,
};
pub use report::{FileRecord, Outcome, Report};
