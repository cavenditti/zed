use gpui::{
    AnyView, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable as _, ManagedView,
    MouseButton, Subscription,
};
use ui::prelude::*;

#[derive(Debug)]
pub enum DismissDecision {
    Dismiss(bool),
    Pending,
}

pub trait ModalView: ManagedView {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> DismissDecision {
        DismissDecision::Dismiss(true)
    }

    fn fade_out_background(&self) -> bool {
        false
    }

    fn render_bare(&self) -> bool {
        false
    }
}

trait ModalViewHandle {
    fn on_before_dismiss(&mut self, window: &mut Window, cx: &mut App) -> DismissDecision;
    fn view(&self) -> AnyView;
    fn view_focus_handle(&self, cx: &App) -> FocusHandle;
    fn fade_out_background(&self, cx: &mut App) -> bool;
    fn render_bare(&self, cx: &mut App) -> bool;
}

impl<V: ModalView> ModalViewHandle for Entity<V> {
    fn on_before_dismiss(&mut self, window: &mut Window, cx: &mut App) -> DismissDecision {
        self.update(cx, |this, cx| this.on_before_dismiss(window, cx))
    }

    fn view(&self) -> AnyView {
        self.clone().into()
    }

    fn view_focus_handle(&self, cx: &App) -> FocusHandle {
        gpui::Focusable::focus_handle(self, cx)
    }

    fn fade_out_background(&self, cx: &mut App) -> bool {
        self.read(cx).fade_out_background()
    }

    fn render_bare(&self, cx: &mut App) -> bool {
        self.read(cx).render_bare()
    }
}

pub struct ActiveModal {
    modal: Box<dyn ModalViewHandle>,
    _subscriptions: [Subscription; 2],
    previous_focus_handle: Option<FocusHandle>,
    focus_handle: FocusHandle,
}

pub struct ModalLayer {
    active_modal: Option<ActiveModal>,
    dismiss_on_focus_lost: bool,
}

pub(crate) struct ModalOpenedEvent;

impl EventEmitter<ModalOpenedEvent> for ModalLayer {}

impl Default for ModalLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl ModalLayer {
    pub fn new() -> Self {
        Self {
            active_modal: None,
            dismiss_on_focus_lost: false,
        }
    }

    /// Toggles a modal of type `V`. If a modal of the same type is currently active,
    /// it will be hidden. If a different modal is active, it will be replaced with the new one.
    /// If no modal is active, the new modal will be shown.
    ///
    /// If closing the current modal fails (e.g., due to `on_before_dismiss` returning
    /// `DismissDecision::Dismiss(false)` or `DismissDecision::Pending`), the new modal
    /// will not be shown.
    pub fn toggle_modal<V, B>(&mut self, window: &mut Window, cx: &mut Context<Self>, build_view: B)
    where
        V: ModalView,
        B: FnOnce(&mut Window, &mut Context<V>) -> V,
    {
        if let Some(active_modal) = &self.active_modal {
            let should_close = active_modal.modal.view().downcast::<V>().is_ok();
            let did_close = self.hide_modal(window, cx);
            if should_close || !did_close {
                return;
            }
        }
        let new_modal = cx.new(|cx| build_view(window, cx));
        self.show_modal(new_modal, window, cx);
        cx.emit(ModalOpenedEvent);
    }

    /// Shows a modal and sets up subscriptions for dismiss events and focus tracking.
    /// The modal is automatically focused after being shown.
    fn show_modal<V>(&mut self, new_modal: Entity<V>, window: &mut Window, cx: &mut Context<Self>)
    where
        V: ModalView,
    {
        let focus_handle = cx.focus_handle();
        self.active_modal = Some(ActiveModal {
            modal: Box::new(new_modal.clone()),
            _subscriptions: [
                cx.subscribe_in(
                    &new_modal,
                    window,
                    |this, _, _: &DismissEvent, window, cx| {
                        this.hide_modal(window, cx);
                    },
                ),
                cx.on_focus_out(&focus_handle, window, |this, _event, window, cx| {
                    if this.dismiss_on_focus_lost {
                        this.hide_modal(window, cx);
                    }
                }),
            ],
            previous_focus_handle: window.focused(cx),
            focus_handle,
        });
        cx.defer_in(window, move |_, window, cx| {
            window.focus(&new_modal.focus_handle(cx), cx);
        });
        cx.notify();
    }

    /// Attempts to hide the currently active modal.
    ///
    /// The modal's `on_before_dismiss` method is called to determine if dismissal should proceed.
    /// If dismissal is allowed, the modal is removed and focus is restored to the previously
    /// focused element.
    ///
    /// Returns `true` if the modal was successfully hidden, `false` otherwise.
    pub fn hide_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(active_modal) = self.active_modal.as_mut() else {
            self.dismiss_on_focus_lost = false;
            return false;
        };

        match active_modal.modal.on_before_dismiss(window, cx) {
            DismissDecision::Dismiss(should_dismiss) => {
                if !should_dismiss {
                    self.dismiss_on_focus_lost = !should_dismiss;
                    return false;
                }
            }
            DismissDecision::Pending => {
                self.dismiss_on_focus_lost = false;
                return false;
            }
        }

        if let Some(active_modal) = self.active_modal.take() {
            // The layer's own `focus_handle` is only attached to an element in
            // the non-bare render path; a `render_bare` modal tracks its view's
            // own handle instead, so consult both — otherwise dismissing a bare
            // modal drops window focus entirely instead of restoring it.
            let modal_contained_focus = active_modal.focus_handle.contains_focused(window, cx)
                || active_modal
                    .modal
                    .view_focus_handle(cx)
                    .contains_focused(window, cx);
            if let Some(previous_focus) = active_modal.previous_focus_handle
                && modal_contained_focus
            {
                previous_focus.focus(window, cx);
            }
            cx.notify();
        }
        self.dismiss_on_focus_lost = false;
        true
    }

    /// Returns the currently active modal if it is of type `V`.
    pub fn active_modal<V>(&self) -> Option<Entity<V>>
    where
        V: 'static,
    {
        let active_modal = self.active_modal.as_ref()?;
        active_modal.modal.view().downcast::<V>().ok()
    }

    pub fn has_active_modal(&self) -> bool {
        self.active_modal.is_some()
    }
}

impl Render for ModalLayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(active_modal) = &self.active_modal else {
            return div().into_any_element();
        };

        if active_modal.modal.render_bare(cx) {
            return active_modal.modal.view().into_any_element();
        }

        div()
            .absolute()
            .size_full()
            .inset_0()
            .occlude()
            .when(active_modal.modal.fade_out_background(cx), |this| {
                let mut background = cx.theme().colors().elevated_surface_background;
                background.fade_out(0.2);
                this.bg(background)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.hide_modal(window, cx);
                }),
            )
            .child(
                v_flex()
                    .h(px(0.0))
                    .top_20()
                    .items_center()
                    .track_focus(&active_modal.focus_handle)
                    .child(
                        h_flex()
                            .occlude()
                            .child(active_modal.modal.view())
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            }),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Focusable, TestAppContext};

    struct BareModal(FocusHandle);

    impl EventEmitter<DismissEvent> for BareModal {}

    impl Focusable for BareModal {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.0.clone()
        }
    }

    impl ModalView for BareModal {
        fn render_bare(&self) -> bool {
            true
        }
    }

    impl Render for BareModal {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.0)
        }
    }

    struct TestRoot {
        modal_layer: Entity<ModalLayer>,
        pane_focus: FocusHandle,
    }

    impl Render for TestRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(div().track_focus(&self.pane_focus))
                .child(self.modal_layer.clone())
        }
    }

    #[gpui::test]
    async fn test_bare_modal_dismiss_restores_previous_focus(cx: &mut TestAppContext) {
        let (root, cx) = cx.add_window_view(|_, cx| TestRoot {
            modal_layer: cx.new(|_| ModalLayer::new()),
            pane_focus: cx.focus_handle(),
        });

        let (modal_layer, pane_focus) = root.read_with(cx, |root, _| {
            (root.modal_layer.clone(), root.pane_focus.clone())
        });

        cx.update(|window, cx| window.focus(&pane_focus, cx));
        cx.executor().run_until_parked();
        cx.update(|window, _| assert!(pane_focus.is_focused(window)));

        modal_layer.update_in(cx, |modal_layer, window, cx| {
            modal_layer.toggle_modal(window, cx, |_, cx| BareModal(cx.focus_handle()));
        });
        cx.executor().run_until_parked();

        let modal = modal_layer
            .read_with(cx, |modal_layer, _| modal_layer.active_modal::<BareModal>())
            .expect("bare modal is active");
        cx.update(|window, cx| {
            assert!(
                modal.focus_handle(cx).is_focused(window),
                "opening a bare modal moves focus onto it"
            );
        });

        // Dismiss the way escape does — the modal emits DismissEvent itself.
        modal.update(cx, |_, cx| cx.emit(DismissEvent));
        cx.executor().run_until_parked();

        cx.update(|window, cx| {
            assert!(
                window.focused(cx).is_some(),
                "dismissing a bare modal must not leave the window without focus"
            );
            assert!(
                pane_focus.is_focused(window),
                "dismissing a bare modal restores focus to the previously focused element"
            );
        });
    }
}
