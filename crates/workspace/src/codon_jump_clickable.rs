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

use std::cell::{Cell, RefCell};
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
pub type ClickableAction = Arc<dyn Fn(&mut Window, &mut App) + Send + 'static>;

thread_local! {
    static CLICKABLE_REGISTRY: RefCell<Vec<ClickableEntry>> = const { RefCell::new(Vec::new()) };
    /// Wall-clock time of the most recent `JumpClickable::paint`. Used to
    /// detect frame boundaries: paints within the same frame happen in
    /// microseconds of each other, so a gap larger than
    /// `FRAME_RESET_THRESHOLD` means we've entered a new frame and the
    /// registry should be cleared before the next push.
    static LAST_PUSH_AT: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Anything older than this on drain is dropped — covers windows where
/// the application stopped painting (sleep, hidden window) but the
/// registry still holds stale entries.
const ENTRY_FRESHNESS: Duration = Duration::from_millis(250);
/// Gap between consecutive `paint` calls that marks a new frame. Same
/// frame paints share microseconds; >5ms means a frame boundary.
const FRAME_RESET_THRESHOLD: Duration = Duration::from_millis(5);

#[derive(Clone)]
struct ClickableEntry {
    bounds: Bounds<Pixels>,
    on_click: ClickableAction,
    painted_at: Instant,
}

/// Drain every clickable entry registered on the current frame and
/// return them as `(bounds, on_click)` pairs. Called by the codon-jump
/// overlay on activation; subsequent paint passes refill the registry.
pub fn take_clickables() -> Vec<(Bounds<Pixels>, ClickableAction)> {
    CLICKABLE_REGISTRY.with(|cell| {
        cell.borrow_mut()
            .drain(..)
            .filter(|entry| entry.painted_at.elapsed() < ENTRY_FRESHNESS)
            .map(|entry| (entry.bounds, entry.on_click))
            .collect()
    })
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
    LAST_PUSH_AT.with(|c| c.set(None));
}

/// Internal push used by `JumpClickable::paint`. Detects frame
/// boundaries via `LAST_PUSH_AT` and clears the registry before the
/// first push of a new frame — without this, every paint at 60 Hz
/// would accumulate entries forever between modal opens.
fn push_clickable(entry: ClickableEntry) {
    let now = entry.painted_at;
    let crossed_frame_boundary = LAST_PUSH_AT.with(|c| match c.get() {
        Some(previous) => now.saturating_duration_since(previous) > FRAME_RESET_THRESHOLD,
        None => true,
    });
    if crossed_frame_boundary {
        CLICKABLE_REGISTRY.with(|cell| cell.borrow_mut().clear());
    }
    LAST_PUSH_AT.with(|c| c.set(Some(now)));
    CLICKABLE_REGISTRY.with(|cell| cell.borrow_mut().push(entry));
}

/// Fluent extension that lets any element opt into the jump-clickable
/// overlay:
///
/// ```ignore
/// use workspace::codon_jump_clickable::JumpClickableExt;
///
/// h_flex()
///     .child("Tabs")
///     .on_click(cx.listener(|this, _, w, cx| this.activate(w, cx)))
///     .jump_target(cx.listener(|this, _, w, cx| this.activate(w, cx)));
/// ```
///
/// `on_click` here should mirror the element's existing `on_click` —
/// the overlay invokes it directly when the user selects this
/// candidate's two-key label.
pub trait JumpClickableExt: IntoElement + Sized {
    fn jump_target<F>(self, on_click: F) -> JumpClickable<Self::Element>
    where
        F: Fn(&mut Window, &mut App) + Send + 'static,
    {
        JumpClickable {
            inner: self.into_element(),
            on_click: Arc::new(on_click),
        }
    }
}

impl<E: IntoElement> JumpClickableExt for E {}

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

    fn push_now() {
        push_clickable(ClickableEntry {
            bounds: unit_bounds(),
            on_click: Arc::new(|_, _| {}),
            painted_at: Instant::now(),
        });
    }

    #[test]
    fn registry_drains_to_empty() {
        clear_clickable_registry();
        assert_eq!(clickable_registry_len(), 0);
        push_now();
        assert_eq!(clickable_registry_len(), 1);
        let drained = take_clickables();
        assert_eq!(drained.len(), 1);
        assert_eq!(clickable_registry_len(), 0);
    }

    #[test]
    fn push_after_frame_gap_clears_prior_entries() {
        clear_clickable_registry();
        push_now();
        push_now();
        assert_eq!(clickable_registry_len(), 2);
        // Simulate a frame boundary: backdate LAST_PUSH_AT well past
        // `FRAME_RESET_THRESHOLD` so the next push triggers a clear.
        let backdated = Instant::now() - FRAME_RESET_THRESHOLD - Duration::from_millis(20);
        LAST_PUSH_AT.with(|c| c.set(Some(backdated)));
        push_now();
        assert_eq!(clickable_registry_len(), 1);
    }

    #[test]
    fn stale_entries_filtered_on_drain() {
        clear_clickable_registry();
        // Bypass `push_clickable`'s frame-reset to seed a stale entry
        // directly. (`push_clickable` would clear if we backdate the
        // last-push timestamp; we want both stale + fresh in the
        // registry at the same time to test the drain-side filter.)
        let stale = Instant::now() - ENTRY_FRESHNESS - Duration::from_millis(10);
        CLICKABLE_REGISTRY.with(|cell| {
            cell.borrow_mut().push(ClickableEntry {
                bounds: unit_bounds(),
                on_click: Arc::new(|_, _| {}),
                painted_at: stale,
            });
        });
        push_now();
        let drained = take_clickables();
        assert_eq!(drained.len(), 1);
    }
}
