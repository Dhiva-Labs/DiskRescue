//! Choosing a folder or files with the browser.

use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

use crate::fsbrowse::{PickEvent, Picker};
use crate::screens::form::FieldId;
use crate::screens::results::ResultsData;
use crate::widgets::Theme;

/// What the chosen paths are for.
#[derive(Debug, Clone)]
pub enum PickPurpose {
    /// A field of the form below this screen.
    Field(FieldId),
    /// An image file to recover from.
    OpenImage,
    /// The output folder of an earlier recovery.
    OpenResults,
    /// Loose files to repair or check.
    RepairFiles,
    /// A reference file for a damaged result.
    Donor {
        /// The results.
        data: Arc<ResultsData>,
        /// The damaged item.
        target: usize,
    },
}

/// The screen around a browser.
pub struct PickScreen {
    /// The browser.
    pub picker: Picker,
    /// What the choice is for.
    pub purpose: PickPurpose,
}

/// What the screen wants the app to do.
#[derive(Debug, Clone)]
pub enum PickAction {
    /// Nothing.
    None,
    /// Cancelled.
    Cancel,
    /// Chosen.
    Chosen(Vec<PathBuf>),
    /// Show the help.
    Help,
}

impl PickScreen {
    /// A browser for `purpose`.
    pub fn new(picker: Picker, purpose: PickPurpose) -> Self {
        PickScreen { picker, purpose }
    }

    /// Text is being typed (the "go to" path).
    pub fn typing(&self) -> bool {
        self.picker.goto.is_some()
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> PickAction {
        if self.picker.goto.is_none()
            && matches!(
                key.code,
                crossterm::event::KeyCode::Char('?') | crossterm::event::KeyCode::F(1)
            )
        {
            return PickAction::Help;
        }
        match self.picker.handle_key(key) {
            PickEvent::None => PickAction::None,
            PickEvent::Cancel => PickAction::Cancel,
            PickEvent::Chosen(v) => PickAction::Chosen(v),
        }
    }

    /// Click a row.
    pub fn click_row(&mut self, index: usize) -> PickAction {
        match self.picker.click(index) {
            PickEvent::None => PickAction::None,
            PickEvent::Cancel => PickAction::Cancel,
            PickEvent::Chosen(v) => PickAction::Chosen(v),
        }
    }

    /// Scroll with the wheel.
    pub fn wheel(&mut self, delta: isize, height: usize) {
        self.picker.wheel(delta, height);
    }

    /// Draw it. Returns the rows' area and first visible row.
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) -> (Rect, usize) {
        let (off, rect) = self.picker.draw(f, area, theme);
        (rect, off)
    }
}
