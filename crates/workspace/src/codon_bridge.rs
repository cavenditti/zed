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

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use gpui::{App, AsyncWindowContext, Context, Entity, Task, WeakEntity, Window};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::{
    Member, Pane, PaneAxis, Workspace,
    item::ItemHandle,
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
            if let Some(serializable) = handle.to_serializable_item_handle(cx) {
                Some(ItemSnapshot {
                    kind: serializable.serialized_item_kind().to_string(),
                    item_id: serializable.item_id().as_u64(),
                    active: Some(serializable.item_id()) == active_id,
                    preview: pane_ref.is_active_preview_item(serializable.item_id()),
                })
            } else if let Some(kind) = panel_kind_for_item(handle.as_ref()) {
                // codon fallback: adapter-hosted panels expose their kind
                // (`Panel::persistent_name()`) but don't implement
                // `SerializableItem`. Capture them under that kind so
                // `apply_layout` can route them through the panel-restorer
                // registry on rehydrate.
                Some(ItemSnapshot {
                    kind: kind.to_string(),
                    item_id: handle.item_id().as_u64(),
                    active: Some(handle.item_id()) == active_id,
                    preview: pane_ref.is_active_preview_item(handle.item_id()),
                })
            } else {
                None
            }
        })
        .collect();
    PaneSnapshot {
        items,
        active: pane_ref.has_focus(window, cx),
        pinned_count: pane_ref.pinned_count(),
    }
}

/// Codon-only hook: codon-panes installs this to surface the
/// `persistent_name` of adapter-hosted panels for capture. Without it,
/// adapter items are silently dropped by `capture_layout` (they aren't
/// `SerializableItem`). See `lookup_panel_restorer` for the matching
/// rehydrate hook.
pub type ItemPanelKindFn = fn(&dyn ItemHandle) -> Option<&'static str>;
static ITEM_PANEL_KIND: OnceLock<RwLock<Option<ItemPanelKindFn>>> = OnceLock::new();

fn item_panel_kind_slot() -> &'static RwLock<Option<ItemPanelKindFn>> {
    ITEM_PANEL_KIND.get_or_init(|| RwLock::new(None))
}

/// Codon-side: register the function that detects whether a given
/// `ItemHandle` is a `PanelItemAdapter<P>` and returns the panel's
/// `persistent_name`. Called once from `codon-panes::init`.
pub fn register_item_panel_kind_fn(f: ItemPanelKindFn) {
    *item_panel_kind_slot().write() = Some(f);
}

fn panel_kind_for_item(handle: &dyn ItemHandle) -> Option<&'static str> {
    let f = *item_panel_kind_slot().read();
    f.and_then(|f| f(handle))
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

/// Factory signature used by the panel-restorer registry below.
///
/// Codon hosts the seven Zed `impl Panel` types via a generic adapter
/// (`PanelItemAdapter<P>` in `codon-panes`) rather than the built-in
/// dock-host. To round-trip those adapter-hosted panes through
/// `LayoutSnapshot` we need a way to spawn the panel by its
/// `persistent_name()` kind string — there is no generic `Panel::load`
/// constructor we can call by type parameter.
///
/// codon-panes registers one factory per converted panel during init.
/// The async closure runs the panel's existing `load` constructor and
/// wraps the resulting entity in the adapter.
pub type PanelRestorerFn = fn(
    WeakEntity<Workspace>,
    AsyncWindowContext,
) -> Task<anyhow::Result<Box<dyn ItemHandle>>>;

static PANEL_RESTORERS: OnceLock<RwLock<HashMap<&'static str, PanelRestorerFn>>> = OnceLock::new();

fn restorer_map() -> &'static RwLock<HashMap<&'static str, PanelRestorerFn>> {
    PANEL_RESTORERS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register a panel restorer for the given `persistent_name` kind.
///
/// Idempotent: re-registering the same kind overwrites the prior factory.
/// Called by `codon-panes::init` once per converted panel; safe to call
/// from non-test paths only.
pub fn register_panel_restorer(kind: &'static str, factory: PanelRestorerFn) {
    restorer_map().write().insert(kind, factory);
}

/// Look up a previously-registered panel restorer for `kind`. Returns
/// `None` when the kind has no codon-panes registration (e.g. when the
/// snapshot was captured against a build without codon-panes, or before
/// the restorer registration ran).
pub fn lookup_panel_restorer(kind: &str) -> Option<PanelRestorerFn> {
    restorer_map().read().get(kind).copied()
}
