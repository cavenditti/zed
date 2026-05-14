//! Terminal-side bridge between the codon-jump overlay and the
//! alacritty visible grid. Mirrors `editor::codon_jump_provider` and
//! the `codon_bridge` vendored-helper pattern.
//!
//! The provider walks `Terminal::last_content().cells` (the alacritty
//! display iterator snapshot taken on every paint) and yields one
//! [`JumpCandidate`] per:
//!
//! - **Word**: a run of two or more whitespace-free, non-control cells.
//! - **URL**: a `URL_REGEX` match over the concatenated visible text.
//!
//! Off-screen scrollback is implicitly excluded because the cells in
//! `last_content` only contain the visible viewport.

use std::sync::Arc;

use codon_jump::{JumpCandidate, JumpContext, JumpKind, JumpMode, JumpProvider, JumpRegistry};
use gpui::{App, Bounds, ClipboardItem, Entity, Focusable, Pixels, Point, Size, WeakEntity};
use regex::Regex;
use terminal::alacritty_terminal::index::{Column as AlacColumn, Point as AlacPoint};
use terminal::{IndexedCell, Terminal, TerminalBounds};

use crate::TerminalView;

/// The same URL pattern `terminal::terminal_hyperlinks` uses for hover
/// detection. Duplicated here (rather than re-exported) to keep the
/// hyperlink module's `pub(super)` shape intact.
const URL_REGEX: &str = r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`']+"#;

fn url_regex() -> &'static Regex {
    use std::sync::OnceLock;
    static URL: OnceLock<Regex> = OnceLock::new();
    URL.get_or_init(|| Regex::new(URL_REGEX).expect("URL_REGEX is a valid pattern"))
}

/// Codon-jump provider backed by a single `TerminalView`. Registered
/// from `TerminalView::new` for the lifetime of the view.
pub struct TerminalJumpProvider {
    terminal_view: WeakEntity<TerminalView>,
}

impl TerminalJumpProvider {
    pub fn register(view: &Entity<TerminalView>, cx: &mut App) {
        let provider = Arc::new(TerminalJumpProvider {
            terminal_view: view.downgrade(),
        });
        JumpRegistry::register(cx, provider);
    }
}

impl JumpProvider for TerminalJumpProvider {
    fn collect(&self, ctx: &JumpContext, cx: &mut App) -> Vec<JumpCandidate> {
        let Some(view_handle) = self.terminal_view.upgrade() else {
            return Vec::new();
        };
        let mode = ctx.mode;
        let view_weak = self.terminal_view.clone();
        view_handle.read_with(cx, |view, cx| {
            // Mirrors the editor freshness gate: a hidden TerminalView
            // still owns a populated `Terminal::last_content` from its
            // last paint, so without this check it would emit ghost
            // candidates pointing at wherever the terminal used to sit.
            const PAINT_FRESHNESS: std::time::Duration =
                std::time::Duration::from_millis(250);
            let Some(last_painted_at) = view.last_painted_at else {
                return Vec::new();
            };
            if last_painted_at.elapsed() > PAINT_FRESHNESS {
                return Vec::new();
            }
            let terminal = view.terminal();
            terminal.read_with(cx, |terminal, _| {
                collect_inner(view_weak.clone(), terminal, mode)
            })
        })
    }

    fn is_alive(&self, _cx: &App) -> bool {
        self.terminal_view.upgrade().is_some()
    }
}

fn collect_inner(
    view_weak: WeakEntity<TerminalView>,
    terminal: &Terminal,
    mode: JumpMode,
) -> Vec<JumpCandidate> {
    let content = terminal.last_content();
    let bounds = content.terminal_bounds;
    if bounds.bounds.size.width <= Pixels::ZERO || bounds.bounds.size.height <= Pixels::ZERO {
        return Vec::new();
    }

    let mut out = Vec::new();

    if !matches!(mode, JumpMode::Url) {
        for token in walk_visible_word_tokens(&content.cells) {
            let candidate_bounds = cell_to_window_bounds(token.start, &bounds);
            out.push(make_word_candidate(
                view_weak.clone(),
                candidate_bounds,
                token.start,
            ));
        }
    }

    for hit in find_visible_urls(&content.cells, bounds.num_columns()) {
        let candidate_bounds = cell_to_window_bounds(hit.start, &bounds);
        out.push(make_url_candidate(
            view_weak.clone(),
            candidate_bounds,
            hit.url,
        ));
    }

    out
}

fn make_word_candidate(
    view_weak: WeakEntity<TerminalView>,
    bounds: Bounds<Pixels>,
    cell: AlacPoint,
) -> JumpCandidate {
    let action: Box<dyn FnOnce(&mut gpui::Window, &mut App)> = Box::new(move |window, cx| {
        let Some(view) = view_weak.upgrade() else {
            return;
        };
        let handle = view.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        view.update(cx, |view, cx| {
            view.terminal().update(cx, |terminal, _cx| {
                terminal.select_word_at_cell(cell);
            });
        });
    });
    JumpCandidate {
        bounds,
        kind: JumpKind::Word,
        action,
    }
}

fn make_url_candidate(
    view_weak: WeakEntity<TerminalView>,
    bounds: Bounds<Pixels>,
    url: String,
) -> JumpCandidate {
    let url_for_kind = url.clone();
    let action: Box<dyn FnOnce(&mut gpui::Window, &mut App)> = Box::new(move |window, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(url));
        if let Some(view) = view_weak.upgrade() {
            let handle = view.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
    });
    JumpCandidate {
        bounds,
        kind: JumpKind::Url(url_for_kind),
        action,
    }
}

/// Convert an alacritty grid `point` to its top-left pixel bounds in
/// window-absolute coordinates. The terminal element's `terminal_bounds`
/// origin is already window-absolute (see `terminal_element.rs:993`).
fn cell_to_window_bounds(point: AlacPoint, bounds: &TerminalBounds) -> Bounds<Pixels> {
    let line = point.line.0.max(0) as f32;
    let column = point.column.0 as f32;
    let x = bounds.bounds.origin.x + bounds.cell_width * column;
    let y = bounds.bounds.origin.y + bounds.line_height * line;
    Bounds {
        origin: Point { x, y },
        size: Size {
            width: bounds.cell_width * 2.0,
            height: bounds.line_height,
        },
    }
}

/// A contiguous run of word-character cells (>=2 chars), tagged with
/// the alacritty grid point of its first cell.
#[derive(Debug, Clone, Copy)]
pub struct WordToken {
    pub start: AlacPoint,
}

/// Walk the cell list and return the start of every >=2-char word
/// token. Words are separated by whitespace or non-printable cells.
pub fn walk_visible_word_tokens(cells: &[IndexedCell]) -> Vec<WordToken> {
    let mut tokens = Vec::new();
    let mut in_word = false;
    let mut current_start: Option<AlacPoint> = None;
    let mut char_count = 0usize;
    let mut current_line: Option<i32> = None;

    for indexed in cells {
        let ch = indexed.cell.c;
        let line = indexed.point.line.0;
        let line_changed = current_line != Some(line);
        if line_changed && in_word {
            if char_count >= 2
                && let Some(start) = current_start
            {
                tokens.push(WordToken { start });
            }
            in_word = false;
            current_start = None;
            char_count = 0;
        }
        current_line = Some(line);

        if is_word_char(ch) {
            if !in_word {
                in_word = true;
                current_start = Some(indexed.point);
                char_count = 0;
            }
            char_count += 1;
        } else if in_word {
            if char_count >= 2
                && let Some(start) = current_start
            {
                tokens.push(WordToken { start });
            }
            in_word = false;
            current_start = None;
            char_count = 0;
        }
    }
    if in_word
        && char_count >= 2
        && let Some(start) = current_start
    {
        tokens.push(WordToken { start });
    }
    tokens
}

/// A URL match in the visible grid, tagged with the alacritty grid
/// point of its first character.
#[derive(Debug, Clone)]
pub struct UrlHit {
    pub start: AlacPoint,
    pub url: String,
}

/// Run `URL_REGEX` over the concatenated visible text and report every
/// match with its starting cell. `columns` is the terminal width — used
/// to insert newlines between lines so URLs never span line breaks.
pub fn find_visible_urls(cells: &[IndexedCell], columns: usize) -> Vec<UrlHit> {
    if cells.is_empty() {
        return Vec::new();
    }
    let columns = columns.max(1);
    // Build per-line text, recording each char's grid point.
    let mut text = String::with_capacity(cells.len());
    let mut point_for_byte: Vec<AlacPoint> = Vec::with_capacity(cells.len());

    let mut last_line: Option<i32> = None;
    let mut last_col: i32 = -1;
    for indexed in cells {
        let line = indexed.point.line.0;
        let col = indexed.point.column.0 as i32;
        if let Some(prev) = last_line
            && prev != line
        {
            // Newline between rows.
            text.push('\n');
            point_for_byte.push(AlacPoint {
                line: indexed.point.line,
                column: AlacColumn(0),
            });
            last_col = -1;
        }
        // Pad any column gaps with spaces so byte offsets align.
        while last_col + 1 < col && (last_col + 1) < columns as i32 {
            text.push(' ');
            point_for_byte.push(AlacPoint {
                line: indexed.point.line,
                column: AlacColumn((last_col + 1) as usize),
            });
            last_col += 1;
        }
        let ch = indexed.cell.c;
        let pushed_ch = if ch == '\0' { ' ' } else { ch };
        let prev_len = text.len();
        text.push(pushed_ch);
        for _ in prev_len..text.len() {
            point_for_byte.push(indexed.point);
        }
        last_col = col;
        last_line = Some(line);
    }

    let mut out = Vec::new();
    for m in url_regex().find_iter(&text) {
        let Some(start_point) = point_for_byte.get(m.start()).copied() else {
            continue;
        };
        out.push(UrlHit {
            start: start_point,
            url: m.as_str().to_string(),
        });
    }
    out
}

fn is_word_char(ch: char) -> bool {
    !ch.is_whitespace() && !ch.is_control() && ch != '\0'
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal::alacritty_terminal::index::{Column, Line};
    use terminal::alacritty_terminal::term::cell::Cell;

    fn make_cell(line: i32, column: usize, ch: char) -> IndexedCell {
        let mut cell = Cell::default();
        cell.c = ch;
        IndexedCell {
            point: AlacPoint {
                line: Line(line),
                column: Column(column),
            },
            cell,
        }
    }

    #[test]
    fn walk_visible_word_tokens_splits_on_whitespace() {
        let cells = vec![
            make_cell(0, 0, 'h'),
            make_cell(0, 1, 'i'),
            make_cell(0, 2, ' '),
            make_cell(0, 3, 'y'),
            make_cell(0, 4, 'o'),
        ];
        let tokens = walk_visible_word_tokens(&cells);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].start.column.0, 0);
        assert_eq!(tokens[1].start.column.0, 3);
    }

    #[test]
    fn walk_visible_word_tokens_skips_single_char_words() {
        let cells = vec![
            make_cell(0, 0, 'a'),
            make_cell(0, 1, ' '),
            make_cell(0, 2, 'b'),
            make_cell(0, 3, 'c'),
        ];
        let tokens = walk_visible_word_tokens(&cells);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].start.column.0, 2);
    }

    #[test]
    fn walk_visible_word_tokens_splits_on_line_change() {
        let cells = vec![
            make_cell(0, 0, 'a'),
            make_cell(0, 1, 'b'),
            make_cell(1, 0, 'c'),
            make_cell(1, 1, 'd'),
        ];
        let tokens = walk_visible_word_tokens(&cells);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].start.line.0, 0);
        assert_eq!(tokens[1].start.line.0, 1);
    }

    #[test]
    fn find_visible_urls_matches_https() {
        let url = "https://example.com";
        let cells: Vec<IndexedCell> = url
            .char_indices()
            .map(|(i, c)| make_cell(0, i, c))
            .collect();
        let hits = find_visible_urls(&cells, 80);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, url);
        assert_eq!(hits[0].start.column.0, 0);
    }

    #[test]
    fn find_visible_urls_ignores_plain_text() {
        let cells: Vec<IndexedCell> = "hello world"
            .char_indices()
            .map(|(i, c)| make_cell(0, i, c))
            .collect();
        let hits = find_visible_urls(&cells, 80);
        assert!(hits.is_empty());
    }
}
