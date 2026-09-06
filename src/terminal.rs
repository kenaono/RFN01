//! The screen the shell writes on (技術検証 9.3 calls this half `terminal-core`).
//!
//! **Nothing here knows about Windows**, and nothing here draws. Bytes go in;
//! what comes out is a grid of cells, a cursor, a scrollback, and now and then a
//! short reply the caller is expected to send back up the pipe. That is the
//! whole of the interface, and it is why this half can be tested without a
//! console, a window, or a shell — the tests below are driven by the bytes a
//! real `wsl.exe` sent through `pty.rs` (技術検証 9.4).
//!
//! # Why a grid and not the text engine
//!
//! The editor lays out flowing text: a block is a run between hard breaks, it
//! wraps to the pane, and its height is whatever DirectWrite says. **A terminal
//! is none of that.** What arrives is not a document but a series of repaints —
//! hide the cursor, put it at row 4 column 1, write these glyphs, show the
//! cursor — and the same cell is written over and over. The unit is a cell at a
//! fixed row and column, so the model here is an array of them.
//!
//! The *drawing* is still the editor's (Direct2D, DirectWrite, the tile cache,
//! the display settings). It is the layout that is different.
//!
//! # What a terminal owes the shell
//!
//! Some sequences are questions: "where is the cursor", "what are you". A
//! terminal that never answers leaves programs waiting, so answers accumulate in
//! [`Screen::take_replies`] and the caller writes them to the pty. **This is the
//! one direction that flows backwards**, and keeping it as bytes-in-a-vec rather
//! than a callback is what lets the tests see it.

#![allow(dead_code)]

use std::collections::VecDeque;

/// A colour as the shell named it.
///
/// **The names are not resolved here.** `Indexed(2)` is "the palette's green",
/// and which green that is belongs to the display settings (要件9), not to the
/// stream. Keeping the name means a palette change repaints correctly instead of
/// needing the last screenful re-parsed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Everything about a cell except which character it holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Attrs {
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: bool,
    pub blink: bool,
    /// Foreground and background swap when drawn. **Not resolved here** for the
    /// same reason the colours are not: the swap depends on what `Default`
    /// turns out to be.
    pub reverse: bool,
    pub hidden: bool,
    pub struck: bool,
}

/// One cell of the screen.
///
/// A full-width character (要件の読者が毎日使う日本語がまさにこれ) occupies two
/// cells: the character sits in the first and the second is a [`Cell::trailing`]
/// spacer. **The spacer is a real cell**, not an absence — the cursor can be put
/// on it, and erasing either half has to clear both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub text: char,
    pub attrs: Attrs,
    pub trailing: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: ' ',
            attrs: Attrs::default(),
            trailing: false,
        }
    }
}

impl Cell {
    fn blank(attrs: Attrs) -> Self {
        Self {
            text: ' ',
            attrs,
            trailing: false,
        }
    }

    /// The cell holds nothing a reader would see.
    pub fn is_blank(&self) -> bool {
        self.text == ' ' && !self.trailing
    }
}

/// One row of cells.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Line {
    pub cells: Vec<Cell>,
    /// The line ran off the right edge and continues on the next one.
    /// **Copying a wrapped line should not insert a newline**, which is the
    /// only reason this is remembered.
    pub wrapped: bool,
}

impl Line {
    fn blank(columns: usize, attrs: Attrs) -> Self {
        Self {
            cells: vec![Cell::blank(attrs); columns],
            wrapped: false,
        }
    }

    /// The row as a reader sees it, without the trailing blanks.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for cell in &self.cells {
            if !cell.trailing {
                out.push(cell.text);
            }
        }
        while out.ends_with(' ') {
            out.pop();
        }
        out
    }
}

/// Where the next character goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
    /// **The last column is written, and only then does the line wrap.**
    /// A terminal that wraps as soon as the last column is filled would break
    /// every program that draws a full-width box: writing the bottom-right
    /// corner would scroll the screen by one line.
    pending_wrap: bool,
}

/// The modes the shell has asked for.
///
/// **These are recorded, not obeyed.** Whether the pane sends bracketed paste
/// markers or focus events is a question for the pane; what this half can do is
/// say what was asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Modes {
    pub cursor_visible: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    /// `ESC[?9001h` — conhost asks for Win32 input records instead of VT input.
    /// **We record it and keep sending VT**, which conhost accepts; honouring it
    /// would mean encoding key events in a Windows-only form in the half that is
    /// supposed to be portable.
    pub win32_input: bool,
    pub application_cursor_keys: bool,
    pub mouse_tracking: bool,
    pub alternate_screen: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            cursor_visible: true,
            bracketed_paste: false,
            focus_events: false,
            win32_input: false,
            application_cursor_keys: false,
            mouse_tracking: false,
            alternate_screen: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SavedCursor {
    cursor: Cursor,
    pen: Attrs,
}

/// The screen, its scrollback, and everything the stream has said about them.
#[derive(Debug)]
pub struct Screen {
    columns: usize,
    rows: usize,
    lines: Vec<Line>,
    /// The primary screen, held while the alternate one is showing.
    stowed: Option<Vec<Line>>,
    scrollback: VecDeque<Line>,
    scrollback_limit: usize,
    cursor: Cursor,
    saved: Option<SavedCursor>,
    pen: Attrs,
    /// Rows the scroll happens between, both ends inclusive (DECSTBM).
    region: (usize, usize),
    tab_stops: Vec<bool>,
    modes: Modes,
    title: String,
    replies: Vec<u8>,
    /// Bumped whenever anything visible changes, so a pane can tell in one
    /// comparison whether it has to draw. **Cheaper than diffing the grid** and
    /// exactly as accurate for the question "is what I drew still current".
    revision: u64,
}

impl Screen {
    pub fn new(columns: usize, rows: usize) -> Self {
        let columns = columns.max(1);
        let rows = rows.max(1);
        Self {
            columns,
            rows,
            lines: vec![Line::blank(columns, Attrs::default()); rows],
            stowed: None,
            scrollback: VecDeque::new(),
            scrollback_limit: 10_000,
            cursor: Cursor::default(),
            saved: None,
            pen: Attrs::default(),
            region: (0, rows - 1),
            tab_stops: (0..columns).map(|column| column % 8 == 0).collect(),
            modes: Modes::default(),
            title: String::new(),
            replies: Vec::new(),
            revision: 0,
        }
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn modes(&self) -> Modes {
        self.modes
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn line(&self, row: usize) -> Option<&Line> {
        self.lines.get(row)
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// The rows that scrolled off the top, oldest first.
    ///
    /// **Empty while the alternate screen is showing** — `vim` scrolling its own
    /// window must not push anything into the history of the shell session.
    pub fn scrollback(&self) -> &VecDeque<Line> {
        &self.scrollback
    }

    /// What the shell asked for and has not been told yet. Send it up the pty.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// The row as a reader sees it. For tests and for copying.
    pub fn row_text(&self, row: usize) -> String {
        self.lines.get(row).map(Line::text).unwrap_or_default()
    }

    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    // --- what the parser calls ------------------------------------------------

    fn print(&mut self, text: char) {
        let width = character_width(text);
        if width == 0 {
            // A combining mark. **Dropping it is wrong and drawing it as its own
            // cell is worse** — it would shift everything after it. Until cells
            // can hold more than one `char`, it joins nothing and is skipped.
            return;
        }
        if self.cursor.pending_wrap || self.cursor.column + width > self.columns {
            self.wrap_line();
        }
        let attrs = self.pen;
        let row = self.cursor.row;
        let column = self.cursor.column;
        self.clear_pair_at(row, column);
        if width == 2 {
            self.clear_pair_at(row, column + 1);
        }
        {
            let cells = &mut self.lines[row].cells;
            cells[column] = Cell {
                text,
                attrs,
                trailing: false,
            };
            if width == 2 {
                cells[column + 1] = Cell {
                    text: ' ',
                    attrs,
                    trailing: true,
                };
            }
        }
        self.cursor.column += width;
        if self.cursor.column >= self.columns {
            self.cursor.column = self.columns - 1;
            self.cursor.pending_wrap = true;
        }
        self.touch();
    }

    /// Writing over half of a full-width character destroys the other half.
    /// **Leaving the orphan would shift the row by one cell** for as long as it
    /// stayed.
    fn clear_pair_at(&mut self, row: usize, column: usize) {
        let attrs = self.pen;
        let cells = &mut self.lines[row].cells;
        if cells[column].trailing && column > 0 {
            cells[column - 1] = Cell::blank(attrs);
        } else if column + 1 < cells.len()
            && cells[column + 1].trailing
            && character_width(cells[column].text) == 2
        {
            cells[column + 1] = Cell::blank(attrs);
        }
    }

    fn wrap_line(&mut self) {
        self.lines[self.cursor.row].wrapped = true;
        self.cursor.column = 0;
        self.cursor.pending_wrap = false;
        self.line_feed();
    }

    fn line_feed(&mut self) {
        if self.cursor.row == self.region.1 {
            self.scroll_up(1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
        self.cursor.pending_wrap = false;
        self.touch();
    }

    fn reverse_index(&mut self) {
        if self.cursor.row == self.region.0 {
            self.scroll_down(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
        self.touch();
    }

    fn carriage_return(&mut self) {
        self.cursor.column = 0;
        self.cursor.pending_wrap = false;
        self.touch();
    }

    fn backspace(&mut self) {
        if self.cursor.pending_wrap {
            self.cursor.pending_wrap = false;
        } else if self.cursor.column > 0 {
            self.cursor.column -= 1;
        }
        self.touch();
    }

    fn tab(&mut self) {
        let mut column = self.cursor.column + 1;
        while column < self.columns && !self.tab_stops[column] {
            column += 1;
        }
        self.cursor.column = column.min(self.columns - 1);
        self.cursor.pending_wrap = false;
        self.touch();
    }

    fn move_to(&mut self, row: usize, column: usize) {
        self.cursor.row = row.min(self.rows - 1);
        self.cursor.column = column.min(self.columns - 1);
        self.cursor.pending_wrap = false;
        self.touch();
    }

    fn move_by(&mut self, rows: isize, columns: isize) {
        let row = (self.cursor.row as isize + rows).clamp(0, self.rows as isize - 1);
        let column = (self.cursor.column as isize + columns).clamp(0, self.columns as isize - 1);
        self.move_to(row as usize, column as usize);
    }

    /// Move `count` lines out of the top of the scroll region.
    ///
    /// **Only lines leaving the top of the whole screen are history.** A program
    /// that set a scroll region is drawing inside a window of its own, and what
    /// falls out of that window was never part of the session's output.
    fn scroll_up(&mut self, count: usize) {
        let (top, bottom) = self.region;
        let count = count.min(bottom - top + 1);
        for _ in 0..count {
            let leaving = self.lines.remove(top);
            if top == 0 && self.stowed.is_none() {
                self.scrollback.push_back(leaving);
                while self.scrollback.len() > self.scrollback_limit {
                    self.scrollback.pop_front();
                }
            }
            self.lines
                .insert(bottom, Line::blank(self.columns, self.pen));
        }
        self.touch();
    }

    fn scroll_down(&mut self, count: usize) {
        let (top, bottom) = self.region;
        let count = count.min(bottom - top + 1);
        for _ in 0..count {
            self.lines.remove(bottom);
            self.lines.insert(top, Line::blank(self.columns, self.pen));
        }
        self.touch();
    }

    fn erase_in_display(&mut self, mode: u16) {
        let attrs = self.pen;
        let (row, column) = (self.cursor.row, self.cursor.column);
        match mode {
            0 => {
                self.erase_in_line(0);
                for line in &mut self.lines[row + 1..] {
                    *line = Line::blank(self.columns, attrs);
                }
            }
            1 => {
                self.erase_in_line(1);
                for line in &mut self.lines[..row] {
                    *line = Line::blank(self.columns, attrs);
                }
            }
            2 => {
                for line in &mut self.lines {
                    *line = Line::blank(self.columns, attrs);
                }
            }
            3 => self.scrollback.clear(),
            _ => {}
        }
        // **The cursor does not move.** `ESC[2J` clears the screen and leaves
        // the caret where it was; the `ESC[H` that so often follows is what
        // moves it, and a shell that omits it means what it omitted.
        let _ = column;
        self.touch();
    }

    fn erase_in_line(&mut self, mode: u16) {
        let attrs = self.pen;
        let column = self.cursor.column;
        let columns = self.columns;
        let cells = &mut self.lines[self.cursor.row].cells;
        let range = match mode {
            0 => column..columns,
            1 => 0..(column + 1).min(columns),
            2 => 0..columns,
            _ => 0..0,
        };
        for cell in &mut cells[range] {
            *cell = Cell::blank(attrs);
        }
        self.touch();
    }

    fn erase_cells(&mut self, count: usize) {
        let attrs = self.pen;
        let column = self.cursor.column;
        let end = (column + count.max(1)).min(self.columns);
        for cell in &mut self.lines[self.cursor.row].cells[column..end] {
            *cell = Cell::blank(attrs);
        }
        self.touch();
    }

    fn insert_cells(&mut self, count: usize) {
        let attrs = self.pen;
        let column = self.cursor.column;
        let columns = self.columns;
        let cells = &mut self.lines[self.cursor.row].cells;
        for _ in 0..count.max(1).min(columns - column) {
            cells.insert(column, Cell::blank(attrs));
            cells.truncate(columns);
        }
        self.touch();
    }

    fn delete_cells(&mut self, count: usize) {
        let attrs = self.pen;
        let column = self.cursor.column;
        let columns = self.columns;
        let cells = &mut self.lines[self.cursor.row].cells;
        for _ in 0..count.max(1).min(columns - column) {
            cells.remove(column);
            cells.push(Cell::blank(attrs));
        }
        self.touch();
    }

    fn insert_lines(&mut self, count: usize) {
        let (top, bottom) = self.region;
        if self.cursor.row < top || self.cursor.row > bottom {
            return;
        }
        let attrs = self.pen;
        for _ in 0..count.max(1).min(bottom - self.cursor.row + 1) {
            self.lines.remove(bottom);
            self.lines
                .insert(self.cursor.row, Line::blank(self.columns, attrs));
        }
        self.touch();
    }

    fn delete_lines(&mut self, count: usize) {
        let (top, bottom) = self.region;
        if self.cursor.row < top || self.cursor.row > bottom {
            return;
        }
        let attrs = self.pen;
        for _ in 0..count.max(1).min(bottom - self.cursor.row + 1) {
            self.lines.remove(self.cursor.row);
            self.lines.insert(bottom, Line::blank(self.columns, attrs));
        }
        self.touch();
    }

    fn set_region(&mut self, top: usize, bottom: usize) {
        let bottom = bottom.min(self.rows - 1);
        if top < bottom {
            self.region = (top, bottom);
            self.move_to(0, 0);
        }
    }

    fn save_cursor(&mut self) {
        self.saved = Some(SavedCursor {
            cursor: self.cursor,
            pen: self.pen,
        });
    }

    fn restore_cursor(&mut self) {
        if let Some(saved) = self.saved {
            self.cursor = saved.cursor;
            self.pen = saved.pen;
            self.cursor.row = self.cursor.row.min(self.rows - 1);
            self.cursor.column = self.cursor.column.min(self.columns - 1);
            self.touch();
        }
    }

    /// Swap in the alternate screen (`ESC[?1049h`) or give the first one back.
    ///
    /// **This is what makes `vim` survivable.** The editor's screenful is drawn
    /// on a blank sheet, and when it leaves, the shell's own scrollback is
    /// exactly as it was — no repaint, nothing lost.
    fn use_alternate_screen(&mut self, alternate: bool) {
        if alternate == self.stowed.is_some() {
            return;
        }
        if alternate {
            self.save_cursor();
            let primary = std::mem::replace(
                &mut self.lines,
                vec![Line::blank(self.columns, Attrs::default()); self.rows],
            );
            self.stowed = Some(primary);
            self.cursor = Cursor::default();
        } else if let Some(primary) = self.stowed.take() {
            self.lines = primary;
            self.restore_cursor();
        }
        self.modes.alternate_screen = alternate;
        self.touch();
    }

    /// Give the screen a new size (要件6.4: a pane can be dragged).
    ///
    /// **Nothing is re-wrapped.** Reflowing the history is a separate question
    /// with its own answer; what a resize must not do is lose the rows the
    /// reader can see, so shrinking pushes the top into the scrollback rather
    /// than dropping the bottom.
    pub fn resize(&mut self, columns: usize, rows: usize) {
        let columns = columns.max(1);
        let rows = rows.max(1);
        if columns == self.columns && rows == self.rows {
            return;
        }
        for line in &mut self.lines {
            line.cells.resize(columns, Cell::blank(Attrs::default()));
        }
        while self.lines.len() > rows {
            if self.cursor.row + 1 >= self.lines.len() {
                let leaving = self.lines.remove(0);
                if self.stowed.is_none() {
                    self.scrollback.push_back(leaving);
                }
                self.cursor.row = self.cursor.row.saturating_sub(1);
            } else {
                self.lines.pop();
            }
        }
        while self.lines.len() < rows {
            self.lines.push(Line::blank(columns, Attrs::default()));
        }
        if let Some(stowed) = &mut self.stowed {
            for line in stowed.iter_mut() {
                line.cells.resize(columns, Cell::blank(Attrs::default()));
            }
            stowed.resize(rows, Line::blank(columns, Attrs::default()));
        }
        self.columns = columns;
        self.rows = rows;
        self.tab_stops = (0..columns).map(|column| column % 8 == 0).collect();
        self.region = (0, rows - 1);
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.column = self.cursor.column.min(columns - 1);
        self.cursor.pending_wrap = false;
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }
        self.touch();
    }

    fn reset(&mut self) {
        let (columns, rows) = (self.columns, self.rows);
        let scrollback = std::mem::take(&mut self.scrollback);
        let limit = self.scrollback_limit;
        *self = Self::new(columns, rows);
        self.scrollback = scrollback;
        self.scrollback_limit = limit;
    }

    fn select_graphic_rendition(&mut self, params: &[Vec<u16>]) {
        if params.is_empty() {
            self.pen = Attrs::default();
            return;
        }
        let mut index = 0;
        while index < params.len() {
            let group = &params[index];
            let code = group.first().copied().unwrap_or(0);
            match code {
                0 => self.pen = Attrs::default(),
                1 => self.pen.bold = true,
                2 => self.pen.faint = true,
                3 => self.pen.italic = true,
                4 => self.pen.underline = true,
                5 | 6 => self.pen.blink = true,
                7 => self.pen.reverse = true,
                8 => self.pen.hidden = true,
                9 => self.pen.struck = true,
                21 | 22 => {
                    self.pen.bold = false;
                    self.pen.faint = false;
                }
                23 => self.pen.italic = false,
                24 => self.pen.underline = false,
                25 => self.pen.blink = false,
                27 => self.pen.reverse = false,
                28 => self.pen.hidden = false,
                29 => self.pen.struck = false,
                30..=37 => self.pen.foreground = Color::Indexed((code - 30) as u8),
                39 => self.pen.foreground = Color::Default,
                40..=47 => self.pen.background = Color::Indexed((code - 40) as u8),
                49 => self.pen.background = Color::Default,
                90..=97 => self.pen.foreground = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.pen.background = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 => {
                    // **Two spellings of the same thing.** `38:2:r:g:b` keeps the
                    // parts in one parameter (the form the standard wants);
                    // `38;2;r;g;b` spreads them over several (the form everything
                    // actually sends). Both have to be read.
                    let (colour, used) = if group.len() > 1 {
                        (extended_colour(&group[1..], true), 1)
                    } else {
                        let rest: Vec<u16> = params[index + 1..]
                            .iter()
                            .map(|group| group.first().copied().unwrap_or(0))
                            .collect();
                        let taken = match rest.first() {
                            Some(5) => 2,
                            Some(2) => 4,
                            _ => 1,
                        };
                        (extended_colour(&rest, false), taken)
                    };
                    if let Some(colour) = colour {
                        if code == 38 {
                            self.pen.foreground = colour;
                        } else {
                            self.pen.background = colour;
                        }
                    }
                    index += used;
                }
                _ => {}
            }
            index += 1;
        }
    }

    fn set_mode(&mut self, private: bool, mode: u16, on: bool) {
        if !private {
            return;
        }
        match mode {
            1 => self.modes.application_cursor_keys = on,
            25 => self.modes.cursor_visible = on,
            1000 | 1002 | 1003 | 1006 => self.modes.mouse_tracking = on,
            1004 => self.modes.focus_events = on,
            1049 => self.use_alternate_screen(on),
            2004 => self.modes.bracketed_paste = on,
            9001 => self.modes.win32_input = on,
            _ => {}
        }
    }

    fn report(&mut self, kind: u16) {
        match kind {
            5 => self.replies.extend_from_slice(b"\x1b[0n"),
            6 => {
                let answer = format!("\x1b[{};{}R", self.cursor.row + 1, self.cursor.column + 1);
                self.replies.extend_from_slice(answer.as_bytes());
            }
            _ => {}
        }
    }

    fn identify(&mut self) {
        // A VT102 with no options. **Claiming less than we are is safe**;
        // claiming more invites sequences this half does not implement.
        self.replies.extend_from_slice(b"\x1b[?6c");
    }
}

/// The `5;n` and `2;r;g;b` tails of SGR 38 and 48.
///
/// **The two spellings are not the same shape.** Written with colons the parts
/// belong to one parameter and may carry a colour-space slot before the
/// components (`38:2::10:20:30`); written with semicolons they are parameters of
/// their own and whatever follows the blue one belongs to the next attribute.
/// So the count means something in the first spelling and nothing in the second,
/// and only the caller knows which it read.
fn extended_colour(parts: &[u16], colon: bool) -> Option<Color> {
    match *parts.first()? {
        5 => parts.get(1).map(|index| Color::Indexed(*index as u8)),
        2 => {
            let components = if colon && parts.len() >= 5 {
                &parts[2..]
            } else {
                &parts[1..]
            };
            match components {
                [red, green, blue, ..] => Some(Color::Rgb(*red as u8, *green as u8, *blue as u8)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// How many cells a character occupies.
///
/// **This is the East Asian Wide question, and it is not decoration.** A
/// terminal that thinks `あ` is one cell puts every following character in the
/// wrong column, and `ls` of a Japanese directory becomes unreadable.
///
/// The ranges below are the wide ones, not the whole of UAX #11: everything not
/// named is one cell, and the combining marks are zero. That is a deliberate
/// simplification of a table that changes with every Unicode version.
pub fn character_width(text: char) -> usize {
    let code = text as u32;
    if matches!(code,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF
        | 0x20D0..=0x20F0 | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F | 0x200B..=0x200F)
    {
        return 0;
    }
    if matches!(code,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xA960..=0xA97F | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF | 0xFE10..=0xFE19 | 0xFE30..=0xFE6F | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF
        | 0x20000..=0x2FFFD | 0x30000..=0x3FFFD)
    {
        return 2;
    }
    1
}

/// Where the byte stream is in the middle of a sequence.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    /// `ESC (` and friends: the next byte names a character set. **Read and
    /// dropped** — one byte that would otherwise be printed.
    Designator,
    Csi,
    Osc,
    /// Inside an OSC, having just seen `ESC`: the next byte should be `\`.
    OscEscape,
    /// A DCS/APC/PM string, kept only so its end can be found.
    Ignore,
    IgnoreEscape,
}

/// Bytes to sequences, sequences to the screen.
///
/// **The parser holds no screen of its own.** It is given one for the length of
/// a `feed`, which is what makes it safe to read a pipe into a buffer of any
/// size: a sequence split across two reads simply resumes.
#[derive(Debug, Default)]
pub struct Parser {
    state: State,
    groups: Vec<Vec<u16>>,
    group: Vec<u16>,
    number: Option<u16>,
    private: Option<u8>,
    intermediate: Option<u8>,
    osc: Vec<u8>,
    /// A UTF-8 character being assembled, and how many bytes are still due.
    pending: u32,
    due: u8,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8], screen: &mut Screen) {
        for &byte in bytes {
            self.step(byte, screen);
        }
    }

    fn step(&mut self, byte: u8, screen: &mut Screen) {
        match self.state {
            State::Ground => self.ground(byte, screen),
            State::Escape => self.escape(byte, screen),
            State::Designator => self.state = State::Ground,
            State::Csi => self.csi(byte, screen),
            State::Osc => match byte {
                0x07 => {
                    self.finish_osc(screen);
                    self.state = State::Ground;
                }
                0x1b => self.state = State::OscEscape,
                _ => self.osc.push(byte),
            },
            State::OscEscape => {
                self.finish_osc(screen);
                self.state = State::Ground;
                // `ESC \` ends the string; anything else was a sequence of its
                // own and still has to be read.
                if byte != b'\\' {
                    self.escape(byte, screen);
                }
            }
            State::Ignore => {
                if byte == 0x1b {
                    self.state = State::IgnoreEscape;
                }
            }
            State::IgnoreEscape => {
                self.state = if byte == b'\\' {
                    State::Ground
                } else {
                    State::Ignore
                };
            }
        }
    }

    fn ground(&mut self, byte: u8, screen: &mut Screen) {
        match byte {
            0x1b => {
                self.clear();
                self.state = State::Escape;
            }
            0x08 => screen.backspace(),
            0x09 => screen.tab(),
            0x0a | 0x0b | 0x0c => screen.line_feed(),
            0x0d => screen.carriage_return(),
            // BEL and the rest of C0 say nothing about the screen.
            0x00..=0x1f | 0x7f => {}
            0x20..=0x7e => {
                self.due = 0;
                screen.print(byte as char);
            }
            _ => self.utf8(byte, screen),
        }
    }

    /// **A character can be split across two reads.** The pipe hands over
    /// whatever has arrived, so the bytes of one `あ` may come 8KB apart.
    fn utf8(&mut self, byte: u8, screen: &mut Screen) {
        if self.due > 0 {
            if byte & 0xc0 == 0x80 {
                self.pending = (self.pending << 6) | (byte & 0x3f) as u32;
                self.due -= 1;
                if self.due == 0 {
                    if let Some(text) = char::from_u32(self.pending) {
                        screen.print(text);
                    }
                }
            } else {
                // Truncated. Drop it and read this byte as a fresh start.
                self.due = 0;
                self.utf8(byte, screen);
            }
            return;
        }
        let (bits, due) = match byte {
            0xc2..=0xdf => ((byte & 0x1f) as u32, 1),
            0xe0..=0xef => ((byte & 0x0f) as u32, 2),
            0xf0..=0xf4 => ((byte & 0x07) as u32, 3),
            // A stray continuation byte, or an encoding this is not.
            _ => return,
        };
        self.pending = bits;
        self.due = due;
    }

    fn escape(&mut self, byte: u8, screen: &mut Screen) {
        self.state = State::Ground;
        match byte {
            b'[' => {
                self.clear();
                self.state = State::Csi;
            }
            b']' => {
                self.osc.clear();
                self.state = State::Osc;
            }
            b'P' | b'X' | b'^' | b'_' => self.state = State::Ignore,
            b'(' | b')' | b'*' | b'+' => self.state = State::Designator,
            b'7' => screen.save_cursor(),
            b'8' => screen.restore_cursor(),
            b'D' => screen.line_feed(),
            b'E' => {
                screen.carriage_return();
                screen.line_feed();
            }
            b'M' => screen.reverse_index(),
            b'c' => screen.reset(),
            // `ESC =` / `ESC >` (keypad modes) and anything else: nothing here
            // has a screen to change.
            _ => {}
        }
    }

    fn csi(&mut self, byte: u8, screen: &mut Screen) {
        match byte {
            b'0'..=b'9' => {
                let digit = (byte - b'0') as u16;
                self.number = Some(self.number.unwrap_or(0).saturating_mul(10) + digit);
            }
            b';' => {
                let number = self.number.take().unwrap_or(0);
                self.group.push(number);
                let group = std::mem::take(&mut self.group);
                self.groups.push(group);
            }
            b':' => {
                let number = self.number.take().unwrap_or(0);
                self.group.push(number);
            }
            b'<'..=b'?' => self.private = Some(byte),
            0x20..=0x2f => self.intermediate = Some(byte),
            0x40..=0x7e => {
                if self.number.is_some() || !self.group.is_empty() {
                    let number = self.number.take().unwrap_or(0);
                    self.group.push(number);
                    let group = std::mem::take(&mut self.group);
                    self.groups.push(group);
                }
                self.dispatch(byte, screen);
                self.clear();
                self.state = State::Ground;
            }
            _ => {}
        }
    }

    /// The first number of parameter `index`, with 0 and "absent" meaning the
    /// same thing: **the default the sequence was defined with.**
    fn at(&self, index: usize, default: u16) -> u16 {
        match self.groups.get(index).and_then(|group| group.first()) {
            Some(0) | None => default,
            Some(value) => *value,
        }
    }

    fn dispatch(&mut self, final_byte: u8, screen: &mut Screen) {
        let private = self.private == Some(b'?');
        let count = |value: u16| value as usize;
        match final_byte {
            b'A' => screen.move_by(-(self.at(0, 1) as isize), 0),
            b'B' | b'e' => screen.move_by(self.at(0, 1) as isize, 0),
            b'C' | b'a' => screen.move_by(0, self.at(0, 1) as isize),
            b'D' => screen.move_by(0, -(self.at(0, 1) as isize)),
            b'E' => {
                let rows = self.at(0, 1) as isize;
                screen.move_by(rows, 0);
                screen.carriage_return();
            }
            b'F' => {
                let rows = self.at(0, 1) as isize;
                screen.move_by(-rows, 0);
                screen.carriage_return();
            }
            b'G' | b'`' => {
                let row = screen.cursor.row;
                screen.move_to(row, count(self.at(0, 1)) - 1);
            }
            b'd' => {
                let column = screen.cursor.column;
                screen.move_to(count(self.at(0, 1)) - 1, column);
            }
            b'H' | b'f' => {
                screen.move_to(count(self.at(0, 1)) - 1, count(self.at(1, 1)) - 1);
            }
            b'J' => screen.erase_in_display(self.at(0, 0).min(3)),
            b'K' => screen.erase_in_line(self.at(0, 0).min(2)),
            b'L' => screen.insert_lines(count(self.at(0, 1))),
            b'M' => screen.delete_lines(count(self.at(0, 1))),
            b'P' => screen.delete_cells(count(self.at(0, 1))),
            b'X' => screen.erase_cells(count(self.at(0, 1))),
            b'@' => screen.insert_cells(count(self.at(0, 1))),
            b'S' => screen.scroll_up(count(self.at(0, 1))),
            b'T' => screen.scroll_down(count(self.at(0, 1))),
            b'r' => {
                let bottom = self.at(1, screen.rows as u16);
                screen.set_region(count(self.at(0, 1)) - 1, count(bottom) - 1);
            }
            b'h' | b'l' => {
                let on = final_byte == b'h';
                let modes: Vec<u16> = self
                    .groups
                    .iter()
                    .filter_map(|group| group.first().copied())
                    .collect();
                for mode in modes {
                    screen.set_mode(private, mode, on);
                }
            }
            b'm' => {
                let groups = std::mem::take(&mut self.groups);
                screen.select_graphic_rendition(&groups);
                self.groups = groups;
            }
            b'n' => screen.report(self.at(0, 0)),
            b'c' => {
                if self.private.is_none() {
                    screen.identify();
                }
            }
            b's' => {
                if self.private.is_none() {
                    screen.save_cursor();
                }
            }
            b'u' => {
                if self.private.is_none() {
                    screen.restore_cursor();
                }
            }
            b'g' => {
                let column = screen.cursor.column;
                match self.at(0, 0) {
                    3 => screen.tab_stops.iter_mut().for_each(|stop| *stop = false),
                    _ => screen.tab_stops[column] = false,
                }
            }
            _ => {}
        }
    }

    fn finish_osc(&mut self, screen: &mut Screen) {
        let string = std::mem::take(&mut self.osc);
        let mut parts = string.splitn(2, |byte| *byte == b';');
        let kind = parts.next().unwrap_or(b"");
        let rest = parts.next().unwrap_or(b"");
        // 0 sets both the icon name and the title; 2 sets the title. Nothing
        // here has an icon name, so they are the same thing.
        if kind == b"0" || kind == b"2" {
            screen.title = String::from_utf8_lossy(rest).into_owned();
            screen.touch();
        }
    }

    fn clear(&mut self) {
        self.groups.clear();
        self.group.clear();
        self.number = None;
        self.private = None;
        self.intermediate = None;
    }
}

/// A screen and the parser that feeds it.
#[derive(Debug)]
pub struct Terminal {
    pub screen: Screen,
    parser: Parser,
}

impl Terminal {
    pub fn new(columns: usize, rows: usize) -> Self {
        Self {
            screen: Screen::new(columns, rows),
            parser: Parser::new(),
        }
    }

    /// Read what the shell wrote. **Any split is allowed**: the parser resumes.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.feed(bytes, &mut self.screen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prompt `wsl.exe` sent through `pty.rs`, byte for byte (技術検証9.4).
    const WSL_PROMPT: &[u8] = b"\x1b[?9001h\x1b[?1004h\x1b[?25l\x1b[2J\x1b[m\x1b[32m\x1b[1m\
\x1b[Hken@KN01\x1b[m:\x1b[34m\x1b[1m/mnt/d/Projects/10_Creation/50_Dev/10_Editor\x1b[m$\x1b[1C\
\x1b]0;ken@KN01: /mnt/d/Projects/10_Creation/50_Dev/10_Editor\x07\x1b[?25h\x1b[?2004h";

    fn terminal(columns: usize, rows: usize) -> Terminal {
        Terminal::new(columns, rows)
    }

    #[test]
    fn the_prompt_a_real_shell_sent_lands_in_the_grid() {
        let mut it = terminal(80, 25);
        it.feed(WSL_PROMPT);
        assert_eq!(
            it.screen.row_text(0),
            "ken@KN01:/mnt/d/Projects/10_Creation/50_Dev/10_Editor$"
        );
        assert_eq!(
            it.screen.title(),
            "ken@KN01: /mnt/d/Projects/10_Creation/50_Dev/10_Editor"
        );
        // The prompt is 54 columns, and `ESC[1C` left the cursor one past it.
        assert_eq!(it.screen.cursor().row, 0);
        assert_eq!(it.screen.cursor().column, 55);
    }

    #[test]
    fn the_modes_the_shell_asked_for_are_remembered() {
        let mut it = terminal(80, 25);
        it.feed(WSL_PROMPT);
        let modes = it.screen.modes();
        assert!(modes.win32_input, "conhost asked for win32 input");
        assert!(modes.focus_events);
        assert!(modes.bracketed_paste, "bash turns this on for its prompt");
        assert!(modes.cursor_visible, "hidden for the repaint, shown after");
    }

    #[test]
    fn the_colours_the_prompt_was_written_in_are_kept() {
        let mut it = terminal(80, 25);
        it.feed(WSL_PROMPT);
        let user = it.screen.line(0).unwrap().cells[0];
        assert_eq!(user.text, 'k');
        assert_eq!(user.attrs.foreground, Color::Indexed(2));
        assert!(user.attrs.bold);
        let colon = it.screen.line(0).unwrap().cells[8];
        assert_eq!(colon.text, ':');
        assert_eq!(
            colon.attrs.foreground,
            Color::Default,
            "`ESC[m` between the two colours puts the pen back"
        );
        let path = it.screen.line(0).unwrap().cells[9];
        assert_eq!(path.attrs.foreground, Color::Indexed(4));
    }

    #[test]
    fn the_last_column_is_written_before_the_line_wraps() {
        let mut it = terminal(4, 3);
        it.feed(b"abcd");
        assert_eq!(it.screen.row_text(0), "abcd");
        assert_eq!(
            (it.screen.cursor().row, it.screen.cursor().column),
            (0, 3),
            "still on the last column: a box drawn to the corner must not scroll"
        );
        it.feed(b"e");
        assert_eq!(it.screen.row_text(1), "e");
        assert_eq!((it.screen.cursor().row, it.screen.cursor().column), (1, 1));
        assert!(it.screen.line(0).unwrap().wrapped, "copying joins the two");
    }

    #[test]
    fn a_full_width_character_takes_two_cells() {
        let mut it = terminal(10, 3);
        it.feed("あい".as_bytes());
        assert_eq!(it.screen.row_text(0), "あい");
        assert_eq!(it.screen.cursor().column, 4);
        assert!(it.screen.line(0).unwrap().cells[1].trailing);
        assert!(it.screen.line(0).unwrap().cells[3].trailing);
    }

    #[test]
    fn writing_over_half_of_a_wide_character_takes_the_other_half_with_it() {
        let mut it = terminal(10, 3);
        it.feed("あ\r".as_bytes());
        it.feed(b"x");
        assert_eq!(
            it.screen.row_text(0),
            "x",
            "the orphaned half would shift the whole row"
        );
    }

    #[test]
    fn a_character_split_across_two_reads_is_still_one_character() {
        let mut it = terminal(10, 3);
        let bytes = "あ".as_bytes();
        it.feed(&bytes[..1]);
        it.feed(&bytes[1..]);
        assert_eq!(it.screen.row_text(0), "あ");
    }

    #[test]
    fn a_sequence_split_across_two_reads_is_still_one_sequence() {
        let mut it = terminal(10, 3);
        it.feed(b"\x1b[3");
        it.feed(b";2Hx");
        assert_eq!(it.screen.row_text(2), " x");
    }

    #[test]
    fn lines_that_leave_the_top_become_history() {
        let mut it = terminal(10, 2);
        it.feed(b"one\r\ntwo\r\nthree");
        assert_eq!(it.screen.scrollback().len(), 1);
        assert_eq!(it.screen.scrollback()[0].text(), "one");
        assert_eq!(it.screen.row_text(0), "two");
        assert_eq!(it.screen.row_text(1), "three");
    }

    #[test]
    fn the_alternate_screen_gives_the_first_one_back_untouched() {
        let mut it = terminal(10, 2);
        it.feed(b"shell\r\n");
        it.feed(b"\x1b[?1049h");
        it.feed(b"editing");
        assert_eq!(it.screen.row_text(0), "editing");
        assert!(it.screen.modes().alternate_screen);
        let history = it.screen.scrollback().len();
        it.feed(b"\r\nmore\r\nand more");
        assert_eq!(
            it.screen.scrollback().len(),
            history,
            "a full-screen program's scrolling is not the session's history"
        );
        it.feed(b"\x1b[?1049l");
        assert_eq!(it.screen.row_text(0), "shell");
        assert!(!it.screen.modes().alternate_screen);
    }

    #[test]
    fn asking_where_the_cursor_is_gets_an_answer_to_send_back() {
        let mut it = terminal(80, 25);
        it.feed(b"\x1b[3;5H\x1b[6n");
        assert_eq!(it.screen.take_replies(), b"\x1b[3;5R".to_vec());
        assert!(
            it.screen.take_replies().is_empty(),
            "an answer is sent once"
        );
    }

    #[test]
    fn a_title_can_end_with_bel_or_with_st() {
        let mut it = terminal(80, 25);
        it.feed(b"\x1b]0;by bell\x07");
        assert_eq!(it.screen.title(), "by bell");
        it.feed(b"\x1b]2;by string terminator\x1b\\");
        assert_eq!(it.screen.title(), "by string terminator");
    }

    #[test]
    fn erasing_the_display_leaves_the_cursor_where_it_was() {
        let mut it = terminal(10, 3);
        it.feed(b"\x1b[2;3Hxyz\x1b[2J");
        assert_eq!(it.screen.row_text(1), "");
        assert_eq!((it.screen.cursor().row, it.screen.cursor().column), (1, 5));
    }

    #[test]
    fn erasing_to_the_end_of_a_line_keeps_what_is_before_the_cursor() {
        let mut it = terminal(10, 2);
        it.feed(b"abcdef\x1b[1;4H\x1b[K");
        assert_eq!(it.screen.row_text(0), "abc");
    }

    #[test]
    fn a_scroll_region_leaves_the_rows_outside_it_still() {
        let mut it = terminal(10, 4);
        it.feed(b"\x1b[1;1Htop\x1b[4;1Hfoot");
        it.feed(b"\x1b[2;3r"); // scroll between rows 2 and 3
        it.feed(b"\x1b[2;1Hone\r\ntwo\r\nthree");
        assert_eq!(it.screen.row_text(0), "top");
        assert_eq!(it.screen.row_text(3), "foot");
        assert_eq!(it.screen.row_text(1), "two");
        assert_eq!(it.screen.row_text(2), "three");
        assert!(
            it.screen.scrollback().is_empty(),
            "what falls out of a program's own window is not session history"
        );
    }

    #[test]
    fn an_extended_colour_can_be_written_either_way() {
        let mut it = terminal(10, 2);
        it.feed(b"\x1b[38;2;10;20;30ma");
        assert_eq!(
            it.screen.line(0).unwrap().cells[0].attrs.foreground,
            Color::Rgb(10, 20, 30)
        );
        it.feed(b"\x1b[38:5:200mb");
        assert_eq!(
            it.screen.line(0).unwrap().cells[1].attrs.foreground,
            Color::Indexed(200)
        );
        // The colon spelling with the colour-space slot left empty, and the
        // semicolon spelling with another attribute behind the colour.
        it.feed(b"\x1b[38:2::10:20:30mc");
        assert_eq!(
            it.screen.line(0).unwrap().cells[2].attrs.foreground,
            Color::Rgb(10, 20, 30)
        );
        it.feed(b"\x1b[38;2;1;2;3;1md");
        let cell = it.screen.line(0).unwrap().cells[3];
        assert_eq!(cell.attrs.foreground, Color::Rgb(1, 2, 3));
        assert!(cell.attrs.bold, "the `1` after the colour is still bold");
    }

    #[test]
    fn a_tab_goes_to_the_next_stop() {
        let mut it = terminal(20, 2);
        it.feed(b"ab\tc");
        assert_eq!(it.screen.row_text(0), "ab      c");
        assert_eq!(it.screen.cursor().column, 9);
    }

    #[test]
    fn inserting_and_deleting_cells_moves_the_rest_of_the_row() {
        let mut it = terminal(10, 2);
        it.feed(b"abcdef\x1b[1;3H\x1b[2@");
        assert_eq!(it.screen.row_text(0), "ab  cdef");
        it.feed(b"\x1b[2P");
        assert_eq!(it.screen.row_text(0), "abcdef");
    }

    #[test]
    fn a_narrower_pane_keeps_the_rows_the_reader_can_see() {
        let mut it = terminal(20, 3);
        it.feed(b"one\r\ntwo\r\nthree");
        it.screen.resize(20, 2);
        assert_eq!(it.screen.row_text(0), "two");
        assert_eq!(it.screen.row_text(1), "three");
        assert_eq!(it.screen.scrollback().len(), 1);
        assert_eq!(it.screen.cursor().row, 1);
    }

    #[test]
    fn nothing_visible_changes_without_the_revision_moving() {
        let mut it = terminal(10, 2);
        let before = it.screen.revision();
        it.feed(b"\x1b[?25l");
        it.feed(b"x");
        assert!(it.screen.revision() > before);
        let settled = it.screen.revision();
        it.feed(b"\x00\x07");
        assert_eq!(
            it.screen.revision(),
            settled,
            "a bell does not repaint anything"
        );
    }
}
