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
use gpui::{App, AsyncWindowContext, Bounds, Context, Entity, Pixels, Task, WeakEntity, Window};
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

/// Capture a [`LayoutSnapshot`] directly from a `Member` tree that is
/// not currently attached to a workspace (e.g. the cached `Member`
/// inside codon's `WindowRuntimeCache`). Behaves identically to
/// [`capture_layout`] but skips the `workspace.center.root` indirection.
///
/// Used by `c-skip-capture-on-cache-hit` to materialize a fresh
/// `LayoutSnapshot` from a runtime-cache entry on eviction / detach /
/// shutdown.
pub fn capture_from_member(
    member: &Member,
    window: &mut Window,
    cx: &mut App,
) -> LayoutSnapshot {
    capture_member(member, window, cx)
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
            } else if let Some(kind) = codon_pane_kind_for_item(handle.as_ref()) {
                // codon fallback: adapter-hosted panels expose their kind
                // (`Panel::persistent_name()`) but don't implement
                // `SerializableItem`. Capture them under that kind so
                // `apply_layout` can route them through the codon-pane
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

/// Async factory that rehydrates a codon adapter-hosted pane by its kind.
///
/// Codon hosts the seven Zed `impl Panel` types via a generic adapter
/// (`PanelItemAdapter<P>` in `codon-panes`) rather than the built-in
/// dock-host. To round-trip those adapter-hosted panes through
/// `LayoutSnapshot` we need a way to spawn the panel by its
/// `persistent_name()` kind string — there is no generic `Panel::load`
/// constructor we can call by type parameter.
pub type CodonPaneRestoreFn = fn(
    WeakEntity<Workspace>,
    AsyncWindowContext,
) -> Task<anyhow::Result<Box<dyn ItemHandle>>>;

/// Predicate that returns true when `handle` is the codon adapter for this
/// pane kind. Used during `capture_layout` to detect adapter-hosted panels
/// that don't implement `SerializableItem`.
pub type CodonPaneMatchesFn = fn(&dyn ItemHandle) -> bool;

/// Codon-only registry entry: one per adapter-hosted pane kind.
///
/// codon-panes registers one spec per converted panel during init via
/// [`codon_register_pane_kind`]. The registry replaces an earlier pair of
/// codon-side registration functions that split the same concern across
/// two static slots.
#[derive(Clone, Copy)]
pub struct CodonPaneKindSpec {
    /// The panel's `persistent_name()`. Used as the `kind` tag in
    /// `ItemSnapshot` and as the lookup key for restoration.
    pub kind: &'static str,
    /// True when `handle` is the adapter for this kind. Called from
    /// `capture_layout` for every non-`SerializableItem` handle.
    pub matches: CodonPaneMatchesFn,
    /// Async factory that runs the panel's existing `load` constructor and
    /// wraps the result in `PanelItemAdapter`.
    pub restore: CodonPaneRestoreFn,
}

static CODON_PANE_KINDS: OnceLock<RwLock<HashMap<&'static str, CodonPaneKindSpec>>> =
    OnceLock::new();

fn codon_pane_kind_map() -> &'static RwLock<HashMap<&'static str, CodonPaneKindSpec>> {
    CODON_PANE_KINDS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register a codon adapter-hosted pane kind.
///
/// Idempotent: re-registering the same `kind` overwrites the prior spec.
/// Called by `codon-panes::init` once per converted panel.
pub fn codon_register_pane_kind(spec: CodonPaneKindSpec) {
    codon_pane_kind_map().write().insert(spec.kind, spec);
}

/// Look up a previously-registered codon pane kind spec. Returns `None`
/// when the kind has no codon-panes registration (e.g. when the snapshot
/// was captured against a build without codon-panes, or before the
/// registration ran).
pub fn codon_pane_kind_spec(kind: &str) -> Option<CodonPaneKindSpec> {
    codon_pane_kind_map().read().get(kind).copied()
}

fn codon_pane_kind_for_item(handle: &dyn ItemHandle) -> Option<&'static str> {
    let map = codon_pane_kind_map().read();
    for spec in map.values() {
        if (spec.matches)(handle) {
            return Some(spec.kind);
        }
    }
    None
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

/// Callback fired at the end of [`Workspace::restore_center_root`] when
/// the codon-session crate has installed a trace recorder. The first
/// argument is the wall-clock duration of the restore in milliseconds;
/// the second is the count of previously-unseen panes that the restore
/// attached to the workspace (used by the harness to correlate
/// retained-vs-new pane budgets).
///
/// Wired through a function pointer so the vendored crate does not have
/// to import the codon trace types — the codon-session crate installs
/// the callback during its `init` and forwards into
/// `file_manager::record_switch_timing`.
pub type CodonRestoreTimingFn = fn(restore_ms: f32, new_pane_count: u32);

static CODON_RESTORE_TIMING_CB: OnceLock<CodonRestoreTimingFn> = OnceLock::new();

/// Install the restore-timing callback. Idempotent: the first install
/// wins so a re-`init` (e.g. in tests) does not overwrite the active
/// recorder; subsequent calls return the already-installed callback.
pub fn set_restore_timing_callback(cb: CodonRestoreTimingFn) {
    if let Err(_existing) = CODON_RESTORE_TIMING_CB.set(cb) {
        log::trace!("codon restore-timing callback already installed; ignoring re-install");
    }
}

/// Notify the installed restore-timing callback (if any). Called from
/// `Workspace::restore_center_root`.
pub fn notify_restore_timing(restore_ms: f32, new_pane_count: u32) {
    if let Some(cb) = CODON_RESTORE_TIMING_CB.get() {
        cb(restore_ms, new_pane_count);
    }
}

/// Pixel bounds of the workspace's currently active center pane.
///
/// Returns `None` before the first layout pass has measured the pane (the
/// center group records bounding boxes during `request_layout`). Used by
/// codon-which-key to size and position the chord HUD against the active
/// pane rather than the whole window — see
/// `REQ:codon/which-key-overlay#c-full-pane-width`.
pub fn active_pane_bounds(workspace: &Workspace) -> Option<Bounds<Pixels>> {
    workspace.bounding_box_for_pane(workspace.active_pane())
}
