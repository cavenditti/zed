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
use std::sync::Arc;

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Window,
};

/// Action closure stored alongside each painted clickable. `Fn` (not
/// `FnOnce`) because every painted frame re-pushes the same Arc — the
/// overlay can invoke it without consuming the registry entry.
pub type ClickableAction = Arc<dyn Fn(&mut Window, &mut App) + 'static>;

thread_local! {
    static CLICKABLE_REGISTRY: RefCell<Vec<ClickableEntry>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone)]
struct ClickableEntry {
    bounds: Bounds<Pixels>,
    on_click: ClickableAction,
}

/// Drain every clickable entry registered on the current frame and
/// return them as `(bounds, on_click)` pairs. Called by the codon-jump
/// overlay on activation; subsequent paint passes refill the registry.
pub fn take_clickables() -> Vec<(Bounds<Pixels>, ClickableAction)> {
    CLICKABLE_REGISTRY.with(|cell| {
        cell.borrow_mut()
            .drain(..)
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
        F: Fn(&mut Window, &mut App) + 'static,
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
        CLICKABLE_REGISTRY.with(|cell| {
            cell.borrow_mut().push(ClickableEntry {
                bounds,
                on_click: self.on_click.clone(),
            });
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

    #[test]
    fn registry_drains_to_empty() {
        clear_clickable_registry();
        assert_eq!(clickable_registry_len(), 0);
        CLICKABLE_REGISTRY.with(|cell| {
            cell.borrow_mut().push(ClickableEntry {
                bounds: Bounds {
                    origin: Point {
                        x: px(0.0),
                        y: px(0.0),
                    },
                    size: Size {
                        width: px(10.0),
                        height: px(10.0),
                    },
                },
                on_click: Arc::new(|_, _| {}),
            });
        });
        assert_eq!(clickable_registry_len(), 1);
        let drained = take_clickables();
        assert_eq!(drained.len(), 1);
        assert_eq!(clickable_registry_len(), 0);
    }
}
