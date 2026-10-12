//! Shared front-end layer of DiskRescue.
//!
//! Everything a user interface needs that is not drawing: the command line
//! (`diskrescue`), the terminal app and the desktop app all use it, so they
//! refuse the same unsafe things with the same words and run the same
//! code.
//!
//! * [`devices`]: list and describe disks, cards and USB drives, with
//!   plain-language [`devices::advice`] (system disk, failed controller,
//!   SSD TRIM, mounted, read-only).
//! * [`source`]: open a device, partition or image file read-only (an
//!   image's mapfile is attached automatically).
//! * [`plan`] and [`preflight`]: plain descriptions of every operation and
//!   the checks that must pass before it starts, as a list of
//!   [`preflight::Blocker`]s with the [`preflight::Override`] that lifts
//!   each one (if any).
//! * [`jobs`]: run imaging, recovery, carving, repair, verification and
//!   scans on background threads; poll [`jobs::JobHandle::snapshot`].
//! * [`helper`]: image a device that needs administrator rights through
//!   the command line under `pkexec`.
//! * [`results`]: read a recovery report, list its files, make thumbnails.
//! * [`text`]: labels, tones and number formatting shared by every UI.
//! * [`imaging`], [`recovery`], [`json`], [`ownership`], [`util`]: the
//!   building blocks the command line drives directly.
//!
//! Rules: the source is never written to; everything is `Send + Sync`;
//! nothing is printed (messages go to the `log` crate or into job state).

#![warn(missing_docs)]

pub mod devices;
pub mod error;
pub mod helper;
pub mod imaging;
pub mod jobs;
pub mod json;
pub mod ownership;
pub mod plan;
pub mod preflight;
pub mod recovery;
pub mod results;
pub mod source;
pub mod text;
pub mod util;

pub use error::{AppError, Result};
pub use jobs::{JobHandle, JobOutcome, JobState};
pub use plan::{CarvePlan, ImagePlan, RecoverPlan, RepairPlan, ScanPlan, VerifyPlan};

#[cfg(test)]
mod tests {
    use super::*;

    fn send_sync<T: Send + Sync>() {}

    #[test]
    fn everything_a_ui_holds_is_send_and_sync() {
        send_sync::<JobHandle<jobs::ImageState>>();
        send_sync::<JobHandle<jobs::RecoverState>>();
        send_sync::<JobHandle<jobs::RepairState>>();
        send_sync::<JobHandle<jobs::VerifyState>>();
        send_sync::<JobHandle<jobs::ScanState>>();
        send_sync::<jobs::ImageState>();
        send_sync::<jobs::RecoverState>();
        send_sync::<preflight::Preflight<preflight::ResolvedImage>>();
        send_sync::<preflight::Preflight<preflight::ResolvedRecover>>();
        send_sync::<ImagePlan>();
        send_sync::<RecoverPlan>();
        send_sync::<CarvePlan>();
        send_sync::<RepairPlan>();
        send_sync::<VerifyPlan>();
        send_sync::<ScanPlan>();
        send_sync::<source::Opened>();
        send_sync::<imaging::Ready>();
        send_sync::<results::ResultItem>();
        send_sync::<devices::DeviceDetails>();
        send_sync::<AppError>();
    }
}
