//! What the app asks of the system, behind a trait so that tests can show
//! made-up drives and safety-check results without touching real devices.

use std::path::Path;

use diskrescue_app::jobs::ImageState;
use diskrescue_app::plan::{ImagePlan, RecoverPlan};
use diskrescue_app::preflight::{self, Preflight, ResolvedRecover};
use diskrescue_app::{helper, JobHandle};
use diskrescue_core::device::DeviceInfo;

/// Device listing, permission probe and safety checks.
pub trait Backend: Send + Sync + 'static {
    /// Disks, cards and USB drives (virtual devices left out).
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, String>;

    /// True when this user cannot read `path` without administrator rights.
    fn needs_privilege(&self, path: &Path) -> bool;

    /// The safety checks of a recovery plan.
    fn preflight_recover(&self, plan: &RecoverPlan) -> Preflight<ResolvedRecover>;

    /// Copy a device that needs administrator rights (Linux: the
    /// command-line program under pkexec, which asks for a password).
    fn start_image_privileged(
        &self,
        plan: ImagePlan,
    ) -> diskrescue_app::Result<JobHandle<ImageState>> {
        helper::start_image_privileged(plan)
    }
}

/// The real system.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemBackend;

impl Backend for SystemBackend {
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, String> {
        diskrescue_app::devices::list(false).map_err(|e| e.message().to_string())
    }

    fn needs_privilege(&self, path: &Path) -> bool {
        diskrescue_app::helper::needs_privilege(path)
    }

    fn preflight_recover(&self, plan: &RecoverPlan) -> Preflight<ResolvedRecover> {
        preflight::preflight_recover(plan)
    }
}
