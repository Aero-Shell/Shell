//! A searchable, navigable history picker.
//!
//! The REPL hands control here when the user asks for history search (Ctrl-R,
//! or Up on a partial command). The picker filters the whole history as the
//! query is typed, moves through the matches with the arrow keys, and hands
//! the chosen command back to the prompt for editing.
//!
//! The state machine and drawing are kept apart from the terminal layer
//! (`pick`) so matching, navigation and layout can be tested without a TTY.

use std::io::{self, IsTerminal, Write};

use aeroshell::config;

// Terminal styling for the picker itself, independent of the prompt theme.
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const REVERSE: &str = "\x1b[7m";
const REVERSE_OFF: &str = "\x1b[27m";
const MATCH_ON: &str = "\x1b[1;4m";
const MATCH_OFF: &str = "\x1b[22;24m";

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

/// One rendered picker frame.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub lines: Vec<String>,
    /// Zero-based (row, column) where the terminal cursor belongs.
    pub cursor: (usize, usize),
}

/// The picker's state machine: query, matches, selection and scrolling.
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
            page: 10,
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

    /// Draw one frame. `height` includes the header, query line, separator and
    /// footer; whatever is left is the scrolling match window. Visible lines
    /// never exceed `width` columns, so the frame cannot wrap and damage the
    /// full-screen layout.
    pub fn render(&mut self, width: usize, height: usize, color: bool) -> Frame {
        let width = width.max(1);
        let height = height.max(1);

        let mut lines: Vec<String> = Vec::with_capacity(height);
        if height >= 4 {
            lines.push(self.header_line(width, color));
        }
        let query_row = lines.len();
        let (query_line, cursor_col) = self.query_line(width, color);
        lines.push(query_line);
        if height >= 3 {
            lines.push(self.separator_line(width, color));
        }

        let footer_reserved = height > lines.len();
        let list_rows = height.saturating_sub(lines.len() + usize::from(footer_reserved));
        self.page = list_rows;

        if list_rows > 0 {
            self.ensure_visible(list_rows);
            let mut drawn = 0;
            for slot in 0..list_rows {
                if let Some(&index) = self.matching.get(self.offset + slot) {
                    let selected = self.offset + slot == self.selected;
                    lines.push(self.row_line(&self.rows[index], selected, width, color));
                    drawn += 1;
                } else if slot == 0 && self.matching.is_empty() {
                    let message = if self.rows.is_empty() {
                        "  no history yet"
                    } else {
                        "  no matching commands"
                    };
                    lines.push(style(fit(message, width), DIM, color));
                    drawn += 1;
                } else {
                    break;
                }
            }
            // Keep the frame exactly `height` rows so nothing stale is left
            // behind when a later frame is shorter.
            for _ in drawn..list_rows {
                lines.push(String::new());
            }
        }

        if footer_reserved {
            lines.push(self.footer_line(width, color));
        }
        while lines.len() < height {
            lines.push(String::new());
        }
        lines.truncate(height);

        Frame {
            lines,
            cursor: (query_row, cursor_col),
        }
    }

    fn ensure_visible(&mut self, list_rows: usize) {
        let list_rows = list_rows.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + list_rows {
            self.offset = self.selected + 1 - list_rows;
        }
        let max_offset = self.matching.len().saturating_sub(list_rows);
        self.offset = self.offset.min(max_offset);
    }

    fn header_line(&self, width: usize, color: bool) -> String {
        let title = " Ash history search ";
        let counts = format!("{}/{} ", self.matching.len(), self.rows.len());
        if title.chars().count() + counts.chars().count() <= width {
            let padding = width - title.chars().count() - counts.chars().count();
            style(
                format!("{title}{}{counts}", " ".repeat(padding)),
                BOLD,
                color,
            )
        } else {
            style(fit(&format!("{title}{counts}"), width), BOLD, color)
        }
    }

    fn query_line(&self, width: usize, color: bool) -> (String, usize) {
        let prefix = if width >= 2 { "> " } else { "" };
        let available = width - prefix.chars().count();
        let total = self.query.chars().count();
        let shown: String = if total <= available {
            self.query.clone()
        } else {
            // Keep the end of a long query visible, like a text field.
            self.query.chars().skip(total - available).collect()
        };
        let cursor = (prefix.chars().count() + shown.chars().count()).min(width - 1);
        let mut line = String::new();
        if color && !prefix.is_empty() {
            line.push_str(BOLD);
            line.push_str(prefix);
            line.push_str(RESET);
        } else {
            line.push_str(prefix);
        }
        line.push_str(&shown);
        (line, cursor)
    }

    fn separator_line(&self, width: usize, color: bool) -> String {
        style("─".repeat(width), DIM, color)
    }

    fn footer_line(&self, width: usize, color: bool) -> String {
        let position = if self.matching.is_empty() {
            "0/0".to_string()
        } else {
            format!("{}/{}", self.selected + 1, self.matching.len())
        };
        let text =
            format!("  {position}   \u{2191}\u{2193} move \u{00b7} enter edit \u{00b7} esc cancel");
        style(fit(&text, width), DIM, color)
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

/// Run the picker on the terminal. Returns `Unavailable` when there is nothing
/// to search or stdin/stdout are not a TTY, so the caller can keep the normal
/// prompt without opening an empty screen.
pub fn pick(entries: Vec<String>, initial: &str) -> Picked {
    if entries.is_empty() || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Picked::Unavailable;
    }
    let Some(mut terminal) = Terminal::enter() else {
        return Picked::Unavailable;
    };
    let color = config::active_config().map(|cfg| cfg.color).unwrap_or(true);
    let mut picker = HistoryPicker::new(entries, initial);
    loop {
        let (width, height) = terminal_size();
        let frame = picker.render(width, height, color);
        if terminal.draw(&frame).is_err() {
            return Picked::Cancelled;
        }
        let Ok(key) = terminal.read_key() else {
            return Picked::Cancelled;
        };
        match picker.handle(key) {
            Action::Continue => {}
            Action::Accept(entry) => return Picked::Entry(entry),
            Action::Cancel => return Picked::Cancelled,
        }
    }
}

/// Clear the prompt line that the picker interrupted, so the resumed prompt
/// replaces it instead of leaving a stale copy in the transcript. Only acts
/// when the prompt and its text fit on one terminal row; a wrapped or
/// multi-line edit is left exactly as the line editor drew it.
pub fn erase_prompt_line(prompt: &str, line: &str, hint_len: usize) {
    if line.contains('\n') || prompt.contains('\n') {
        return;
    }
    let (width, _) = terminal_size();
    if visible_width(prompt) + line.chars().count() + hint_len >= width {
        return;
    }
    let mut stdout = io::stdout();
    let _ = stdout.write_all(b"\x1b[1A\r\x1b[2K");
    let _ = stdout.flush();
}

/// Width of `text` on screen, ignoring ANSI escape sequences.
fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut escape = false;
    for character in text.chars() {
        if escape {
            if character.is_ascii_alphabetic() {
                escape = false;
            }
        } else if character == '\x1b' {
            escape = true;
        } else {
            width += 1;
        }
    }
    width
}

/// Raw mode plus the alternate screen, restored on drop (including panics).
struct Terminal {
    original: libc::termios,
}

impl Terminal {
    fn enter() -> Option<Self> {
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return None;
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) } != 0 {
            return None;
        }
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[?1049h\x1b[?25l\x1b[H\x1b[2J");
        let _ = stdout.flush();
        Some(Self { original })
    }

    fn draw(&mut self, frame: &Frame) -> io::Result<()> {
        let mut out = String::from("\x1b[?25l\x1b[H");
        for (index, line) in frame.lines.iter().enumerate() {
            out.push_str("\x1b[2K");
            out.push_str(line);
            if index + 1 < frame.lines.len() {
                out.push_str("\r\n");
            }
        }
        out.push_str(&format!(
            "\x1b[{};{}H\x1b[?25h",
            frame.cursor.0 + 1,
            frame.cursor.1 + 1
        ));
        let mut stdout = io::stdout();
        stdout.write_all(out.as_bytes())?;
        stdout.flush()
    }

    fn read_key(&mut self) -> io::Result<Key> {
        let byte = read_byte()?;
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

    /// Parse the rest of an escape sequence. A lone Esc cancels; terminal
    /// arrows and paging keys arrive as CSI/SS3 sequences.
    fn read_escape(&mut self) -> io::Result<Key> {
        if !wait_readable(20) {
            return Ok(Key::Cancel);
        }
        let first = read_byte()?;
        match first {
            b'[' => {
                let mut sequence = vec![b'['];
                loop {
                    if sequence.len() > 8 {
                        return Ok(Key::Ignore);
                    }
                    let byte = read_byte()?;
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
                let byte = read_byte()?;
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
            bytes.push(read_byte()?);
        }
        Ok(std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.chars().next())
            .map_or(Key::Ignore, Key::Char))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[?25h\x1b[?1049l");
        let _ = stdout.flush();
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.original);
        }
    }
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

    fn body(frame: &Frame) -> String {
        frame
            .lines
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

        let frame = picker.render(40, 8, false);
        let text = body(&frame);
        assert!(text.find("new").unwrap() < text.find("middle").unwrap());
        assert!(text.find("middle").unwrap() < text.find("old").unwrap());
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
        // Height 10 leaves six list rows: header, query, separator, footer.
        picker.render(40, 10, false);
        assert_eq!(picker.page, 6);

        picker.handle(Key::PageDown);
        assert_eq!(picker.selected(), 6);
        picker.handle(Key::PageDown);
        assert_eq!(picker.selected(), 12);
        picker.handle(Key::PageUp);
        assert_eq!(picker.selected(), 6);
        picker.handle(Key::End);
        assert_eq!(picker.selected(), 29);
        picker.handle(Key::Home);
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn changing_the_query_resets_selection_and_scroll() {
        let entries: Vec<String> = (0..30).map(|n| format!("cmd {n}")).collect();
        let mut picker = HistoryPicker::new(entries, "");
        picker.render(40, 10, false);
        picker.handle(Key::PageDown);
        assert_eq!(picker.selected(), 6);
        picker.render(40, 10, false);
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
    fn render_marks_the_selected_row_and_highlights_the_query() {
        let mut picker = picker(&["git status", "git log"]);
        for character in "git".chars() {
            picker.handle(Key::Char(character));
        }
        let frame = picker.render(60, 8, true);
        let selected = &frame.lines[3];
        assert!(selected.contains(REVERSE));
        assert!(selected.contains(MATCH_ON));
        assert!(plain(selected).starts_with("> git log"));

        let unselected = &frame.lines[4];
        assert!(!unselected.contains(REVERSE));
        assert!(plain(unselected).starts_with("  git status"));
    }

    #[test]
    fn color_free_render_uses_a_marker_for_selection() {
        let mut picker = picker(&["git status", "git log"]);
        let frame = picker.render(60, 8, false);
        assert!(!frame.lines.iter().any(|line| line.contains('\x1b')));
        assert!(frame.lines[3].starts_with("> git log"));
        assert!(frame.lines[4].starts_with("  git status"));
    }

    #[test]
    fn frames_fit_the_terminal_width() {
        let long = "x".repeat(200);
        let mut picker = picker(&[long.as_str(), "short"]);
        picker.handle(Key::Char('x'));
        for width in [1, 2, 5, 20, 79] {
            let frame = picker.render(width, 10, true);
            for line in &frame.lines {
                assert!(
                    plain(line).chars().count() <= width,
                    "line {line:?} exceeds width {width}"
                );
            }
            assert!(frame.cursor.1 < width.max(1));
        }
    }

    #[test]
    fn long_queries_show_their_tail() {
        let mut picker = picker(&["unrelated"]);
        for character in "abcdefghijklmnopqrstuvwxyz".chars() {
            picker.handle(Key::Char(character));
        }
        let frame = picker.render(12, 6, false);
        let query_line = plain(&frame.lines[1]);
        assert!(query_line.starts_with("> "));
        assert!(query_line.ends_with("xyz"));
        assert_eq!(frame.cursor, (1, 11));
    }

    #[test]
    fn frames_are_exactly_the_terminal_height() {
        let mut picker = picker(&["one", "two"]);
        for height in 1..=12 {
            let frame = picker.render(40, height, false);
            assert_eq!(frame.lines.len(), height, "height {height}");
        }
    }

    #[test]
    fn list_area_scrolls_to_keep_the_selection_visible() {
        let entries: Vec<String> = (0..100).map(|n| format!("cmd {n:02}")).collect();
        let mut picker = HistoryPicker::new(entries, "");
        picker.handle(Key::End);
        let frame = picker.render(40, 8, false);
        let text = body(&frame);
        assert!(text.contains("> cmd 00"));
        assert_eq!(picker.offset(), 96);

        picker.handle(Key::Home);
        let frame = picker.render(40, 8, false);
        assert!(body(&frame).contains("> cmd 99"));
        assert_eq!(picker.offset(), 0);
    }

    #[test]
    fn newlines_are_flattened_and_control_characters_dropped() {
        let mut picker = picker(&["echo hi\nthere", "esc \x1b[31mred"]);
        let frame = picker.render(60, 8, false);
        let text = body(&frame);
        assert!(text.contains("echo hi \u{23ce} there"));
        assert!(!text.contains('\x1b'));
        assert!(text.contains("esc [31mred"));
    }

    #[test]
    fn no_matches_shows_a_message_and_zero_counts() {
        let mut picker = picker_with(&["git status"], "zzz");
        let frame = picker.render(50, 8, false);
        let text = body(&frame);
        assert!(text.contains("no matching commands"));
        assert!(text.contains("0/0"));
        assert!(text.contains("0/1"));
    }

    #[test]
    fn empty_history_is_reported_in_the_ui() {
        let mut picker = picker(&[]);
        let frame = picker.render(50, 8, false);
        assert!(body(&frame).contains("no history yet"));
        assert_eq!(picker.handle(Key::Enter), Action::Continue);
    }

    #[test]
    fn header_and_footer_show_counts_and_help() {
        let mut picker = picker(&["one", "two", "three"]);
        let frame = picker.render(60, 8, false);
        assert!(body(&frame).contains("3/3"));
        assert!(body(&frame).contains("1/3"));
        assert!(body(&frame).contains("enter edit"));
        assert!(body(&frame).contains("esc cancel"));
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
    fn visible_width_ignores_escape_sequences() {
        assert_eq!(visible_width("plain"), 5);
        assert_eq!(visible_width("\x1b[32m➜\x1b[0m ~ $"), 5);
        assert_eq!(visible_width("\x1b[1;36mheader\x1b[0m"), 6);
        assert_eq!(visible_width(""), 0);
    }

    #[test]
    fn split_match_finds_ascii_case_insensitively() {
        assert_eq!(split_match("Git Status", "git"), ("", "Git", " Status"));
        assert_eq!(split_match("run Git now", "GIT"), ("run ", "Git", " now"));
        assert_eq!(split_match("nothing", "zzz"), ("nothing", "", ""));
        assert_eq!(split_match("anything", ""), ("anything", "", ""));
    }
}
