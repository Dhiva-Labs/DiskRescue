//! DiskRescue desktop app: recover photos and videos from failing drives.
//!
//! Everything that is not drawing (devices, safety checks, background
//! jobs, the privileged imaging helper, results and thumbnails, labels)
//! comes from `diskrescue-app`; this crate draws it with egui and never
//! blocks its user interface thread: anything that may touch a disk runs
//! on a background thread and is polled every frame.

#![warn(missing_docs)]

pub mod app;
pub mod backend;
pub mod drives;
pub mod help;
pub mod icons;
pub mod image_page;
pub mod progress;
pub mod repair;
pub mod results;
pub mod system;
pub mod tasks;
pub mod theme;
pub mod thumbs;
pub mod widgets;
pub mod wizard;

pub use app::{DiskRescueApp, Page, Saved};
pub use backend::{Backend, SystemBackend};

/// The application id (window class, configuration folder).
pub const APP_ID: &str = "diskrescue";
