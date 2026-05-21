//! `JumpClickable` element wrapper + paint-time clickable registry.
//!
//! Vendored helper for the codon-jump overlay (`crates/codon-jump`).
//! Lives inside the `workspace` crate so adoption sites scattered
//! across vendored Zed UI crates (workspace/dock, status_bar,
//! title_bar, git_ui, agent_ui, project_panel, notifications) can
//! depend on a single common location without pulling in `codon-jump`
//! and creating a cycle (codon-jump itself depends on `workspace`
//! for `ModalView` + `toggle_modal`).
//!
//! The codon-jump overlay re-exports [`take_clickables`] and the
//! [`JumpClickableExt`] trait from this module.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Window,
};

/// Action closure stored alongside each painted clickable. `Fn` (not
/// `FnOnce`) because every painted frame re-pushes the same Arc — the
/// overlay can invoke it without consuming the registry entry. `Send`
/// matches the bound on `codon_jump::JumpCandidate::action` so the
/// overlay can wrap us directly without bridging trait objects.
pub type ClickableAction = Arc<dyn Fn(&mut Window, &mut App) + 'static>;

thread_local! {
    static CLICKABLE_REGISTRY: RefCell<Vec<ClickableEntry>> = const { RefCell::new(Vec::new()) };
}

/// Anything older than this on drain is dropped — covers windows where
/// the application stopped painting (sleep, hidden window) but the
/// registry still holds stale entries. Two paint cycles at 60 Hz is
/// ~33 ms, so 250 ms also tolerates a few skipped frames without losing
/// a still-visible clickable.
const ENTRY_FRESHNESS: Duration = Duration::from_millis(250);

/// Soft cap on registry size. When exceeded, the push path evicts every
/// entry older than [`ENTRY_FRESHNESS`] before appending. This bounds
/// memory growth when the overlay is never opened without needing a
/// frame-boundary signal we can't reliably synthesize from inside paint.
/// Bound math: with ~50 clickables per frame at 60 Hz, one freshness
/// window holds ~750 entries; 10 000 buys 200 ms of slack before the
/// soft-cap path runs.
const SOFT_CAP: usize = 10_000;

#[derive(Clone)]
struct ClickableEntry {
    bounds: Bounds<Pixels>,
    on_click: ClickableAction,
    painted_at: Instant,
}

/// Drain every still-fresh clickable entry and return them as
/// `(bounds, on_click)` pairs, deduplicated so re-paints across the
/// freshness window only contribute one entry per visual element.
///
/// Why dedup here rather than wipe between frames: the registry is
/// refilled on every paint, but we can't observe paint-pass boundaries
/// from inside `JumpClickable::paint` without a Window-level hook. So
/// each frame appends and `take_clickables` collapses duplicates by
/// the bounds rectangle. The most-recently-painted entry for each
/// rectangle wins (so the freshest closure handle is the one the
/// overlay calls).
pub fn take_clickables() -> Vec<(Bounds<Pixels>, ClickableAction)> {
    CLICKABLE_REGISTRY.with(|cell| {
        let drained: Vec<ClickableEntry> = cell.borrow_mut().drain(..).collect();
        let mut latest_for_key: HashMap<BoundsKey, ClickableEntry> = HashMap::new();
        for entry in drained.into_iter() {
            if entry.painted_at.elapsed() >= ENTRY_FRESHNESS {
                continue;
            }
            latest_for_key.insert(BoundsKey::from(&entry.bounds), entry);
        }
        latest_for_key
            .into_values()
            .map(|entry| (entry.bounds, entry.on_click))
            .collect()
    })
}

/// Pixel-rounded bounds key used to dedupe re-paints across the
/// freshness window. Sub-pixel jitter from layout is collapsed onto a
/// whole-pixel grid so two paints of the same element key the same.
#[derive(Debug, PartialEq, Eq, Hash)]
struct BoundsKey {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl From<&Bounds<Pixels>> for BoundsKey {
    fn from(bounds: &Bounds<Pixels>) -> Self {
        Self {
            x: f32::from(bounds.origin.x).round() as i32,
            y: f32::from(bounds.origin.y).round() as i32,
            w: f32::from(bounds.size.width).round() as i32,
            h: f32::from(bounds.size.height).round() as i32,
        }
    }
}

/// Number of clickables currently registered. Test/debug helper.
#[doc(hidden)]
pub fn clickable_registry_len() -> usize {
    CLICKABLE_REGISTRY.with(|cell| cell.borrow().len())
}

/// Clear the registry without consuming. Test/debug helper.
#[doc(hidden)]
pub fn clear_clickable_registry() {
    CLICKABLE_REGISTRY.with(|cell| cell.borrow_mut().clear());
}

/// Internal push used by `JumpClickable::paint`. Appends unconditionally;
/// `take_clickables` is the side that filters by freshness and dedups by
/// bounds. The soft-cap branch evicts stale entries when the registry
/// grows beyond [`SOFT_CAP`] — without this, an app that never opens the
/// overlay would accumulate entries forever.
fn push_clickable(entry: ClickableEntry) {
    CLICKABLE_REGISTRY.with(|cell| {
        let mut registry = cell.borrow_mut();
        if registry.len() >= SOFT_CAP {
            registry.retain(|e| e.painted_at.elapsed() < ENTRY_FRESHNESS);
        }
        registry.push(entry);
    });
}

/// Fluent extension that lets any element opt into the jump-clickable
/// overlay:
///
/// ```ignore
/// use workspace::codon_jump_clickable::{JumpClickableExt, JumpListenerExt};
///
/// h_flex()
///     .child("Tabs")
///     .on_click(cx.listener(|this, _, w, cx| this.activate(w, cx)))
///     .jump_target(cx.jump_listener(|this, w, cx| this.activate(w, cx)));
/// ```
///
/// `on_click` here should mirror the element's existing `on_click` —
/// the overlay invokes it directly when the user selects this
/// candidate's two-key label.
pub trait JumpClickableExt: IntoElement + Sized {
    fn jump_target<F>(self, on_click: F) -> JumpClickable<Self::Element>
    where
        F: Fn(&mut Window, &mut App) + 'static,
    {
        JumpClickable {
            inner: self.into_element(),
            on_click: Arc::new(on_click),
        }
    }
}

impl<E: IntoElement> JumpClickableExt for E {}

/// Mirror of `Context<T>::listener` for the `jump_target` shape. The
/// overlay's stored action takes `(window, cx)` — no event — so the
/// standard `cx.listener(|this, _, w, cx| ...)` closure has the wrong
/// arity. `jump_listener` strips the event slot and keeps the same
/// weak-update plumbing.
pub trait JumpListenerExt<T: 'static> {
    fn jump_listener<F>(&self, f: F) -> Box<dyn Fn(&mut Window, &mut App) + 'static>
    where
        F: Fn(&mut T, &mut Window, &mut gpui::Context<T>) + 'static;
}

impl<T: 'static> JumpListenerExt<T> for gpui::Context<'_, T> {
    fn jump_listener<F>(&self, f: F) -> Box<dyn Fn(&mut Window, &mut App) + 'static>
    where
        F: Fn(&mut T, &mut Window, &mut gpui::Context<T>) + 'static,
    {
        let weak = self.entity().downgrade();
        Box::new(move |window, cx| {
            weak.update(cx, |this, cx| f(this, window, cx)).ok();
        })
    }
}

/// Element wrapper that registers its paint-time bounds into
/// [`CLICKABLE_REGISTRY`]. Composes transparently: request_layout /
/// prepaint forward to the inner element, then paint runs the inner
/// paint first and pushes a single registry entry after it returns.
pub struct JumpClickable<E> {
    inner: E,
    on_click: ClickableAction,
}

impl<E: Element> Element for JumpClickable<E> {
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.inner.id()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.inner.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.inner.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> E::PrepaintState {
        self.inner
            .prepaint(id, inspector_id, bounds, state, window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.paint(
            id,
            inspector_id,
            bounds,
            request_layout,
            prepaint,
            window,
            cx,
        );
        push_clickable(ClickableEntry {
            bounds,
            on_click: self.on_click.clone(),
            painted_at: Instant::now(),
        });
    }
}

impl<E: Element> IntoElement for JumpClickable<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Point, Size, px};

    fn unit_bounds() -> Bounds<Pixels> {
        Bounds {
            origin: Point {
                x: px(0.0),
                y: px(0.0),
            },
            size: Size {
                width: px(10.0),
                height: px(10.0),
            },
        }
    }

    fn push_at(bounds: Bounds<Pixels>) {
        push_clickable(ClickableEntry {
            bounds,
            on_click: Arc::new(|_, _| {}),
            painted_at: Instant::now(),
        });
    }

    fn shifted_bounds(dx: f32, dy: f32) -> Bounds<Pixels> {
        Bounds {
            origin: Point {
                x: px(dx),
                y: px(dy),
            },
            size: Size {
                width: px(10.0),
                height: px(10.0),
            },
        }
    }

    #[test]
    fn registry_drains_to_empty() {
        clear_clickable_registry();
        assert_eq!(clickable_registry_len(), 0);
        push_at(unit_bounds());
        assert_eq!(clickable_registry_len(), 1);
        let drained = take_clickables();
        assert_eq!(drained.len(), 1);
        assert_eq!(clickable_registry_len(), 0);
    }

    #[test]
    fn repeated_pushes_at_same_bounds_dedup_on_drain() {
        clear_clickable_registry();
        // Simulate three repaints of the same clickable across the
        // freshness window.
        push_at(unit_bounds());
        push_at(unit_bounds());
        push_at(unit_bounds());
        assert_eq!(clickable_registry_len(), 3);
        let drained = take_clickables();
        assert_eq!(drained.len(), 1);
    }

    #[test]
    fn pushes_separated_by_long_delay_within_window_all_survive() {
        // Regression: an earlier implementation cleared the registry
        // whenever consecutive pushes were >5 ms apart, which wiped
        // legitimate same-frame clickables sitting on either side of a
        // heavy element. Simulate that by hand-seeding two entries with
        // a wide gap between them, then pushing a third; all three
        // distinct rectangles must be returned on drain.
        clear_clickable_registry();
        CLICKABLE_REGISTRY.with(|cell| {
            let mut registry = cell.borrow_mut();
            registry.push(ClickableEntry {
                bounds: shifted_bounds(0.0, 0.0),
                on_click: Arc::new(|_, _| {}),
                painted_at: Instant::now() - Duration::from_millis(50),
            });
            registry.push(ClickableEntry {
                bounds: shifted_bounds(100.0, 0.0),
                on_click: Arc::new(|_, _| {}),
                painted_at: Instant::now() - Duration::from_millis(20),
            });
        });
        push_at(shifted_bounds(200.0, 0.0));
        let drained = take_clickables();
        assert_eq!(drained.len(), 3);
    }

    #[test]
    fn stale_entries_filtered_on_drain() {
        clear_clickable_registry();
        let stale = Instant::now() - ENTRY_FRESHNESS - Duration::from_millis(10);
        CLICKABLE_REGISTRY.with(|cell| {
            cell.borrow_mut().push(ClickableEntry {
                bounds: shifted_bounds(50.0, 50.0),
                on_click: Arc::new(|_, _| {}),
                painted_at: stale,
            });
        });
        push_at(unit_bounds());
        let drained = take_clickables();
        assert_eq!(drained.len(), 1);
    }

    #[test]
    fn soft_cap_evicts_stale_entries_to_make_room() {
        clear_clickable_registry();
        let stale = Instant::now() - ENTRY_FRESHNESS - Duration::from_millis(10);
        CLICKABLE_REGISTRY.with(|cell| {
            let mut registry = cell.borrow_mut();
            for index in 0..SOFT_CAP {
                registry.push(ClickableEntry {
                    bounds: shifted_bounds(index as f32, 0.0),
                    on_click: Arc::new(|_, _| {}),
                    painted_at: stale,
                });
            }
        });
        assert_eq!(clickable_registry_len(), SOFT_CAP);
        push_at(unit_bounds());
        // The push triggers the soft-cap branch, which evicts every
        // stale entry before appending the fresh one.
        assert_eq!(clickable_registry_len(), 1);
    }
}
