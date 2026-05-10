//! Minimal public bridge for the `codon-session` crate.
//!
//! Exposes a serde-friendly snapshot of the workspace's center pane group
//! plus a way to swap a new layout in. Used to implement tmux-style sessions
//! and windows in the codon fork without making the upstream persistence
//! types public.
//!
//! The shapes here are intentionally minimal — only what's needed to round-trip
//! a layout. Item-level state (cursor, scroll, terminal cwd) continues to
//! restore through Zed's existing `SerializableItem` machinery, since panes
//! re-deserialize the same item ids.

use std::sync::Arc;

use anyhow::Result;
use gpui::{App, Context, Entity, Task, Window};
use serde::{Deserialize, Serialize};

use crate::{
    Member, Pane, PaneAxis, Workspace,
    persistence::{
        SerializedAxis,
        model::{SerializedItem, SerializedPane, SerializedPaneGroup},
    },
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LayoutSnapshot {
    Group {
        axis: SnapshotAxis,
        flexes: Option<Vec<f32>>,
        children: Vec<LayoutSnapshot>,
    },
    Stack {
        members: Vec<LayoutSnapshot>,
        active: usize,
    },
    Pane(PaneSnapshot),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotAxis {
    Horizontal,
    Vertical,
}

impl SnapshotAxis {
    fn into_gpui(self) -> gpui::Axis {
        match self {
            SnapshotAxis::Horizontal => gpui::Axis::Horizontal,
            SnapshotAxis::Vertical => gpui::Axis::Vertical,
        }
    }

    fn from_gpui(axis: gpui::Axis) -> Self {
        match axis {
            gpui::Axis::Horizontal => SnapshotAxis::Horizontal,
            gpui::Axis::Vertical => SnapshotAxis::Vertical,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub items: Vec<ItemSnapshot>,
    pub active: bool,
    #[serde(default)]
    pub pinned_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemSnapshot {
    pub kind: String,
    pub item_id: u64,
    pub active: bool,
    #[serde(default)]
    pub preview: bool,
}

impl LayoutSnapshot {
    pub fn empty_pane() -> Self {
        LayoutSnapshot::Pane(PaneSnapshot {
            items: Vec::new(),
            active: true,
            pinned_count: 0,
        })
    }

    fn into_serialized(self) -> SerializedPaneGroup {
        match self {
            LayoutSnapshot::Group {
                axis,
                flexes,
                children,
            } => SerializedPaneGroup::Group {
                axis: SerializedAxis(axis.into_gpui()),
                flexes,
                children: children
                    .into_iter()
                    .map(LayoutSnapshot::into_serialized)
                    .collect(),
            },
            LayoutSnapshot::Stack { members, active } => members
                .into_iter()
                .nth(active)
                .map(LayoutSnapshot::into_serialized)
                .unwrap_or_else(|| SerializedPaneGroup::Pane(SerializedPane::new(vec![], true, 0))),
            LayoutSnapshot::Pane(pane) => SerializedPaneGroup::Pane(SerializedPane::new(
                pane.items
                    .into_iter()
                    .map(|item| {
                        SerializedItem::new(item.kind, item.item_id, item.active, item.preview)
                    })
                    .collect(),
                pane.active,
                pane.pinned_count,
            )),
        }
    }
}

pub fn capture_layout(
    workspace: &Workspace,
    window: &mut Window,
    cx: &mut App,
) -> LayoutSnapshot {
    capture_member(&workspace.center.root, window, cx)
}

fn capture_member(member: &Member, window: &mut Window, cx: &mut App) -> LayoutSnapshot {
    match member {
        Member::Axis(axis) => capture_axis(axis, window, cx),
        Member::Pane(pane) => LayoutSnapshot::Pane(capture_pane(pane, window, cx)),
    }
}

fn capture_axis(axis: &PaneAxis, window: &mut Window, cx: &mut App) -> LayoutSnapshot {
    LayoutSnapshot::Group {
        axis: SnapshotAxis::from_gpui(axis.axis),
        flexes: Some(axis.flexes.lock().clone()),
        children: axis
            .members
            .iter()
            .map(|m| capture_member(m, window, cx))
            .collect(),
    }
}

fn capture_pane(pane: &Entity<Pane>, window: &mut Window, cx: &mut App) -> PaneSnapshot {
    let pane_ref = pane.read(cx);
    let active_id = pane_ref.active_item().map(|item| item.item_id());
    let items = pane_ref
        .items()
        .filter_map(|handle| {
            let serializable = handle.to_serializable_item_handle(cx)?;
            Some(ItemSnapshot {
                kind: serializable.serialized_item_kind().to_string(),
                item_id: serializable.item_id().as_u64(),
                active: Some(serializable.item_id()) == active_id,
                preview: pane_ref.is_active_preview_item(serializable.item_id()),
            })
        })
        .collect();
    PaneSnapshot {
        items,
        active: pane_ref.has_focus(window, cx),
        pinned_count: pane_ref.pinned_count(),
    }
}

/// Replace the workspace's center group with `snapshot`. Existing panes are
/// dropped before the new ones are constructed; items re-hydrate via the
/// `SerializableItemRegistry`, so editor buffers and terminal connections
/// referenced by the same item id are preserved.
pub fn apply_layout(
    workspace: &mut Workspace,
    snapshot: LayoutSnapshot,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    workspace.replace_center_with_snapshot(snapshot.into_serialized(), window, cx)
}

/// Convenience for `Arc`-shared snapshots when callers want to hand them off
/// to background work without cloning the whole tree.
pub fn capture_arc(workspace: &Workspace, window: &mut Window, cx: &mut App) -> Arc<LayoutSnapshot> {
    Arc::new(capture_layout(workspace, window, cx))
}
