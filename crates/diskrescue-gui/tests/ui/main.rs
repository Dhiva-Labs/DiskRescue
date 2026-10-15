//! User interface tests, run headless (`env -u DISPLAY -u WAYLAND_DISPLAY
//! cargo test -p diskrescue-gui`): no window is ever opened and no real
//! block device is ever read. One test binary for all of them (they share
//! the fixtures and the rendering lock).

mod common;
mod flow;
mod gallery;
mod pages;
mod repair;
mod small_window;
mod store_images;
mod store_screenshots;
mod wizard;
