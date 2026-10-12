//! Interactive terminal app for DiskRescue.
//!
//! A full-screen, keyboard-first program on top of `diskrescue-app`: pick
//! a device, answer a few questions, watch the copy and the recovery, then
//! browse what came back. [`App`] is a state machine: it takes [`Event`]s
//! and draws itself into a ratatui frame, so the whole program can be
//! driven in tests without a terminal. [`runtime`] connects it to a real
//! terminal.
//!
//! The imaging dashboard ([`dashboard`]) is shared with the command line's
//! `diskrescue image`.

pub mod app;
pub mod config;
pub mod dashboard;
pub mod event;
pub mod fsbrowse;
pub mod input;
pub mod model;
pub mod parse;
pub mod pending;
pub mod runtime;
pub mod screens;
pub mod testing;
pub mod widgets;

pub use app::App;
pub use config::Config;
pub use event::Event;
