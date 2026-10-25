//! A compact, searchable history popup for the prompt.
//!
//! The REPL hands control here when the user asks for history search (Ctrl-R,
//! or Up on a partial command). Instead of taking over the screen, the picker
//! draws a small popup at the prompt: up to five matching commands and one
//! search/status line. The popup opens downwards when there is room below the
//! prompt; at the bottom of the screen it scrolls recent output up just enough
//! to fit above the prompt, which stays visible below it. It is removed again
//! when a command is picked or the search is cancelled.
//!
//! The state machine and drawing are kept apart from the terminal layer so
//! matching, navigation and layout can be tested without a TTY.

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use aeroshell::config;

// Terminal styling for the popup itself, independent of the prompt theme.
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const REVERSE: &str = "\x1b[7m";
const REVERSE_OFF: &str = "\x1b[27m";
const MATCH_ON: &str = "\x1b[1;4m";
const MATCH_OFF: &str = "\x1b[22;24m";

/// The popup shows up to this many matching commands, plus its query line.
const MAX_MATCH_ROWS: usize = 5;

/// What the user asked for with one keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Backspace,
    CtrlW,
    Clear,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Cancel,
    Ignore,
}

/// What the REPL should do after a keystroke.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Continue,
    Accept(String),
    Cancel,
}

/// One history entry, prepared for searching and display.
struct Row {
    /// The command as stored, handed back verbatim on accept.
    text: String,
    /// One-line rendering: newlines flattened, control characters removed.
    line: String,
    /// `line` lowercased for case-insensitive matching (same byte length).
    lower: String,
}

impl Row {
    fn new(text: String) -> Self {
        let line = display_text(&text);
        let lower = line.to_ascii_lowercase();
        Self { text, line, lower }
    }
}

/// One rendered popup frame.
#[derive(Debug, PartialEq, Eq)]
pub struct PopupFrame {
    /// The search/status line.
    pub query: String,
    /// Zero-based column where the terminal cursor belongs on the query line.
    pub cursor_col: usize,
    /// Match rows, top to bottom. Always exactly as many as the popup asked
    /// for; unused rows are blank.
    pub rows: Vec<String>,
    /// Whether the list grows upwards towards the old output (query at the
    /// bottom, just above the prompt).
    pub upward: bool,
}

/// The popup's state machine: query, matches, selection and scrolling.
pub struct HistoryPicker {
    rows: Vec<Row>,
    matching: Vec<usize>,
    query: String,
    selected: usize,
    offset: usize,
    page: usize,
}

impl HistoryPicker {
    /// Build a picker over `entries`, oldest first, with an initial query.
    /// Empty entries are dropped; they can never be selected anyway.
    pub fn new(entries: Vec<String>, initial: &str) -> Self {
        let rows: Vec<Row> = entries
            .into_iter()
            .filter(|entry| !entry.is_empty())
            .map(Row::new)
            .collect();
        let mut picker = Self {
            rows,
            matching: Vec::new(),
            query: initial.to_string(),
            selected: 0,
            offset: 0,
            page: 1,
        };
        picker.refilter();
        picker
    }

    #[cfg(test)]
    pub fn query(&self) -> &str {
        &self.query
    }

    #[cfg(test)]
    pub fn selected(&self) -> usize {
        self.selected
    }

    #[cfg(test)]
    pub fn offset(&self) -> usize {
        self.offset
    }

    #[cfg(test)]
    pub fn match_count(&self) -> usize {
        self.matching.len()
    }

    pub fn selected_entry(&self) -> Option<&str> {
        self.matching
            .get(self.selected)
            .map(|&index| self.rows[index].text.as_str())
    }

    /// Apply one keystroke.
    pub fn handle(&mut self, key: Key) -> Action {
        match key {
            Key::Char(c) => {
                self.query.push(c);
                self.refilter();
            }
            Key::Backspace => {
                if self.query.pop().is_some() {
                    self.refilter();
                }
            }
            Key::CtrlW => {
                delete_last_word(&mut self.query);
                self.refilter();
            }
            Key::Clear => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.refilter();
                }
            }
            Key::Up => self.move_selection(-1),
            Key::Down => self.move_selection(1),
            Key::PageUp => self.move_page(-1),
            Key::PageDown => self.move_page(1),
            Key::Home => {
                if !self.matching.is_empty() {
                    self.selected = 0;
                }
            }
            Key::End => {
                if !self.matching.is_empty() {
                    self.selected = self.matching.len() - 1;
                }
            }
            Key::Enter => {
                if let Some(entry) = self.selected_entry() {
                    return Action::Accept(entry.to_string());
                }
            }
            Key::Cancel => return Action::Cancel,
            Key::Ignore => {}
        }
        Action::Continue
    }

    /// Recompute the matches for the current query. Prefix matches come first
    /// (they are usually what the user means), then substring matches; each
    /// group keeps history order, newest first.
    fn refilter(&mut self) {
        self.matching.clear();
        if self.query.is_empty() {
            self.matching.extend((0..self.rows.len()).rev());
        } else {
            let needle = self.query.to_ascii_lowercase();
            let mut prefixes = Vec::new();
            let mut substrings = Vec::new();
            for (index, row) in self.rows.iter().enumerate() {
                if let Some(pos) = row.lower.find(&needle) {
                    if pos == 0 {
                        prefixes.push(index);
                    } else {
                        substrings.push(index);
                    }
                }
            }
            prefixes.reverse();
            substrings.reverse();
            self.matching.extend(prefixes);
            self.matching.extend(substrings);
        }
        self.selected = 0;
        self.offset = 0;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.matching.is_empty() {
            return;
        }
        let last = self.matching.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }

    fn move_page(&mut self, pages: isize) {
        let step = self.page.max(1) as isize * pages;
        self.move_selection(step);
    }

    /// Draw the match area and query line. `match_rows` is the number of
    /// matching commands the popup has room for; the query/status line is
    /// drawn separately by the caller. `upward` reverses the visual order so
    /// the newest visible match sits closest to the query line at the bottom.
    pub fn frame(
        &mut self,
        width: usize,
        match_rows: usize,
        upward: bool,
        color: bool,
    ) -> PopupFrame {
        let width = width.max(1);
        let match_rows = match_rows.max(1);
        self.page = match_rows;
        self.ensure_visible(match_rows);

        let mut rows: Vec<String> = if self.matching.is_empty() {
            let message = if self.rows.is_empty() {
                "  no history yet"
            } else {
                "  no matching commands"
            };
            let message = style(fit(message, width), DIM, color);
            let mut rows = vec![String::new(); match_rows];
            if upward {
                rows[match_rows - 1] = message;
            } else {
                rows[0] = message;
            }
            rows
        } else {
            let end = (self.offset + match_rows).min(self.matching.len());
            let mut lines: Vec<String> = (self.offset..end)
                .map(|matching_index| {
                    let row = &self.rows[self.matching[matching_index]];
                    self.row_line(row, matching_index == self.selected, width, color)
                })
                .collect();
            if upward {
                lines.reverse();
                let mut padded = vec![String::new(); match_rows - lines.len()];
                padded.extend(lines);
                padded
            } else {
                lines.resize(match_rows, String::new());
                lines
            }
        };
        rows.truncate(match_rows);

        let (query, cursor_col) = self.query_line(width, color);
        PopupFrame {
            query,
            cursor_col,
            rows,
            upward,
        }
    }

    fn ensure_visible(&mut self, match_rows: usize) {
        let match_rows = match_rows.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + match_rows {
            self.offset = self.selected + 1 - match_rows;
        }
        let max_offset = self.matching.len().saturating_sub(match_rows);
        self.offset = self.offset.min(max_offset);
    }

    /// `> query` on the left and the position/help on the right. The query is
    /// cropped from the left when it is too long for the space available.
    fn query_line(&self, width: usize, color: bool) -> (String, usize) {
        let prefix = if width >= 2 { "> " } else { "" };
        let prefix_width = prefix.chars().count();
        let position = if self.matching.is_empty() {
            "0/0".to_string()
        } else {
            format!("{}/{}", self.selected + 1, self.matching.len())
        };
        let mut status = if width >= 36 {
            format!("{position}  \u{2191}\u{2193}  enter  esc")
        } else if width >= 10 {
            position
        } else {
            String::new()
        };
        let mut status_width = status.chars().count();
        // Keep at least one column between the query and the status.
        if status_width > 0 && width <= prefix_width + status_width + 1 {
            status.clear();
            status_width = 0;
        }
        let reserved = if status_width > 0 {
            status_width + 1
        } else {
            0
        };
        let available = width.saturating_sub(prefix_width + reserved);
        let total = self.query.chars().count();
        let shown: String = if total <= available {
            self.query.clone()
        } else if available == 0 {
            String::new()
        } else {
            self.query.chars().skip(total - available).collect()
        };
        let shown_width = shown.chars().count();
        let left_width = prefix_width + shown_width;
        let gap = width.saturating_sub(left_width + status_width);

        let mut line = String::new();
        if color && !prefix.is_empty() {
            line.push_str(BOLD);
            line.push_str(prefix);
            line.push_str(RESET);
        } else {
            line.push_str(prefix);
        }
        line.push_str(&shown);
        line.push_str(&" ".repeat(gap));
        if !status.is_empty() {
            line.push_str(&style(status, DIM, color));
        }
        let cursor = left_width.min(width.saturating_sub(1));
        (line, cursor)
    }

    fn row_line(&self, row: &Row, selected: bool, width: usize, color: bool) -> String {
        // One column is enough for the selection marker on very narrow
        // terminals; two keep the text aligned when there is room.
        let prefix = match (selected, width) {
            (true, 0..=1) => ">",
            (true, _) => "> ",
            (false, 0..=1) => " ",
            (false, _) => "  ",
        };
        let prefix_width = prefix.chars().count();
        let text = fit(&row.line, width.saturating_sub(prefix_width));
        let (before, matched, after) = split_match(&text, &self.query);

        let mut out = String::new();
        if color {
            if selected {
                out.push_str(REVERSE);
            }
            out.push_str(prefix);
            out.push_str(before);
            if !matched.is_empty() {
                out.push_str(MATCH_ON);
                out.push_str(matched);
                out.push_str(MATCH_OFF);
            }
            out.push_str(after);
            if selected {
                // A full-width bar reads better than a reverse patch that
                // stops at the end of a short entry.
                let used = prefix_width + text.chars().count();
                out.push_str(&" ".repeat(width.saturating_sub(used)));
                out.push_str(REVERSE_OFF);
            }
        } else {
            out.push_str(prefix);
            out.push_str(&text);
        }
        out
    }
}

/// The terminal-facing result of asking for a search.
#[derive(Debug, PartialEq, Eq)]
pub enum Picked {
    Entry(String),
    Cancelled,
    Unavailable,
}

/// Run the popup on the terminal. Returns `Unavailable` when stdin/stdout are
/// not a TTY or the terminal is too small to show even one match, so the
/// caller can keep the normal prompt.
pub fn pick(entries: Vec<String>, initial: &str, prompt: &str, hint_len: usize) -> Picked {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Picked::Unavailable;
    }
    let Some(mut terminal) = Terminal::enter() else {
        return Picked::Unavailable;
    };
    let (width, height) = terminal_size();
    if width < 2 || height < 2 {
        return Picked::Unavailable;
    }

    // Leave one row for the query line. The prompt itself already has the row
    // it was drawn on, so the popup may use up to `height - 1` match rows.
    let match_rows = (height - 2).clamp(1, MAX_MATCH_ROWS);
    // After rustyline finishes interrupting the line it leaves the cursor on
    // the row below the prompt. Ask the terminal where that is; when it does
    // not answer, assume the prompt is at the bottom (one row above the
    // cursor).
    let cursor_row = terminal.cursor_position().map_or(height, |(row, _)| row);
    let last_prompt_row = prompt_row_from_cursor(cursor_row, height);
    // A long prompt (or a hint) can wrap over several rows. Anchor the popup
    // on the first one and clear the whole block, so the resumed prompt does
    // not leave a stale first line behind.
    let prompt_rows = prompt_height(prompt, initial, hint_len, width);
    let prompt_row = last_prompt_row.saturating_sub(prompt_rows - 1);
    let layout = layout(height, prompt_row, match_rows);
    let prompt_display = format!("{prompt}{initial}");
    if !terminal.setup(layout, prompt_row, prompt_rows, height, &prompt_display) {
        return Picked::Unavailable;
    }

    let color = config::active_config().map(|cfg| cfg.color).unwrap_or(true);
    let mut picker = HistoryPicker::new(entries, initial);
    loop {
        let frame = picker.frame(width, match_rows, layout.upward, color);
        if terminal.draw(&frame).is_err() {
            return Picked::Cancelled;
        }
        let Ok(key) = terminal.read_key() else {
            return Picked::Cancelled;
        };
        // In the upward layout the list is drawn in reverse, so the same
        // physical key has to move the selection the other way.
        let key = orient_key(key, layout.upward);
        match picker.handle(key) {
            Action::Continue => {}
            Action::Accept(entry) => return Picked::Entry(entry),
            Action::Cancel => return Picked::Cancelled,
        }
    }
}

/// Where the popup's rows live and whether the screen had to scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    /// Matches above the query line (the screen was scrolled up to make room).
    upward: bool,
    /// The prompt is left below the popup and has to be redrawn while the
    /// picker is open.
    prompt_visible: bool,
    /// Lines the screen was scrolled up by; undone when the popup closes.
    scrolled: usize,
    /// First and last (inclusive) terminal row covered by the popup.
    popup_start: usize,
    popup_end: usize,
    /// Row of the query/status line.
    query_row: usize,
    /// First row of the match area.
    match_start: usize,
}

/// Number of terminal rows taken by the prompt, the text being edited and the
/// inline hint that follows it. ANSI colour sequences do not take space.
fn prompt_height(prompt: &str, line: &str, hint_len: usize, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut rows = 1usize;
    let mut column = 0usize;
    let mut escape = false;
    for character in prompt.chars().chain(line.chars()) {
        if escape {
            if character.is_ascii_alphabetic() {
                escape = false;
            }
        } else if character == '\x1b' {
            escape = true;
        } else if character == '\n' {
            rows += 1;
            column = 0;
        } else {
            column += 1;
            while column > width {
                rows += 1;
                column -= width;
            }
        }
    }
    column += hint_len;
    while column > width {
        rows += 1;
        column -= width;
    }
    rows
}

/// Convert the 1-based cursor row reported by the terminal into the 0-based
/// row of the prompt that was just interrupted. Rustyline prints a newline
/// after the interrupted prompt, leaving the cursor one row below it; the
/// other row is the 1-based to 0-based conversion.
fn prompt_row_from_cursor(cursor_row: usize, height: usize) -> usize {
    cursor_row.saturating_sub(2).min(height.saturating_sub(1))
}

/// Work out where a `match_rows`-tall list plus its query line can go without
/// covering any more output than necessary. Rows are zero-based.
fn layout(height: usize, prompt_row: usize, match_rows: usize) -> Layout {
    let popup_rows = match_rows + 1;
    if prompt_row + popup_rows <= height {
        // There is room below the prompt: replace the prompt line with the
        // query and grow downwards.
        Layout {
            upward: false,
            prompt_visible: false,
            scrolled: 0,
            popup_start: prompt_row,
            popup_end: prompt_row + match_rows,
            query_row: prompt_row,
            match_start: prompt_row + 1,
        }
    } else if prompt_row >= popup_rows {
        // Near the bottom: scroll the output up so the whole popup fits above
        // the prompt, then redraw the prompt below it. The prompt stays where
        // the user left it and the matches are above the search line.
        Layout {
            upward: true,
            prompt_visible: true,
            scrolled: popup_rows,
            popup_start: prompt_row - popup_rows,
            popup_end: prompt_row - 1,
            query_row: prompt_row - 1,
            match_start: prompt_row - popup_rows,
        }
    } else {
        // Tiny terminal: there is no room for both the popup and the prompt.
        // Fall back to scrolling the prompt out of the way and putting the
        // query line at the bottom.
        let start = height - 1 - match_rows;
        let scrolled = prompt_row.saturating_sub(start);
        Layout {
            upward: true,
            prompt_visible: false,
            scrolled,
            popup_start: start,
            popup_end: height - 1,
            query_row: height - 1,
            match_start: start,
        }
    }
}

/// Raw mode and a hidden cursor, restored on drop (including panics).
struct Terminal {
    original: libc::termios,
    geometry: Option<Layout>,
    prompt_row: usize,
    prompt_rows: usize,
    /// Bytes read while waiting for a cursor-position reply, replayed as keys.
    pending: VecDeque<u8>,
}

impl Terminal {
    fn enter() -> Option<Self> {
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return None;
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        // Do not flush pending input: a fast typist may already have typed the
        // first letters of the query while the popup was opening. Those bytes
        // are replayed through `pending` after the cursor-position query.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        let _ = io::stdout().write_all(b"\x1b[?25l");
        let _ = io::stdout().flush();
        Some(Self {
            original,
            geometry: None,
            prompt_row: 0,
            prompt_rows: 0,
            pending: VecDeque::new(),
        })
    }

    /// Ask the terminal for the cursor position (`CSI 6n`). Best effort: if
    /// nothing sensible comes back within a short window, the caller falls
    /// back to assuming the prompt is at the bottom of the screen.
    fn cursor_position(&mut self) -> Option<(usize, usize)> {
        let mut stdout = io::stdout();
        stdout.write_all(b"\x1b[6n").ok()?;
        stdout.flush().ok()?;

        let deadline = Instant::now() + Duration::from_millis(120);
        let mut bytes = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let timeout = (remaining.as_millis().min(20)) as i32;
            if !wait_readable(timeout) {
                continue;
            }
            let Ok(byte) = read_byte() else {
                break;
            };
            bytes.push(byte);
            if byte == b'R' {
                break;
            }
        }
        if let Some((start, position)) = cursor_reply(&bytes) {
            // Anything read before the reply is user input; keep it for the
            // picker instead of dropping the first characters typed.
            for byte in bytes[..start].iter().rev() {
                self.pending.push_front(*byte);
            }
            return Some(position);
        }
        for byte in bytes.into_iter().rev() {
            self.pending.push_front(byte);
        }
        None
    }

    /// Scroll (if needed), remember the popup geometry, and clear the rows it
    /// will occupy as well as any remaining rows of a wrapped prompt.
    fn setup(
        &mut self,
        layout: Layout,
        prompt_row: usize,
        prompt_rows: usize,
        height: usize,
        prompt_display: &str,
    ) -> bool {
        if layout.scrolled > 0 {
            let mut stdout = io::stdout();
            if stdout
                .write_all(format!("\x1b[{}S", layout.scrolled).as_bytes())
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return false;
            }
        }
        self.geometry = Some(layout);
        self.prompt_row = prompt_row;
        self.prompt_rows = prompt_rows;
        let end = layout
            .popup_end
            .max(prompt_row + prompt_rows - 1)
            .min(height - 1);
        let mut out = String::new();
        for row in layout.popup_start..=end {
            out.push_str(&format!("\x1b[{};1H\x1b[2K", row + 1));
        }
        if layout.prompt_visible {
            out.push_str(&format!("\x1b[{};1H\x1b[2K", prompt_row + 1));
            out.push_str(prompt_display);
        }
        let mut stdout = io::stdout();
        stdout
            .write_all(out.as_bytes())
            .and_then(|()| stdout.flush())
            .is_ok()
    }

    fn draw(&mut self, frame: &PopupFrame) -> io::Result<()> {
        let Some(layout) = self.geometry else {
            return Ok(());
        };
        let mut out = String::from("\x1b[?25l");
        for (offset, line) in frame.rows.iter().enumerate() {
            let row = layout.match_start + offset;
            out.push_str(&format!("\x1b[{};1H\x1b[2K{line}", row + 1));
        }
        out.push_str(&format!(
            "\x1b[{};1H\x1b[2K{}",
            layout.query_row + 1,
            frame.query
        ));
        out.push_str(&format!(
            "\x1b[{};{}H\x1b[?25h",
            layout.query_row + 1,
            frame.cursor_col + 1
        ));
        let mut stdout = io::stdout();
        stdout.write_all(out.as_bytes())?;
        stdout.flush()
    }

    fn read_key(&mut self) -> io::Result<Key> {
        let byte = self.next_byte()?;
        match byte {
            b'\x1b' => self.read_escape(),
            0x03 | 0x07 | 0x04 => Ok(Key::Cancel), // Ctrl-C, Ctrl-G, Ctrl-D
            b'\r' | b'\n' => Ok(Key::Enter),
            0x7f | 0x08 => Ok(Key::Backspace),
            0x15 => Ok(Key::Clear), // Ctrl-U
            0x17 => Ok(Key::CtrlW),
            0x10 => Ok(Key::Up),   // Ctrl-P
            0x0e => Ok(Key::Down), // Ctrl-N
            0x12 => Ok(Key::Up),   // Ctrl-R steps back through matches
            byte if byte < 0x20 => Ok(Key::Ignore),
            byte if byte < 0x80 => Ok(Key::Char(byte as char)),
            byte => self.read_utf8(byte),
        }
    }

    fn read_escape(&mut self) -> io::Result<Key> {
        if self.pending.is_empty() && !wait_readable(20) {
            return Ok(Key::Cancel);
        }
        let first = self.next_byte()?;
        match first {
            b'[' => {
                let mut sequence = vec![b'['];
                loop {
                    if sequence.len() > 8 {
                        return Ok(Key::Ignore);
                    }
                    let byte = self.next_byte()?;
                    sequence.push(byte);
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
                Ok(match sequence.as_slice() {
                    b"[A" => Key::Up,
                    b"[B" => Key::Down,
                    b"[H" | b"[1~" | b"[7~" => Key::Home,
                    b"[F" | b"[4~" | b"[8~" => Key::End,
                    b"[5~" => Key::PageUp,
                    b"[6~" => Key::PageDown,
                    _ => Key::Ignore,
                })
            }
            b'O' => {
                let byte = self.next_byte()?;
                Ok(match byte {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'H' => Key::Home,
                    b'F' => Key::End,
                    _ => Key::Ignore,
                })
            }
            _ => Ok(Key::Ignore),
        }
    }

    fn read_utf8(&mut self, first: u8) -> io::Result<Key> {
        let extra = match first {
            0xc0..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf7 => 3,
            _ => return Ok(Key::Ignore),
        };
        let mut bytes = vec![first];
        for _ in 0..extra {
            bytes.push(self.next_byte()?);
        }
        Ok(std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.chars().next())
            .map_or(Key::Ignore, Key::Char))
    }

    fn next_byte(&mut self) -> io::Result<u8> {
        match self.pending.pop_front() {
            Some(byte) => Ok(byte),
            None => read_byte(),
        }
    }

    /// Remove the popup, undo the scroll, and put the cursor where the prompt
    /// should be redrawn.
    fn cleanup(&mut self) {
        let Some(layout) = self.geometry.take() else {
            let _ = io::stdout().write_all(b"\x1b[?25h");
            let _ = io::stdout().flush();
            return;
        };
        let mut out = String::new();
        for row in layout.popup_start..=layout.popup_end {
            out.push_str(&format!("\x1b[{};1H\x1b[2K", row + 1));
        }
        if layout.prompt_visible {
            for row in self.prompt_row..self.prompt_row + self.prompt_rows {
                out.push_str(&format!("\x1b[{};1H\x1b[2K", row + 1));
            }
        }
        if layout.scrolled > 0 {
            out.push_str(&format!("\x1b[{}T", layout.scrolled));
        }
        out.push_str(&format!("\x1b[{};1H\x1b[2K\x1b[?25h", self.prompt_row + 1));
        let mut stdout = io::stdout();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.cleanup();
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.original);
        }
    }
}

/// Locate a cursor-position reply inside the bytes read while waiting for it.
/// Returns the reply's offset and `(row, column)`; any bytes before it are
/// typed input that still has to be handled.
fn cursor_reply(bytes: &[u8]) -> Option<(usize, (usize, usize))> {
    for start in 0..bytes.len() {
        if bytes[start] == 0x1b
            && bytes.get(start + 1) == Some(&b'[')
            && let Some(position) = parse_cursor_position(&bytes[start..])
        {
            return Some((start, position));
        }
    }
    None
}

fn parse_cursor_position(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] == 0x1b && bytes[index + 1] == b'[' {
            let rest = &bytes[index + 2..];
            let end = rest.iter().position(|&byte| byte == b'R')?;
            let body = std::str::from_utf8(&rest[..end]).ok()?;
            let (row, column) = body.split_once(';')?;
            let row = row.parse().ok()?;
            let column = column.parse().ok()?;
            return Some((row, column));
        }
        index += 1;
    }
    None
}

fn terminal_size() -> (usize, usize) {
    #[cfg(unix)]
    {
        let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_col > 0
            && size.ws_row > 0
        {
            return (size.ws_col as usize, size.ws_row as usize);
        }
    }
    (80, 24)
}

fn read_byte() -> io::Result<u8> {
    let mut byte = [0u8; 1];
    loop {
        let read = unsafe { libc::read(libc::STDIN_FILENO, byte.as_mut_ptr().cast(), 1) };
        if read == 1 {
            return Ok(byte[0]);
        }
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "stdin closed"));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}

fn wait_readable(timeout_ms: i32) -> bool {
    let mut poll = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
        if ready >= 0 {
            return ready > 0;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// Map vertical movement keys to the visual direction of the popup. The
/// downward layout draws the newest match at the top; the upward layout draws
/// it at the bottom, directly above the query line.
fn orient_key(key: Key, upward: bool) -> Key {
    if !upward {
        return key;
    }
    match key {
        Key::Up => Key::Down,
        Key::Down => Key::Up,
        Key::PageUp => Key::PageDown,
        Key::PageDown => Key::PageUp,
        other => other,
    }
}

/// The searchable, printable form of a stored command: newlines become a
/// visible marker, tabs become spaces, and other control characters (which
/// could otherwise inject terminal escapes) are dropped.
fn display_text(entry: &str) -> String {
    let mut out = String::with_capacity(entry.len());
    for character in entry.chars() {
        match character {
            '\n' => out.push_str(" \u{23ce} "),
            '\t' => out.push_str("  "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn delete_last_word(query: &mut String) {
    while query.ends_with(' ') {
        query.pop();
    }
    while let Some(last) = query.chars().next_back() {
        if last == ' ' {
            break;
        }
        query.pop();
    }
}

/// Split `text` around its first case-insensitive occurrence of `query`.
fn split_match<'a>(text: &'a str, query: &str) -> (&'a str, &'a str, &'a str) {
    if query.is_empty() {
        return (text, "", "");
    }
    let haystack = text.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    match haystack.find(&needle) {
        Some(start) => {
            let end = start + needle.len();
            // ASCII lowercasing preserves byte length and every non-ASCII
            // character, so these are still valid boundaries in `text`.
            (&text[..start], &text[start..end], &text[end..])
        }
        None => (text, "", ""),
    }
}

/// Crop `text` to `width` characters, ending in an ellipsis when it had to cut.
fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('\u{2026}');
    out
}

fn style(text: String, code: &str, color: bool) -> String {
    if color {
        format!("{code}{text}{RESET}")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(entries: &[&str]) -> HistoryPicker {
        HistoryPicker::new(entries.iter().map(|entry| entry.to_string()).collect(), "")
    }

    fn picker_with(entries: &[&str], query: &str) -> HistoryPicker {
        HistoryPicker::new(
            entries.iter().map(|entry| entry.to_string()).collect(),
            query,
        )
    }

    /// Drop the SGR sequences this module emits, leaving visible text.
    fn plain(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(character) = chars.next() {
            if character == '\x1b' {
                for code in chars.by_ref() {
                    if code.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(character);
            }
        }
        out
    }

    fn body(frame: &PopupFrame) -> String {
        frame
            .rows
            .iter()
            .map(|line| plain(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn newest_entries_come_first() {
        let mut picker = picker(&["old", "middle", "new"]);
        assert_eq!(picker.match_count(), 3);
        assert_eq!(picker.selected_entry(), Some("new"));

        let frame = picker.frame(40, 3, false, false);
        assert!(plain(&frame.rows[0]).starts_with("> new"));
        assert!(plain(&frame.rows[1]).starts_with("  middle"));
        assert!(plain(&frame.rows[2]).starts_with("  old"));
    }

    #[test]
    fn search_is_case_insensitive_and_filters() {
        let mut picker = picker_with(&["Git status", "cargo test", "git log"], "GIT");
        assert_eq!(picker.match_count(), 2);
        assert_eq!(picker.selected_entry(), Some("git log"));

        picker = picker_with(&["Git status", "cargo test", "git log"], "cArGo");
        assert_eq!(picker.match_count(), 1);
        assert_eq!(picker.selected_entry(), Some("cargo test"));
    }

    #[test]
    fn prefix_matches_rank_before_other_contains() {
        let picker = picker_with(&["echo git", "git status", "legit"], "git");
        let entries: Vec<&str> = picker
            .matching
            .iter()
            .map(|&index| picker.rows[index].text.as_str())
            .collect();
        // Prefix match first, then substring matches newest first.
        assert_eq!(entries, vec!["git status", "legit", "echo git"]);
    }

    #[test]
    fn upward_layout_flips_vertical_keys() {
        assert_eq!(orient_key(Key::Up, true), Key::Down);
        assert_eq!(orient_key(Key::Down, true), Key::Up);
        assert_eq!(orient_key(Key::PageUp, true), Key::PageDown);
        assert_eq!(orient_key(Key::PageDown, true), Key::PageUp);
        assert_eq!(orient_key(Key::Up, false), Key::Up);
        assert_eq!(orient_key(Key::PageDown, false), Key::PageDown);
        assert_eq!(orient_key(Key::Home, true), Key::Home);
    }

    #[test]
    fn navigation_stops_at_the_ends() {
        let mut picker = picker(&["one", "two", "three"]);
        assert_eq!(picker.selected(), 0);
        picker.handle(Key::Up);
        assert_eq!(picker.selected(), 0);
        picker.handle(Key::Down);
        picker.handle(Key::Down);
        assert_eq!(picker.selected(), 2);
        picker.handle(Key::Down);
        assert_eq!(picker.selected(), 2);
        picker.handle(Key::Up);
        assert_eq!(picker.selected(), 1);
    }

    #[test]
    fn page_and_home_end_keys_move_selection() {
        let entries: Vec<String> = (0..30).map(|n| format!("entry {n}")).collect();
        let mut picker = HistoryPicker::new(entries, "");
        // Five match rows plus the query line is the usual popup size.
        picker.frame(40, 5, false, false);
        assert_eq!(picker.page, 5);

        picker.handle(Key::PageDown);
        assert_eq!(picker.selected(), 5);
        picker.handle(Key::PageDown);
        assert_eq!(picker.selected(), 10);
        picker.handle(Key::PageUp);
        assert_eq!(picker.selected(), 5);
        picker.handle(Key::End);
        assert_eq!(picker.selected(), 29);
        picker.handle(Key::Home);
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn changing_the_query_resets_selection_and_scroll() {
        let entries: Vec<String> = (0..30).map(|n| format!("cmd {n}")).collect();
        let mut picker = HistoryPicker::new(entries, "");
        picker.frame(40, 5, false, false);
        picker.handle(Key::PageDown);
        assert_eq!(picker.selected(), 5);
        picker.frame(40, 5, false, false);
        assert_eq!(picker.offset(), 1);

        picker.handle(Key::Char('c'));
        assert_eq!(picker.selected(), 0);
        assert_eq!(picker.offset(), 0);
        assert_eq!(picker.match_count(), 30);
    }

    #[test]
    fn backspace_and_ctrl_u_widen_the_search() {
        let mut picker = picker(&["cargo build", "git status"]);
        picker.handle(Key::Char('g'));
        assert_eq!(picker.match_count(), 2);
        picker.handle(Key::Char('i'));
        assert_eq!(picker.match_count(), 1);
        picker.handle(Key::Backspace);
        assert_eq!(picker.match_count(), 2);
        picker.handle(Key::Clear);
        assert_eq!(picker.query(), "");
        assert_eq!(picker.match_count(), 2);
    }

    #[test]
    fn ctrl_w_deletes_the_last_word() {
        let mut picker = picker(&["git status"]);
        for character in "git sta".chars() {
            picker.handle(Key::Char(character));
        }
        assert_eq!(picker.query(), "git sta");
        picker.handle(Key::CtrlW);
        assert_eq!(picker.query(), "git ");
    }

    #[test]
    fn enter_accepts_the_selected_entry_verbatim() {
        let mut picker = picker(&["echo one", "echo two"]);
        picker.handle(Key::Down);
        assert_eq!(
            picker.handle(Key::Enter),
            Action::Accept("echo one".to_string())
        );
    }

    #[test]
    fn enter_restores_newlines_in_a_multiline_entry() {
        let mut picker = picker(&["cat <<EOF\nhello\nEOF"]);
        assert_eq!(
            picker.handle(Key::Enter),
            Action::Accept("cat <<EOF\nhello\nEOF".to_string())
        );
    }

    #[test]
    fn enter_does_nothing_when_nothing_matches() {
        let mut picker = picker_with(&["git status"], "zzz");
        assert_eq!(picker.handle(Key::Enter), Action::Continue);
        assert_eq!(picker.handle(Key::Cancel), Action::Cancel);
    }

    #[test]
    fn escape_and_ctrl_c_cancel() {
        assert_eq!(picker(&["one"]).handle(Key::Cancel), Action::Cancel);
    }

    #[test]
    fn downward_frame_puts_the_newest_match_first() {
        let mut picker = picker(&["old", "middle", "new"]);
        let frame = picker.frame(40, 3, false, false);
        assert!(!frame.upward);
        assert!(plain(&frame.rows[0]).starts_with("> new"));
        assert!(plain(&frame.rows[1]).starts_with("  middle"));
        assert!(plain(&frame.rows[2]).starts_with("  old"));
    }

    #[test]
    fn upward_frame_puts_the_newest_match_nearest_the_query() {
        let mut picker = picker(&["old", "middle", "new"]);
        let frame = picker.frame(40, 3, true, false);
        assert!(frame.upward);
        // Rows are top-to-bottom; the selected/newest entry is at the bottom,
        // closest to the query line.
        assert!(plain(&frame.rows[0]).starts_with("  old"));
        assert!(plain(&frame.rows[1]).starts_with("  middle"));
        assert!(plain(&frame.rows[2]).starts_with("> new"));
    }

    #[test]
    fn unused_match_rows_are_padded_away_from_the_query() {
        let mut picker = picker(&["one", "two"]);
        let downward = picker.frame(40, 4, false, false);
        assert_eq!(plain(&downward.rows[2]), "");
        assert_eq!(plain(&downward.rows[3]), "");

        let upward = picker.frame(40, 4, true, false);
        assert_eq!(plain(&upward.rows[0]), "");
        assert_eq!(plain(&upward.rows[1]), "");
        assert!(plain(&upward.rows[2]).starts_with("  one"));
        assert!(plain(&upward.rows[3]).starts_with("> two"));
    }

    #[test]
    fn render_marks_the_selected_row_and_highlights_the_query() {
        let mut picker = picker(&["git status", "git log"]);
        for character in "git".chars() {
            picker.handle(Key::Char(character));
        }
        let frame = picker.frame(60, 2, false, true);
        let selected = &frame.rows[0];
        assert!(selected.contains(REVERSE));
        assert!(selected.contains(MATCH_ON));
        assert!(plain(selected).starts_with("> git log"));

        let unselected = &frame.rows[1];
        assert!(!unselected.contains(REVERSE));
        assert!(plain(unselected).starts_with("  git status"));
    }

    #[test]
    fn color_free_render_uses_a_marker_for_selection() {
        let mut picker = picker(&["git status", "git log"]);
        for upward in [false, true] {
            let frame = picker.frame(60, 2, upward, false);
            assert!(!frame.rows.iter().any(|line| line.contains('\x1b')));
            let selected = if upward {
                &frame.rows[1]
            } else {
                &frame.rows[0]
            };
            assert!(selected.starts_with("> git log"));
        }
    }

    #[test]
    fn frames_fit_the_terminal_width() {
        let long = "x".repeat(200);
        let mut picker = picker(&[long.as_str(), "short"]);
        picker.handle(Key::Char('x'));
        for width in [1, 2, 5, 20, 79] {
            for upward in [false, true] {
                let frame = picker.frame(width, 5, upward, true);
                assert!(
                    plain(&frame.query).chars().count() <= width,
                    "query {frame:?} exceeds width {width}"
                );
                for line in &frame.rows {
                    assert!(
                        plain(line).chars().count() <= width,
                        "line {line:?} exceeds width {width}"
                    );
                }
                assert!(frame.cursor_col < width.max(1));
            }
        }
    }

    #[test]
    fn query_line_shows_position_and_help() {
        let mut picker = picker(&["one", "two", "three"]);
        let wide = picker.frame(60, 3, false, false);
        assert!(plain(&wide.query).contains("1/3"));
        assert!(plain(&wide.query).contains("enter"));
        assert!(plain(&wide.query).contains("esc"));

        let medium = picker.frame(14, 3, false, false);
        assert!(plain(&medium.query).contains("1/3"));
        assert!(!plain(&medium.query).contains("enter"));

        let narrow = picker.frame(8, 3, false, false);
        assert_eq!(plain(&narrow.query).chars().count(), 8);
    }

    #[test]
    fn query_line_keeps_the_tail_of_a_long_query() {
        let mut picker = picker(&["abcdefghijklmnopqrstuvwxyz"]);
        for character in "abcdefghijklmnopqrstuvwxyz".chars() {
            picker.handle(Key::Char(character));
        }
        let frame = picker.frame(12, 4, false, false);
        let query = plain(&frame.query);
        // Six columns are left for the query after the prompt and the 1/1
        // status; the tail of the query is what stays visible.
        assert!(query.starts_with("> uvwxyz"));
        assert!(query.ends_with("1/1"));
        assert_eq!(frame.cursor_col, 8);
    }

    #[test]
    fn frames_are_exactly_as_tall_as_requested() {
        let mut picker = picker(&["one", "two"]);
        for rows in 1..=8 {
            for upward in [false, true] {
                let frame = picker.frame(40, rows, upward, false);
                assert_eq!(frame.rows.len(), rows);
            }
        }
    }

    #[test]
    fn list_area_scrolls_to_keep_the_selection_visible() {
        let entries: Vec<String> = (0..100).map(|n| format!("cmd {n:02}")).collect();
        let mut picker = HistoryPicker::new(entries, "");
        picker.handle(Key::End);
        let frame = picker.frame(40, 4, false, false);
        assert!(body(&frame).contains("> cmd 00"));
        assert_eq!(picker.offset(), 96);

        picker.handle(Key::Home);
        let frame = picker.frame(40, 4, false, false);
        assert!(body(&frame).contains("> cmd 99"));
        assert_eq!(picker.offset(), 0);

        // The same holds when the list is drawn upwards. The oldest visible
        // entries are at the top, so the selected oldest entry is row 0.
        picker.handle(Key::End);
        let frame = picker.frame(40, 4, true, false);
        assert!(body(&frame).contains("> cmd 00"));
        assert!(plain(&frame.rows[0]).starts_with("> cmd 00"));
    }

    #[test]
    fn newlines_are_flattened_and_control_characters_dropped() {
        let mut picker = picker(&["echo hi\nthere", "esc \x1b[31mred"]);
        let frame = picker.frame(60, 3, false, false);
        let text = body(&frame);
        assert!(text.contains("echo hi \u{23ce} there"));
        assert!(!text.contains('\x1b'));
        assert!(text.contains("esc [31mred"));
    }

    #[test]
    fn no_matches_message_sits_next_to_the_query() {
        let mut picker = picker_with(&["git status"], "zzz");
        let downward = picker.frame(50, 3, false, false);
        assert!(plain(&downward.rows[0]).contains("no matching commands"));
        assert!(plain(&downward.query).contains("0/0"));

        let upward = picker.frame(50, 3, true, false);
        assert!(plain(&upward.rows[2]).contains("no matching commands"));
    }

    #[test]
    fn empty_history_is_reported_in_the_ui() {
        let mut picker = picker(&[]);
        let frame = picker.frame(50, 3, false, false);
        assert!(body(&frame).contains("no history yet"));
        assert_eq!(picker.handle(Key::Enter), Action::Continue);
    }

    #[test]
    fn display_text_sanitizes_each_entry() {
        assert_eq!(display_text("a\tb"), "a  b");
        assert_eq!(display_text("a\nb"), "a \u{23ce} b");
        assert_eq!(display_text("a\x07b"), "ab");
        assert_eq!(display_text("plain"), "plain");
    }

    #[test]
    fn fit_crops_with_an_ellipsis() {
        assert_eq!(fit("hello", 10), "hello");
        assert_eq!(fit("hello", 5), "hello");
        assert_eq!(fit("hello", 4), "hel\u{2026}");
        assert_eq!(fit("hello", 1), "\u{2026}");
        assert_eq!(fit("hello", 0), "");
    }

    #[test]
    fn split_match_finds_ascii_case_insensitively() {
        assert_eq!(split_match("Git Status", "git"), ("", "Git", " Status"));
        assert_eq!(split_match("run Git now", "GIT"), ("run ", "Git", " now"));
        assert_eq!(split_match("nothing", "zzz"), ("nothing", "", ""));
        assert_eq!(split_match("anything", ""), ("anything", "", ""));
    }

    #[test]
    fn cursor_replies_are_separated_from_typed_input() {
        assert_eq!(cursor_reply(b"\x1b[24;1R"), Some((0, (24, 1))));
        assert_eq!(cursor_reply(b"bet\x1b[7;12R"), Some((3, (7, 12))));
        assert_eq!(cursor_reply(b"\x1b[24;1"), None);
        assert_eq!(cursor_reply(b"typed"), None);
        assert_eq!(cursor_reply(b"\x1b[?24;1R"), None);
    }

    #[test]
    fn prompt_height_accounts_for_wrapping_and_hints() {
        assert_eq!(prompt_height("\x1b[32m\u{27a4}\x1b[0m ", "", 0, 10), 1);
        assert_eq!(prompt_height("12345678", "ab", 0, 10), 1);
        assert_eq!(prompt_height("12345678", "ab", 1, 10), 2);
        assert_eq!(prompt_height("123456789", "ab", 0, 10), 2);
        assert_eq!(prompt_height("12345", "6789012345", 0, 10), 2);
        assert_eq!(prompt_height("> ", "a\nb", 0, 10), 2);
    }

    #[test]
    fn prompt_row_is_one_row_above_the_cursor() {
        assert_eq!(prompt_row_from_cursor(24, 24), 22);
        assert_eq!(prompt_row_from_cursor(5, 24), 3);
        assert_eq!(prompt_row_from_cursor(1, 24), 0);
        assert_eq!(prompt_row_from_cursor(2, 24), 0);
    }

    #[test]
    fn layout_opens_downwards_when_there_is_room_below() {
        let layout = layout(24, 5, 5);
        assert!(!layout.upward);
        assert!(!layout.prompt_visible);
        assert_eq!(layout.scrolled, 0);
        assert_eq!(layout.popup_start, 5);
        assert_eq!(layout.popup_end, 10);
        assert_eq!(layout.query_row, 5);
        assert_eq!(layout.match_start, 6);
    }

    #[test]
    fn layout_keeps_the_prompt_visible_at_the_bottom() {
        let layout = layout(24, 23, 5);
        assert!(layout.upward);
        assert!(layout.prompt_visible);
        assert_eq!(layout.scrolled, 6);
        assert_eq!(layout.popup_start, 17);
        assert_eq!(layout.popup_end, 22);
        assert_eq!(layout.query_row, 22);
        assert_eq!(layout.match_start, 17);
    }

    #[test]
    fn layout_keeps_the_prompt_visible_near_the_bottom() {
        let layout = layout(24, 20, 5);
        assert!(layout.upward);
        assert!(layout.prompt_visible);
        assert_eq!(layout.scrolled, 6);
        assert_eq!(layout.popup_start, 14);
        assert_eq!(layout.popup_end, 19);
        assert_eq!(layout.query_row, 19);
        assert_eq!(layout.match_start, 14);
    }

    #[test]
    fn layout_uses_the_fallback_when_there_is_no_room_above_the_prompt() {
        let layout = layout(4, 2, 2);
        assert!(layout.upward);
        assert!(!layout.prompt_visible);
        assert_eq!(layout.scrolled, 1);
        assert_eq!(layout.popup_start, 1);
        assert_eq!(layout.popup_end, 3);
        assert_eq!(layout.query_row, 3);
        assert_eq!(layout.match_start, 1);
    }

    #[test]
    fn layout_handles_tiny_terminals() {
        let bottom = layout(3, 2, 1);
        assert!(bottom.upward);
        assert!(bottom.prompt_visible);
        assert_eq!(bottom.scrolled, 2);
        assert_eq!(bottom.popup_start, 0);
        assert_eq!(bottom.popup_end, 1);
        assert_eq!(bottom.query_row, 1);
        assert_eq!(bottom.match_start, 0);

        let top = layout(3, 1, 1);
        assert!(!top.upward);
        assert!(!top.prompt_visible);
        assert_eq!(top.scrolled, 0);
        assert_eq!(top.popup_start, 1);
        assert_eq!(top.query_row, 1);
    }
}
