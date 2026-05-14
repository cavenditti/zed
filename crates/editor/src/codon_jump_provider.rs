//! Editor-side bridge between the codon-jump overlay and the editor's
//! cached paint state. Mirrors the `codon_bridge` vendored-helper
//! pattern: a small additive surface that lets the codon-jump registry
//! produce jump candidates without taking a `&mut Window`.
//!
//! The provider holds a [`WeakEntity<Editor>`] and on each `collect`
//! reads the editor's last-painted [`PositionMap`] to compute
//! window-absolute pixel bounds for every visible word + URL.
//! Candidates that fall outside the painted viewport are dropped.
//!
//! Two pure helpers live alongside the provider so other call sites
//! (notably `vim::helix::helix_jump_to_word`) can reuse them:
//!
//! - [`visible_word_anchors`] — anchored ranges of all visible
//!   words in a `MultiBufferSnapshot`.
//! - [`find_urls_in_range`] — `linkify`-driven URL scan over a buffer
//!   range, mirroring the single-point [`hover_links::find_url`] but
//!   yielding every match.

use std::ops::Range;
use std::sync::Arc;

use codon_jump::{JumpCandidate, JumpContext, JumpKind, JumpMode, JumpProvider, JumpRegistry};
use gpui::{App, Bounds, ClipboardItem, Entity, Focusable, Pixels, Point, Size, WeakEntity, px};
use linkify::{LinkFinder, LinkKind};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot};
use text::Bias;

use crate::display_map::{DisplayPoint, ToDisplayPoint};
use crate::{Editor, EditorMode, SelectionEffects};

/// Window-absolute pixel bounds for a single jump target, paired with
/// the metadata the provider's action closure needs to dispatch.
pub struct ResolvedCandidate {
    pub bounds: Bounds<Pixels>,
    pub kind: JumpKind,
    pub anchor_range: Range<Anchor>,
}

/// Codon-jump provider backed by a single editor. The provider stores a
/// [`WeakEntity<Editor>`] so it gets garbage-collected by the registry
/// after the editor drops.
pub struct EditorJumpProvider {
    editor: WeakEntity<Editor>,
}

impl EditorJumpProvider {
    /// Register this editor with the global [`JumpRegistry`]. Idempotent
    /// per editor — multiple registrations would each register a fresh
    /// provider, but [`Editor::new_internal`] only calls this once.
    pub fn register(editor_handle: &Entity<Editor>, cx: &mut App) {
        let provider = Arc::new(EditorJumpProvider {
            editor: editor_handle.downgrade(),
        });
        JumpRegistry::register(cx, provider);
    }
}

impl JumpProvider for EditorJumpProvider {
    fn collect(&self, ctx: &JumpContext, cx: &mut App) -> Vec<JumpCandidate> {
        let Ok(editor_handle) = self.editor.upgrade().ok_or(()) else {
            return Vec::new();
        };
        let mode = ctx.mode;
        editor_handle.read_with(cx, |editor, _| collect_inner(&self.editor, editor, mode))
    }

    fn is_alive(&self, _cx: &App) -> bool {
        self.editor.upgrade().is_some()
    }
}

fn collect_inner(
    editor_weak: &WeakEntity<Editor>,
    editor: &Editor,
    mode: JumpMode,
) -> Vec<JumpCandidate> {
    let resolved = collect_for_editor(editor, mode);
    let mut out = Vec::with_capacity(resolved.len());
    for ResolvedCandidate {
        bounds,
        kind,
        anchor_range,
    } in resolved
    {
        let editor_for_action = editor_weak.clone();
        let kind_for_action = kind.clone();
        let action: Box<dyn FnOnce(&mut gpui::Window, &mut App)> =
            Box::new(move |window, cx| {
                let Some(editor_handle) = editor_for_action.upgrade() else {
                    return;
                };
                match kind_for_action {
                    JumpKind::Url(ref url) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    }
                    JumpKind::Word | JumpKind::Clickable => {
                        let start = anchor_range.start;
                        let handle = editor_handle.read(cx).focus_handle(cx);
                        window.focus(&handle, cx);
                        editor_handle.update(cx, |editor, cx| {
                            editor.change_selections(
                                SelectionEffects::default(),
                                window,
                                cx,
                                |selections| {
                                    selections.select_anchor_ranges(std::iter::once(start..start));
                                },
                            );
                        });
                    }
                }
            });
        out.push(JumpCandidate {
            bounds,
            kind,
            action,
        });
    }
    out
}

/// Walk the editor's cached paint state and yield every visible word +
/// URL together with window-absolute pixel bounds. Public so the
/// `Editor::codon_jump_collect` shim can call into it.
pub fn collect_for_editor(editor: &Editor, mode: JumpMode) -> Vec<ResolvedCandidate> {
    if matches!(editor.mode, EditorMode::Minimap { .. }) {
        return Vec::new();
    }
    let Some(position_map) = editor.last_position_map.as_ref() else {
        return Vec::new();
    };

    // Editors that haven't painted recently still hold a stale
    // `last_position_map`. Their `text_hitbox` reflects where they
    // *used* to be on screen, so without this freshness gate they
    // emit ghost candidates that paint chips over wherever the
    // editor's old viewport sat. 250 ms is generous enough to cover
    // a single dropped frame on slow rendering paths while still
    // rejecting editors that have been hidden for any meaningful
    // amount of time.
    const PAINT_FRESHNESS: std::time::Duration = std::time::Duration::from_millis(250);
    let Some(last_painted_at) = editor.last_painted_at else {
        return Vec::new();
    };
    if last_painted_at.elapsed() > PAINT_FRESHNESS {
        return Vec::new();
    }

    let snapshot = &position_map.snapshot;
    let buffer = snapshot.buffer_snapshot();
    let display_snapshot = &snapshot.display_snapshot;

    let visible_rows = position_map.visible_row_range.clone();
    let scroll_row = position_map.scroll_position.y;
    let line_height = position_map.line_height;
    let text_origin = position_map.text_hitbox.bounds.origin;
    let scroll_pixel_x = position_map.scroll_pixel_position.x;
    let em_advance = position_map.em_advance;

    let start_display = DisplayPoint::new(visible_rows.start, 0);
    let end_display = DisplayPoint::new(visible_rows.end, 0);
    let start_point = display_snapshot.display_point_to_point(start_display, Bias::Left);
    let end_point = display_snapshot.display_point_to_point(end_display, Bias::Right);
    let start_offset = buffer.point_to_offset(start_point);
    let end_offset = buffer.point_to_offset(end_point);

    let mut out: Vec<ResolvedCandidate> = Vec::new();

    if !matches!(mode, JumpMode::Url) {
        for word in visible_word_anchors(buffer, start_offset..end_offset) {
            let display_start = word.start.to_display_point(display_snapshot);
            if !visible_rows.contains(&display_start.row()) {
                continue;
            }
            let row_ix = display_start
                .row()
                .0
                .saturating_sub(visible_rows.start.0) as usize;
            let Some(line_layout) = position_map.line_layouts.get(row_ix) else {
                continue;
            };
            let x_in_line = line_layout.x_for_index(display_start.column() as usize);
            let x = text_origin.x + x_in_line - Pixels::from(scroll_pixel_x);
            let y = text_origin.y
                + line_height
                    * (display_start.row().0 as f64 - scroll_row).max(0.0) as f32;
            let bounds = Bounds {
                origin: Point { x, y },
                size: Size {
                    width: em_advance * 2.0,
                    height: line_height,
                },
            };
            out.push(ResolvedCandidate {
                bounds,
                kind: JumpKind::Word,
                anchor_range: word,
            });
        }
    }

    for (range, url) in find_urls_in_range(buffer, start_offset..end_offset) {
        let display_start = range.start.to_display_point(display_snapshot);
        if !visible_rows.contains(&display_start.row()) {
            continue;
        }
        let row_ix = display_start
            .row()
            .0
            .saturating_sub(visible_rows.start.0) as usize;
        let Some(line_layout) = position_map.line_layouts.get(row_ix) else {
            continue;
        };
        let x_in_line = line_layout.x_for_index(display_start.column() as usize);
        let x = text_origin.x + x_in_line - Pixels::from(scroll_pixel_x);
        let y = text_origin.y
            + line_height * (display_start.row().0 as f64 - scroll_row).max(0.0) as f32;
        let bounds = Bounds {
            origin: Point { x, y },
            size: Size {
                width: em_advance * 2.0,
                height: line_height,
            },
        };
        out.push(ResolvedCandidate {
            bounds,
            kind: JumpKind::Url(url),
            anchor_range: range,
        });
    }

    out
}

/// Anchored ranges of every word in `range` of `buffer`. A "word" is a
/// run of two or more characters where each character is alphanumeric
/// or `_` — matching the helix-jump definition in `vim::helix`.
pub fn visible_word_anchors(
    buffer: &MultiBufferSnapshot,
    range: Range<MultiBufferOffset>,
) -> Vec<Range<Anchor>> {
    let mut out = Vec::new();
    if range.start >= range.end {
        return out;
    }

    let mut offset = range.start;
    let mut in_word = false;
    let mut word_start = range.start;
    let mut char_count = 0usize;

    for chunk in buffer.text_for_range(range.clone()) {
        for (idx, ch) in chunk.char_indices() {
            let absolute = offset + idx;
            let is_word = is_jump_word_char(ch);
            if is_word {
                if !in_word {
                    in_word = true;
                    word_start = absolute;
                    char_count = 0;
                }
                char_count += 1;
            } else if in_word {
                if char_count >= 2 {
                    out.push(buffer.anchor_after(word_start)..buffer.anchor_after(absolute));
                }
                in_word = false;
            }
        }
        offset += chunk.len();
    }

    if in_word && char_count >= 2 {
        out.push(buffer.anchor_after(word_start)..buffer.anchor_after(range.end));
    }

    out
}

/// Find every URL inside `range` of `buffer`, returning each as an
/// anchor range + the URL string. The sibling [`hover_links::find_url`]
/// answers the single-point query used by hover; this is the viewport
/// scan the jump-hint overlay needs.
pub fn find_urls_in_range(
    buffer: &MultiBufferSnapshot,
    range: Range<MultiBufferOffset>,
) -> Vec<(Range<Anchor>, String)> {
    if range.start >= range.end {
        return Vec::new();
    }
    let text: String = buffer.text_for_range(range.clone()).collect();
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);
    let mut out = Vec::new();
    for link in finder.links(&text) {
        let start_offset = range.start + link.start();
        let end_offset = range.start + link.end();
        let anchor_range =
            buffer.anchor_before(start_offset)..buffer.anchor_after(end_offset);
        out.push((anchor_range, link.as_str().to_string()));
    }
    out
}

fn is_jump_word_char(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

/// `Pixels` arithmetic helper — multiplies a pixel value by an `f64`
/// scroll offset without losing precision in the `f32` path.
#[allow(dead_code)]
fn pixel_times(p: Pixels, scalar: f64) -> Pixels {
    px(f32::from(p) * scalar as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_jump_word_char_excludes_punctuation() {
        assert!(is_jump_word_char('a'));
        assert!(is_jump_word_char('Z'));
        assert!(is_jump_word_char('0'));
        assert!(is_jump_word_char('_'));
        assert!(!is_jump_word_char(' '));
        assert!(!is_jump_word_char('.'));
        assert!(!is_jump_word_char('-'));
    }
}
