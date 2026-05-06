use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Print, ResetColor, SetAttribute},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};

/// One row in the picker. `key` is the stable identifier returned to the caller
/// (e.g. a session name). `display` is the rendered line shown to the user;
/// it must be plain text (no ANSI escapes) so the cursor highlight renders
/// cleanly. `search_text` is matched against the user's filter query.
pub struct Item {
    pub key: String,
    pub display: String,
    pub search_text: String,
}

/// Run a multi-select picker over `items`. Returns:
/// - `Ok(Some(keys))` with the chosen keys in original-input order
/// - `Ok(None)` if the user cancelled (Ctrl-C)
///
/// Errors if stdin/stdout is not a terminal, or if `items` is empty.
pub fn pick(items: Vec<Item>, title: &str) -> Result<Option<Vec<String>>> {
    if items.is_empty() {
        bail!("Nothing to pick");
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("Interactive mode requires an interactive terminal");
    }

    let mut stdout = io::stdout();
    let _guard = TerminalGuard::enter(&mut stdout)?;

    let mut state = PickerState::new(items, title.to_string());
    let outcome = event_loop(&mut stdout, &mut state)?;

    Ok(match outcome {
        Outcome::Cancel => None,
        Outcome::Confirm => Some(state.confirmed_keys()),
    })
}

// ── Terminal RAII guard ─────────────────────────────────────────────────────

struct TerminalGuard;

impl TerminalGuard {
    fn enter(stdout: &mut io::Stdout) -> Result<Self> {
        terminal::enable_raw_mode().context("Failed to enable raw mode")?;
        execute!(stdout, EnterAlternateScreen, cursor::Hide)
            .context("Failed to enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        // Best-effort teardown: never panic in Drop.
        let _ = execute!(stdout, ResetColor, cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

// ── State machine ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Select,
    Search,
    Confirm,
}

enum Outcome {
    Confirm,
    Cancel,
}

struct PickerState {
    items: Vec<Item>,
    selected: Vec<bool>,
    /// Cursor position as an index into `filtered`.
    cursor: usize,
    /// Top of the visible viewport, also into `filtered`.
    offset: usize,
    mode: Mode,
    query: String,
    /// Indices into `items`, after applying `query`.
    filtered: Vec<usize>,
    title: String,
}

impl PickerState {
    fn new(items: Vec<Item>, title: String) -> Self {
        let n = items.len();
        let filtered = (0..n).collect();
        Self {
            items,
            selected: vec![false; n],
            cursor: 0,
            offset: 0,
            mode: Mode::Select,
            query: String::new(),
            filtered,
            title,
        }
    }

    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        let prev_key = self
            .filtered
            .get(self.cursor)
            .map(|&i| self.items[i].key.clone());
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| q.is_empty() || it.search_text.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();

        // Try to keep the cursor on the same item; otherwise clamp.
        self.cursor = match prev_key {
            Some(k) => self
                .filtered
                .iter()
                .position(|&i| self.items[i].key == k)
                .unwrap_or(0),
            None => 0,
        };
        self.cursor = self
            .cursor
            .min(self.filtered.len().saturating_sub(1));
        self.offset = 0;
    }

    fn ensure_cursor_visible(&mut self, viewport: usize) {
        if viewport == 0 || self.filtered.is_empty() {
            return;
        }
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + viewport {
            self.offset = self.cursor + 1 - viewport;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let n = self.filtered.len();
        if n == 0 {
            self.cursor = 0;
            return;
        }
        let max = n as isize - 1;
        let mut new = self.cursor as isize + delta;
        if new < 0 {
            new = 0;
        }
        if new > max {
            new = max;
        }
        self.cursor = new as usize;
    }

    fn jump_top(&mut self) {
        self.cursor = 0;
    }

    fn jump_bottom(&mut self) {
        self.cursor = self.filtered.len().saturating_sub(1);
    }

    fn toggle_selection_at_cursor(&mut self) {
        if let Some(&idx) = self.filtered.get(self.cursor) {
            self.selected[idx] = !self.selected[idx];
        }
    }

    fn selected_count(&self) -> usize {
        self.selected.iter().filter(|s| **s).count()
    }

    /// Keys to return when the user confirms. If the user has explicitly
    /// selected one or more items (possibly hidden by the current filter),
    /// return those in original input order. Otherwise fall back to the
    /// item under the cursor (if any).
    fn confirmed_keys(&self) -> Vec<String> {
        if self.selected_count() > 0 {
            self.items
                .iter()
                .enumerate()
                .filter(|(i, _)| self.selected[*i])
                .map(|(_, it)| it.key.clone())
                .collect()
        } else if let Some(&idx) = self.filtered.get(self.cursor) {
            vec![self.items[idx].key.clone()]
        } else {
            Vec::new()
        }
    }

    /// Whether pressing Enter in select mode should produce a removal list.
    fn can_confirm(&self) -> bool {
        self.selected_count() > 0 || !self.filtered.is_empty()
    }
}

// ── Event loop ──────────────────────────────────────────────────────────────

fn event_loop(stdout: &mut io::Stdout, state: &mut PickerState) -> Result<Outcome> {
    loop {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let viewport = viewport_height(rows as usize);
        state.ensure_cursor_visible(viewport);
        render(stdout, state, cols as usize, rows as usize, viewport)?;

        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if is_ctrl_c(&key) {
                    return Ok(Outcome::Cancel);
                }
                match state.mode {
                    Mode::Select => match handle_select_key(state, key) {
                        SelectAction::Continue => {}
                        SelectAction::EnterConfirm => state.mode = Mode::Confirm,
                    },
                    Mode::Search => handle_search_key(state, key),
                    Mode::Confirm => match handle_confirm_key(key) {
                        ConfirmAction::Confirmed => return Ok(Outcome::Confirm),
                        ConfirmAction::BackToSelect => state.mode = Mode::Select,
                    },
                }
            }
            Event::Resize(_, _) => { /* re-render on next iteration */ }
            _ => {}
        }
    }
}

fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C'))
}

enum SelectAction {
    Continue,
    EnterConfirm,
}

enum ConfirmAction {
    Confirmed,
    BackToSelect,
}

fn handle_select_key(state: &mut PickerState, key: KeyEvent) -> SelectAction {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => state.move_cursor(1),
        KeyCode::Char('k') | KeyCode::Up => state.move_cursor(-1),
        KeyCode::Char('g') | KeyCode::Home => state.jump_top(),
        KeyCode::Char('G') | KeyCode::End => state.jump_bottom(),
        KeyCode::PageDown => state.move_cursor(10),
        KeyCode::PageUp => state.move_cursor(-10),
        KeyCode::Char(' ') => state.toggle_selection_at_cursor(),
        KeyCode::Char('/') => {
            state.mode = Mode::Search;
        }
        KeyCode::Esc if !state.query.is_empty() => {
            // Esc clears the active search filter from anywhere — the search
            // input itself ALSO maps Esc to clear-and-exit, so this gives the
            // user a single muscle memory for "drop the filter".
            state.query.clear();
            state.refilter();
        }
        KeyCode::Enter if state.can_confirm() => {
            return SelectAction::EnterConfirm;
        }
        _ => {}
    }
    SelectAction::Continue
}

fn handle_confirm_key(key: KeyEvent) -> ConfirmAction {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => ConfirmAction::Confirmed,
        // Anything else (Enter, n, N, Esc, etc.) bounces back to Select mode.
        // This is intentional: the most common accidental press is a stray
        // Enter, and bouncing it back to Select rather than confirming
        // protects against the "double-Enter while finishing a search"
        // pitfall this dialog exists to prevent.
        _ => ConfirmAction::BackToSelect,
    }
}

fn handle_search_key(state: &mut PickerState, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            state.query.clear();
            state.refilter();
            state.mode = Mode::Select;
        }
        KeyCode::Enter => {
            state.mode = Mode::Select;
        }
        KeyCode::Backspace => {
            state.query.pop();
            state.refilter();
        }
        KeyCode::Up => state.move_cursor(-1),
        KeyCode::Down => state.move_cursor(1),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ignore control combos other than Ctrl-C (already handled).
            state.query.push(c);
            state.refilter();
        }
        _ => {}
    }
}

// ── Rendering ───────────────────────────────────────────────────────────────

/// Background SGR sequence used to highlight the cursor row. `\x1b[100m` is
/// the "bright black" background — a subtle dark grey that's distinguishable
/// against both light and dark terminal themes while still letting the
/// per-field foreground colors (magenta, cyan, etc.) read through.
const HIGHLIGHT_BG: &str = "\x1b[100m";

/// Reserve 1 row for the title and 1 row for the footer.
fn viewport_height(rows: usize) -> usize {
    rows.saturating_sub(2).max(1)
}

fn render(
    stdout: &mut io::Stdout,
    state: &PickerState,
    cols: usize,
    rows: usize,
    viewport: usize,
) -> Result<()> {
    queue!(stdout, ResetColor, cursor::MoveTo(0, 0), Clear(ClearType::All))?;

    // Title row.
    queue!(
        stdout,
        cursor::MoveTo(0, 0),
        SetAttribute(Attribute::Bold),
        Print(visible_truncate(&state.title, cols)),
        SetAttribute(Attribute::Reset),
        ResetColor,
    )?;

    // List rows.
    if state.filtered.is_empty() {
        let msg = if state.query.is_empty() {
            "(no items)".to_string()
        } else {
            format!("No matches for '{}'", state.query)
        };
        queue!(
            stdout,
            cursor::MoveTo(0, 1),
            SetAttribute(Attribute::Dim),
            Print(visible_truncate(&msg, cols)),
            SetAttribute(Attribute::Reset),
            ResetColor,
        )?;
    } else {
        let visible = state
            .filtered
            .iter()
            .enumerate()
            .skip(state.offset)
            .take(viewport);
        for (vis_row, (filt_idx, &item_idx)) in visible.enumerate() {
            let y = 1u16 + vis_row as u16;
            let is_cursor = filt_idx == state.cursor;
            let mark = if state.selected[item_idx] { "[x]" } else { "[ ]" };
            let arrow = if is_cursor { ">" } else { " " };
            let display = &state.items[item_idx].display;
            let prefix = format!("{arrow} {mark} ");
            let prefix_visible = prefix.chars().count();
            let body_budget = cols.saturating_sub(prefix_visible);
            let body = visible_truncate(display, body_budget);
            let body_visible = visible_width(&body);
            let pad = cols.saturating_sub(prefix_visible + body_visible);
            queue!(stdout, cursor::MoveTo(0, y))?;
            if is_cursor {
                // Subtle highlight: bright-black background that survives the
                // inline RESETs `format_session_line` sprinkles between
                // colored fields. Foreground colors are preserved.
                let raw = format!("{prefix}{body}{}", " ".repeat(pad));
                let highlighted = apply_background(&raw, HIGHLIGHT_BG);
                queue!(
                    stdout,
                    Print(HIGHLIGHT_BG),
                    Print(highlighted),
                    ResetColor,
                )?;
            } else {
                queue!(stdout, Print(prefix), Print(body), ResetColor)?;
                if pad > 0 {
                    queue!(stdout, Print(" ".repeat(pad)))?;
                }
            }
        }
    }

    // Footer row.
    let footer_y = rows.saturating_sub(1) as u16;
    render_footer(stdout, state, footer_y, cols)?;

    // Caret visibility.
    match state.mode {
        Mode::Search => {
            // "/" + query, then cursor sits one to the right of the slash + query.
            let caret_col = 1 + state.query.chars().count();
            let caret_col = caret_col.min(cols.saturating_sub(1)) as u16;
            queue!(stdout, cursor::MoveTo(caret_col, footer_y), cursor::Show)?;
        }
        _ => {
            queue!(stdout, cursor::Hide)?;
        }
    }

    stdout.flush()?;
    Ok(())
}

fn render_footer(
    stdout: &mut io::Stdout,
    state: &PickerState,
    footer_y: u16,
    cols: usize,
) -> Result<()> {
    queue!(stdout, cursor::MoveTo(0, footer_y))?;
    match state.mode {
        Mode::Search => {
            // "/<query>" — keep it simple so the caret math above is exact.
            queue!(
                stdout,
                Print(visible_truncate(&format!("/{}", state.query), cols)),
                ResetColor,
            )?;
        }
        Mode::Select => {
            let mut parts: Vec<String> = Vec::new();
            parts.push(format!("{} selected", state.selected_count()));
            if !state.query.is_empty() {
                parts.push(format!("filter: {}", state.query));
            }
            parts.push(
                "j/k:nav  space:select  enter:confirm  /:search  esc:clear  ctrl-c:cancel"
                    .to_string(),
            );
            let text = visible_truncate(&parts.join("  |  "), cols);
            queue!(
                stdout,
                SetAttribute(Attribute::Dim),
                Print(text),
                SetAttribute(Attribute::Reset),
                ResetColor,
            )?;
        }
        Mode::Confirm => {
            let count = pending_remove_count(state);
            let noun = if count == 1 { "session" } else { "sessions" };
            let prompt = format!("Remove {count} {noun}? Press [y] to confirm, any other key to go back, ctrl-c to cancel");
            queue!(
                stdout,
                SetAttribute(Attribute::Bold),
                SetAttribute(Attribute::Reverse),
                Print(visible_truncate(&prompt, cols)),
                SetAttribute(Attribute::Reset),
                ResetColor,
            )?;
        }
    }
    Ok(())
}

fn pending_remove_count(state: &PickerState) -> usize {
    let explicit = state.selected_count();
    if explicit > 0 {
        explicit
    } else if state.filtered.is_empty() {
        0
    } else {
        1
    }
}

// ── String helpers ──────────────────────────────────────────────────────────

/// Inject `bg_seq` after every `\x1b[0m` reset in `s`. Used by the cursor
/// row to ensure the highlight background survives the per-field RESETs that
/// `format_session_line` emits between colored chunks. The caller is
/// responsible for emitting `bg_seq` before printing the result and
/// `ResetColor` after.
fn apply_background(s: &str, bg_seq: &str) -> String {
    let needle = "\x1b[0m";
    if !s.contains(needle) {
        return s.to_string();
    }
    let replacement = format!("{needle}{bg_seq}");
    s.replace(needle, &replacement)
}

/// Count the visible (printable) width of `s` in characters, ignoring CSI
/// escape sequences. Assumes characters in the visible part are width-1
/// (true for ASCII, which is what `format_session_line` produces).
fn visible_width(s: &str) -> usize {
    let mut count = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            while let Some(&c2) = chars.peek() {
                chars.next();
                if ('@'..='~').contains(&c2) {
                    break;
                }
            }
        } else {
            count += 1;
        }
    }
    count
}

/// Truncate `s` to at most `max_visible` printable characters while
/// preserving any embedded CSI escape sequences in full. This keeps colors
/// intact when a long session line has to be cropped to terminal width.
fn visible_truncate(s: &str, max_visible: usize) -> String {
    if max_visible == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(s.len());
    let mut count = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            // Always include the full escape sequence (zero visible width).
            out.push(c);
            out.push(chars.next().unwrap()); // '['
            while let Some(&c2) = chars.peek() {
                let ch = chars.next().unwrap();
                out.push(ch);
                if ('@'..='~').contains(&c2) {
                    break;
                }
            }
        } else {
            if count >= max_visible {
                break;
            }
            out.push(c);
            count += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, search: &str) -> Item {
        Item {
            key: key.to_string(),
            display: key.to_string(),
            search_text: search.to_string(),
        }
    }

    fn build(keys: &[(&str, &str)]) -> PickerState {
        let items = keys.iter().map(|(k, s)| item(k, s)).collect();
        PickerState::new(items, "test".to_string())
    }

    #[test]
    fn fresh_state_starts_at_top_with_no_selection() {
        let s = build(&[("a", "aa"), ("b", "bb"), ("c", "cc")]);
        assert_eq!(s.cursor, 0);
        assert_eq!(s.offset, 0);
        assert_eq!(s.selected_count(), 0);
        assert_eq!(s.filtered, vec![0, 1, 2]);
    }

    #[test]
    fn cursor_clamps_at_bounds() {
        let mut s = build(&[("a", ""), ("b", ""), ("c", "")]);
        s.move_cursor(-5);
        assert_eq!(s.cursor, 0);
        s.move_cursor(99);
        assert_eq!(s.cursor, 2);
        s.move_cursor(99);
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn jump_top_and_bottom() {
        let mut s = build(&[("a", ""), ("b", ""), ("c", "")]);
        s.jump_bottom();
        assert_eq!(s.cursor, 2);
        s.jump_top();
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn jump_bottom_on_empty_filter_does_not_panic() {
        let mut s = build(&[("a", "alpha")]);
        s.query = "zzz".into();
        s.refilter();
        assert!(s.filtered.is_empty());
        s.jump_bottom();
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        let mut s = build(&[("a", "Alpha"), ("b", "Bravo"), ("c", "alphabet")]);
        s.query = "ALPHA".into();
        s.refilter();
        assert_eq!(s.filtered, vec![0, 2]);
    }

    #[test]
    fn refilter_keeps_cursor_on_same_item_when_visible() {
        let mut s = build(&[("a", "alpha"), ("b", "bravo"), ("c", "alphabet")]);
        s.cursor = 2; // "alphabet"
        s.query = "alpha".into();
        s.refilter();
        // filtered now [0, 2], cursor should land on filtered index of item 2 (=> 1).
        assert_eq!(s.filtered, vec![0, 2]);
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn refilter_clamps_cursor_when_item_disappears() {
        let mut s = build(&[("a", "alpha"), ("b", "bravo"), ("c", "alphabet")]);
        s.cursor = 1; // "bravo"
        s.query = "alpha".into();
        s.refilter();
        assert_eq!(s.filtered, vec![0, 2]);
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn toggle_selection_marks_underlying_item() {
        let mut s = build(&[("a", ""), ("b", ""), ("c", "")]);
        s.cursor = 1;
        s.toggle_selection_at_cursor();
        assert!(s.selected[1]);
        assert_eq!(s.selected_count(), 1);
        s.toggle_selection_at_cursor();
        assert!(!s.selected[1]);
    }

    #[test]
    fn toggle_with_empty_filter_is_noop() {
        let mut s = build(&[("a", "alpha")]);
        s.query = "zzz".into();
        s.refilter();
        s.toggle_selection_at_cursor();
        assert_eq!(s.selected_count(), 0);
    }

    #[test]
    fn confirmed_keys_returns_selected_in_input_order() {
        let mut s = build(&[("a", ""), ("b", ""), ("c", ""), ("d", "")]);
        s.selected[2] = true;
        s.selected[0] = true;
        assert_eq!(s.confirmed_keys(), vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn confirmed_keys_falls_back_to_cursor_when_nothing_selected() {
        let mut s = build(&[("a", ""), ("b", ""), ("c", "")]);
        s.cursor = 1;
        assert_eq!(s.confirmed_keys(), vec!["b".to_string()]);
    }

    #[test]
    fn confirmed_keys_returns_hidden_selections_too() {
        // Select an item, then filter so it's not visible. Selection should
        // still be honored on confirm.
        let mut s = build(&[("a", "alpha"), ("b", "bravo")]);
        s.cursor = 1;
        s.toggle_selection_at_cursor(); // select "b"
        assert!(s.selected[1]);
        s.query = "alpha".into();
        s.refilter();
        assert_eq!(s.filtered, vec![0]);
        // confirmed_keys returns the hidden "b" — not the cursor item.
        assert_eq!(s.confirmed_keys(), vec!["b".to_string()]);
    }

    #[test]
    fn confirmed_keys_empty_when_no_selection_and_no_visible_item() {
        let mut s = build(&[("a", "alpha")]);
        s.query = "zzz".into();
        s.refilter();
        assert!(s.confirmed_keys().is_empty());
        assert!(!s.can_confirm());
    }

    #[test]
    fn ensure_cursor_visible_scrolls_offset() {
        let mut s = build(&[
            ("a", ""), ("b", ""), ("c", ""), ("d", ""), ("e", ""),
        ]);
        // Viewport of 2: cursor at 4 should set offset to 3.
        s.cursor = 4;
        s.ensure_cursor_visible(2);
        assert_eq!(s.offset, 3);
        // Scrolling up to cursor 0 should set offset to 0.
        s.cursor = 0;
        s.ensure_cursor_visible(2);
        assert_eq!(s.offset, 0);
    }

    #[test]
    fn truncate_handles_multibyte_chars() {
        // Each emoji is multi-byte; truncate by char count, not byte count.
        let s = "ᎯᎰᏂᏃᏄ";
        assert_eq!(visible_truncate(s, 3).chars().count(), 3);
    }

    // ── ANSI helpers ────────────────────────────────────────────────────────

    #[test]
    fn visible_width_ignores_csi() {
        assert_eq!(visible_width("\x1b[31mhello\x1b[0m"), 5);
        assert_eq!(visible_width("hi"), 2);
        assert_eq!(visible_width(""), 0);
    }

    #[test]
    fn visible_truncate_preserves_escape_sequences() {
        let s = "\x1b[31mhello\x1b[0m world";
        let out = visible_truncate(s, 7);
        assert_eq!(visible_width(&out), 7);
        // Color codes survived.
        assert!(out.contains("\x1b[31m"));
        assert!(out.contains("\x1b[0m"));
    }

    #[test]
    fn visible_truncate_zero_budget() {
        assert_eq!(visible_truncate("\x1b[31mhi", 0), "");
    }

    #[test]
    fn apply_background_inserts_after_each_reset() {
        // After every \x1b[0m the bg sequence is re-emitted so the highlight
        // bg sticks across inline resets in the colored row.
        let bg = "\x1b[100m";
        let s = "\x1b[31mhi\x1b[0m there \x1b[32my\x1b[0m";
        let out = apply_background(s, bg);
        assert_eq!(out, "\x1b[31mhi\x1b[0m\x1b[100m there \x1b[32my\x1b[0m\x1b[100m");
    }

    #[test]
    fn apply_background_passthrough_when_no_resets() {
        assert_eq!(apply_background("plain", "\x1b[100m"), "plain");
        assert_eq!(apply_background("", "\x1b[100m"), "");
    }

    #[test]
    fn apply_background_does_not_affect_visible_width() {
        let bg = "\x1b[100m";
        let s = "\x1b[31mhello\x1b[0m world";
        let with_bg = apply_background(s, bg);
        assert_eq!(visible_width(&with_bg), visible_width(s));
    }

    // ── State transitions for new behaviour ──────────────────────────────────

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn esc_in_select_mode_clears_query() {
        let mut s = build(&[("a", "alpha"), ("b", "bravo")]);
        s.query = "alp".into();
        s.refilter();
        assert_eq!(s.filtered.len(), 1);
        let action = handle_select_key(&mut s, key(KeyCode::Esc));
        assert!(matches!(action, SelectAction::Continue));
        assert_eq!(s.mode, Mode::Select);
        assert!(s.query.is_empty());
        assert_eq!(s.filtered.len(), 2);
    }

    #[test]
    fn esc_in_select_mode_with_empty_query_is_noop() {
        let mut s = build(&[("a", "")]);
        let action = handle_select_key(&mut s, key(KeyCode::Esc));
        assert!(matches!(action, SelectAction::Continue));
        assert_eq!(s.mode, Mode::Select);
    }

    #[test]
    fn esc_in_search_mode_still_clears_and_returns_to_select() {
        let mut s = build(&[("a", "alpha"), ("b", "bravo")]);
        s.mode = Mode::Search;
        s.query = "alp".into();
        s.refilter();
        handle_search_key(&mut s, key(KeyCode::Esc));
        assert_eq!(s.mode, Mode::Select);
        assert!(s.query.is_empty());
        assert_eq!(s.filtered.len(), 2);
    }

    #[test]
    fn enter_in_select_mode_transitions_to_confirm() {
        let mut s = build(&[("a", ""), ("b", "")]);
        let action = handle_select_key(&mut s, key(KeyCode::Enter));
        assert!(matches!(action, SelectAction::EnterConfirm));
    }

    #[test]
    fn enter_in_select_mode_does_nothing_when_not_confirmable() {
        let mut s = build(&[("a", "alpha")]);
        s.query = "zzz".into();
        s.refilter();
        // No matches and no selections — Enter should NOT transition.
        let action = handle_select_key(&mut s, key(KeyCode::Enter));
        assert!(matches!(action, SelectAction::Continue));
    }

    #[test]
    fn confirm_y_confirms() {
        assert!(matches!(
            handle_confirm_key(key(KeyCode::Char('y'))),
            ConfirmAction::Confirmed
        ));
        assert!(matches!(
            handle_confirm_key(key(KeyCode::Char('Y'))),
            ConfirmAction::Confirmed
        ));
    }

    #[test]
    fn confirm_other_keys_bounce_back() {
        // The whole point of the dialog is that a stray Enter or N does NOT
        // confirm — it goes back to select.
        for c in [
            key(KeyCode::Enter),
            key(KeyCode::Char('n')),
            key(KeyCode::Char('N')),
            key(KeyCode::Esc),
            key(KeyCode::Char(' ')),
            key(KeyCode::Char('j')),
        ] {
            assert!(matches!(handle_confirm_key(c), ConfirmAction::BackToSelect));
        }
    }

    #[test]
    fn ctrl_c_is_detected() {
        assert!(is_ctrl_c(&ctrl_key(KeyCode::Char('c'))));
        assert!(is_ctrl_c(&ctrl_key(KeyCode::Char('C'))));
        assert!(!is_ctrl_c(&key(KeyCode::Char('c'))));
        assert!(!is_ctrl_c(&ctrl_key(KeyCode::Char('x'))));
    }

    #[test]
    fn pending_remove_count_reflects_state() {
        let mut s = build(&[("a", ""), ("b", ""), ("c", "")]);
        assert_eq!(pending_remove_count(&s), 1); // cursor fallback
        s.selected[0] = true;
        s.selected[2] = true;
        assert_eq!(pending_remove_count(&s), 2);
        // Filter to nothing with no selections cleared - selections still count.
        s.query = "zzz".into();
        s.refilter();
        assert_eq!(pending_remove_count(&s), 2);
        // Now clear selections AND filter.
        s.selected = vec![false; 3];
        assert_eq!(pending_remove_count(&s), 0);
    }
}
