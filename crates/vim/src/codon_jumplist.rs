//! Additive surface for codon's `JumplistPicker`.
//!
//! Vim's "jumplist" in Zed is the focused pane's `NavHistory` — the same
//! structure `ctrl-i` / `ctrl-o` walk through `pane::GoForward` /
//! `pane::GoBack`. The fields of `NavHistoryState` are private, but
//! `NavHistory::for_each_entry` already iterates every recorded jump with
//! its resolved `ProjectPath` + optional absolute path.
//!
//! This module wraps that iteration into a typed `Vec<JumplistEntry>` that
//! the codon picker can consume without depending on `workspace`'s
//! internal types. The surface is intentionally read-only — picker rows
//! never mutate the jumplist. Confirmation in codon-pickers dispatches the
//! existing `pane::GoBack` / open-path actions instead.
//!
//! See `TASK:phase-16/pickers-jumplist` and `crates/codon-pickers/`.

use std::path::PathBuf;

use editor::Editor;
use gpui::{App, Context, Entity, Window};
use project::ProjectPath;
use util::ResultExt as _;
use workspace::Workspace;

use crate::Vim;

/// One row in the jumplist picker. Holds enough state to:
///
/// - Render a list item (`label` is the rendered path; `row` is the
///   1-based line if known, else `None`).
/// - Reopen the destination on confirm — the picker uses
///   `Workspace::open_path` with the `project_path`.
#[derive(Clone, Debug)]
pub struct JumplistEntry {
    pub project_path: ProjectPath,
    pub abs_path: Option<PathBuf>,
    /// 1-based row recorded at the jump site, when known. The picker
    /// formats `path:row` in the list label.
    pub row: Option<u32>,
    /// Logical ordering tag — `timestamp` from the underlying
    /// `NavigationEntry`. Picker rows are sorted descending so the most
    /// recent jump is on top.
    pub timestamp: usize,
}

impl Vim {
    /// Read the active pane's jumplist as a flat `Vec` of entries, sorted
    /// most-recent-first. Returns an empty vec when the pane has no
    /// recorded jumps or when the workspace cannot be resolved (e.g. the
    /// editor is detached). Never panics.
    ///
    /// Note: vim's "jumplist" is conceptually the pane's `NavHistory`,
    /// which combines the backward, forward, and closed-item stacks. We
    /// surface all three so the picker can reach a destination the user
    /// closed earlier this session — Helix's jumplist behaves the same
    /// way.
    pub fn codon_jumplist_entries(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Vec<JumplistEntry> {
        let Some(pane) = self.pane(window, cx) else {
            return Vec::new();
        };
        let pane = pane.read(cx);
        let nav_history = pane.nav_history();
        let mut entries: Vec<JumplistEntry> = Vec::new();
        nav_history.for_each_entry(cx, &mut |entry, (project_path, abs_path)| {
            entries.push(JumplistEntry {
                project_path,
                abs_path,
                row: entry.row,
                timestamp: entry.timestamp,
            });
        });
        // Newest first; ties broken by abs_path to give a stable order
        // when timestamps collide (the path field's Ord is total).
        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| a.abs_path.cmp(&b.abs_path)));
        entries.dedup_by(|a, b| a.project_path == b.project_path && a.row == b.row);
        entries
    }
}

/// Workspace-flavoured variant for callers that do not hold a `Vim`
/// entity. Walks every pane and collects their jumplist entries into a
/// single newest-first list. Codon's picker uses this when the focused
/// pane is not an editor (e.g. a terminal) so the jumplist still surfaces.
pub fn workspace_jumplist_entries(
    workspace: &Workspace,
    cx: &App,
) -> Vec<JumplistEntry> {
    let mut entries: Vec<JumplistEntry> = Vec::new();
    for pane in workspace.panes() {
        let nav_history = pane.read(cx).nav_history().clone();
        nav_history.for_each_entry(cx, &mut |entry, (project_path, abs_path)| {
            entries.push(JumplistEntry {
                project_path,
                abs_path,
                row: entry.row,
                timestamp: entry.timestamp,
            });
        });
    }
    entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| a.abs_path.cmp(&b.abs_path)));
    entries.dedup_by(|a, b| a.project_path == b.project_path && a.row == b.row);
    entries
}

/// Convenience for codon-pickers — given a workspace, return its
/// jumplist. Falls back to the per-pane walk above when no editor is
/// focused. Marked `pub` only because the picker lives in a separate
/// crate; the inner `Workspace::panes` access stays private to
/// `workspace`.
pub fn for_workspace(workspace: &Entity<Workspace>, cx: &App) -> Vec<JumplistEntry> {
    workspace_jumplist_entries(workspace.read(cx), cx)
}

/// Convenience overload that takes a `WeakEntity<Workspace>` so the picker
/// can call it without first upgrading.
pub fn for_weak_workspace(
    workspace: &gpui::WeakEntity<Workspace>,
    cx: &App,
) -> Vec<JumplistEntry> {
    let Some(workspace) = workspace.upgrade() else {
        return Vec::new();
    };
    workspace_jumplist_entries(workspace.read(cx), cx)
}

/// Helper for `JumplistPicker::confirm` — open the entry's project path
/// in the workspace's active pane and scroll to the recorded row. The
/// returned `gpui::Task` resolves once the editor has been opened (and
/// the diff has finished loading, if applicable) and the cursor has been
/// positioned. Errors during open are swallowed — the picker treats a
/// failed jump as a silent no-op.
pub fn jump_to_entry(
    workspace: &Entity<Workspace>,
    entry: &JumplistEntry,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Task<()> {
    let project_path = entry.project_path.clone();
    let row = entry.row;
    let open_task = workspace.update(cx, |workspace, cx| {
        workspace.open_path(project_path, None, true, window, cx)
    });
    window.spawn(cx, async move |cx| {
        let Ok(item) = open_task.await else { return };
        let Some(editor) = item.downcast::<Editor>() else {
            return;
        };
        let diff_task =
            editor.update(cx, |editor, _cx| editor.wait_for_diff_to_load());
        if let Some(diff_task) = diff_task {
            diff_task.await;
        }
        if let Some(target_row) = row {
            cx.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    let point = language::Point::new(target_row.saturating_sub(1), 0);
                    editor.change_selections(
                        editor::SelectionEffects::scroll(
                            editor::scroll::Autoscroll::center(),
                        ),
                        window,
                        cx,
                        |selections| selections.select_ranges([point..point]),
                    );
                });
            })
            .log_err();
        }
    })
}
