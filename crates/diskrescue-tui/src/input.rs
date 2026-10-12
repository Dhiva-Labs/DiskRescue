//! A one-line text input with a cursor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::widgets::Theme;

/// What a key did to an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// The key means nothing to the input.
    Ignored,
    /// The cursor moved.
    Moved,
    /// The text changed.
    Changed,
}

/// A line of text and a cursor (counted in characters).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    value: String,
    cursor: usize,
}

impl TextInput {
    /// An input holding `s`, with the cursor at the end.
    pub fn new(s: impl Into<String>) -> Self {
        let value: String = s.into();
        let cursor = value.chars().count();
        TextInput { value, cursor }
    }

    /// The text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Replace the text; the cursor goes to the end.
    pub fn set(&mut self, s: impl Into<String>) {
        self.value = s.into();
        self.cursor = self.value.chars().count();
    }

    /// True when the input is empty (or only spaces).
    pub fn is_blank(&self) -> bool {
        self.value.trim().is_empty()
    }

    /// Cursor position in characters.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn len(&self) -> usize {
        self.value.chars().count()
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.value
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    /// Insert a character at the cursor.
    pub fn insert(&mut self, c: char) {
        let at = self.byte_at(self.cursor);
        self.value.insert(at, c);
        self.cursor += 1;
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let at = self.byte_at(self.cursor - 1);
        self.value.remove(at);
        self.cursor -= 1;
        true
    }

    /// Delete the character under the cursor.
    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.len() {
            return false;
        }
        let at = self.byte_at(self.cursor);
        self.value.remove(at);
        true
    }

    /// Delete the word before the cursor.
    pub fn delete_word(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor;
        let sep = |c: char| c == ' ' || c == '/' || c == '\\';
        while i > 0 && sep(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && !sep(chars[i - 1]) {
            i -= 1;
        }
        let rest: String = chars[self.cursor..].iter().collect();
        let head: String = chars[..i].iter().collect();
        self.value = format!("{head}{rest}");
        self.cursor = i;
        true
    }

    /// Handle an editing key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> Edit {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert(c);
                Edit::Changed
            }
            KeyCode::Char('a') if ctrl => self.home(),
            KeyCode::Char('e') if ctrl => self.end(),
            KeyCode::Char('u') if ctrl => {
                let at = self.byte_at(self.cursor);
                self.value.replace_range(..at, "");
                self.cursor = 0;
                Edit::Changed
            }
            KeyCode::Char('k') if ctrl => {
                let at = self.byte_at(self.cursor);
                self.value.truncate(at);
                Edit::Changed
            }
            KeyCode::Char('w') if ctrl => {
                if self.delete_word() {
                    Edit::Changed
                } else {
                    Edit::Moved
                }
            }
            KeyCode::Backspace if ctrl => {
                if self.delete_word() {
                    Edit::Changed
                } else {
                    Edit::Moved
                }
            }
            KeyCode::Backspace => {
                if self.backspace() {
                    Edit::Changed
                } else {
                    Edit::Moved
                }
            }
            KeyCode::Delete => {
                if self.delete() {
                    Edit::Changed
                } else {
                    Edit::Moved
                }
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                Edit::Moved
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.len());
                Edit::Moved
            }
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
            _ => Edit::Ignored,
        }
    }

    fn home(&mut self) -> Edit {
        self.cursor = 0;
        Edit::Moved
    }

    fn end(&mut self) -> Edit {
        self.cursor = self.len();
        Edit::Moved
    }

    /// The input as a line `width` characters wide. A focused input shows
    /// its cursor (reverse video) and scrolls to keep it in view.
    pub fn line(&self, width: usize, focused: bool, theme: &Theme) -> Line<'static> {
        let chars: Vec<char> = self.value.chars().collect();
        let width = width.max(2);
        // Room for the cursor cell after the last character.
        let visible = width - 1;
        let start = self.cursor.saturating_sub(visible);
        let end = (start + visible).min(chars.len());
        let before: String = chars[start..self.cursor.min(end).max(start)]
            .iter()
            .collect();
        if !focused {
            let shown: String = chars[start..end].iter().collect();
            return Line::from(Span::raw(shown));
        }
        let at: String = if self.cursor < chars.len() {
            chars[self.cursor].to_string()
        } else {
            " ".to_string()
        };
        let after: String = if self.cursor + 1 < end {
            chars[self.cursor + 1..end].iter().collect()
        } else {
            String::new()
        };
        let cursor_style = if theme.color {
            ratatui::style::Style::new().add_modifier(Modifier::REVERSED)
        } else {
            ratatui::style::Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        };
        Line::from(vec![
            Span::raw(before),
            Span::styled(at, cursor_style),
            Span::raw(after),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..key(KeyCode::Char(c))
        }
    }

    #[test]
    fn typing_editing_and_moving() {
        let mut t = TextInput::new("ab");
        assert_eq!(t.handle_key(&key(KeyCode::Char('c'))), Edit::Changed);
        assert_eq!(t.value(), "abc");
        t.handle_key(&key(KeyCode::Left));
        t.handle_key(&key(KeyCode::Char('X')));
        assert_eq!(t.value(), "abXc");
        t.handle_key(&key(KeyCode::Backspace));
        t.handle_key(&key(KeyCode::Delete));
        assert_eq!(t.value(), "ab");
        t.handle_key(&key(KeyCode::Home));
        t.handle_key(&key(KeyCode::Char('>')));
        assert_eq!(t.value(), ">ab");
        t.handle_key(&ctrl('e'));
        assert_eq!(t.cursor(), 3);
        t.handle_key(&ctrl('u'));
        assert_eq!(t.value(), "");
        assert!(t.is_blank());
        assert_eq!(t.handle_key(&key(KeyCode::F(5))), Edit::Ignored);
    }

    #[test]
    fn works_with_multibyte_characters() {
        let mut t = TextInput::new("caf\u{e9}");
        t.handle_key(&key(KeyCode::Left));
        t.handle_key(&key(KeyCode::Char('X')));
        assert_eq!(t.value(), "cafX\u{e9}");
        t.handle_key(&key(KeyCode::End));
        t.handle_key(&key(KeyCode::Backspace));
        assert_eq!(t.value(), "cafX");
    }

    #[test]
    fn word_deletion_stops_at_slashes() {
        let mut t = TextInput::new("/home/me/photos");
        t.handle_key(&ctrl('w'));
        assert_eq!(t.value(), "/home/me/");
        t.handle_key(&ctrl('w'));
        assert_eq!(t.value(), "/home/");
    }

    #[test]
    fn the_line_scrolls_to_the_cursor() {
        let t = TextInput::new("0123456789abcdef");
        let l = t.line(8, true, &Theme::new(true));
        let text: String = l.spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text.chars().count(), 8);
        assert!(text.starts_with("9abcdef"), "{text}");
        let l = TextInput::new("hello").line(20, false, &Theme::new(false));
        assert_eq!(l.spans[0].content, "hello");
    }
}
