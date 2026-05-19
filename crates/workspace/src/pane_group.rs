use crate::{
    AnyActiveCall, AppState, CollaboratorId, FollowerState, Pane, ParticipantLocation, Workspace,
    WorkspaceSettings,
    notifications::DetachAndPromptErr,
    pane_group::element::pane_axis,
    workspace_settings::{PaneSplitDirectionHorizontal, PaneSplitDirectionVertical},
};
use anyhow::Result;
use collections::HashMap;
use gpui::{
    Along, AnyView, AnyWeakView, Axis, Bounds, Entity, Hsla, IntoElement, MouseButton, Pixels,
    Point, StyleRefinement, WeakEntity, Window, point, size,
};
use parking_lot::Mutex;
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use settings::Settings;
use std::sync::Arc;
use ui::prelude::*;

pub const HANDLE_HITBOX_SIZE: f32 = 4.0;
const HORIZONTAL_MIN_SIZE: f32 = 80.;
const VERTICAL_MIN_SIZE: f32 = 100.;

/// One or many panes, arranged in a horizontal or vertical axis due to a split.
/// Panes have all their tabs and capabilities preserved, and can be split again or resized.
/// Single-pane group is a regular pane.
#[derive(Clone)]
pub struct PaneGroup {
    pub root: Member,
    pub is_center: bool,
}

pub struct PaneRenderResult {
    pub element: gpui::AnyElement,
    pub contains_active_pane: bool,
}

impl PaneGroup {
    pub fn with_root(root: Member) -> Self {
        Self {
            root,
            is_center: false,
        }
    }

    pub fn new(pane: Entity<Pane>) -> Self {
        Self {
            root: Member::Pane(pane),
            is_center: false,
        }
    }

    pub fn set_is_center(&mut self, is_center: bool) {
        self.is_center = is_center;
    }

    pub fn split(
        &mut self,
        old_pane: &Entity<Pane>,
        new_pane: &Entity<Pane>,
        direction: SplitDirection,
        cx: &mut App,
    ) {
        let found = match &mut self.root {
            Member::Pane(pane) => {
                if pane == old_pane {
                    self.root = Member::new_axis(old_pane.clone(), new_pane.clone(), direction);
                    true
                } else {
                    false
                }
            }
            Member::Axis(axis) => axis.split(old_pane, new_pane, direction),
            Member::Stack(stack) => {
                // The stack itself is a leaf; split the visible pane out
                // of the stack and replace the root with a new axis whose
                // two leaves are the surviving stack (if any) plus the
                // new pane.
                if stack.panes.iter().any(|pane| pane == old_pane) {
                    let surviving = collapse_stack_after_removing(stack, old_pane);
                    self.root = match surviving {
                        Some(surviving) => Member::Axis(PaneAxis::new(
                            direction.axis(),
                            if direction.increasing() {
                                vec![surviving, Member::Pane(new_pane.clone())]
                            } else {
                                vec![Member::Pane(new_pane.clone()), surviving]
                            },
                        )),
                        None => Member::Pane(new_pane.clone()),
                    };
                    true
                } else {
                    false
                }
            }
        };

        // If the pane wasn't found, fall back to splitting the first pane in the tree.
        if !found {
            let first_pane = self.root.first_pane();
            match &mut self.root {
                Member::Pane(_) => {
                    self.root = Member::new_axis(first_pane, new_pane.clone(), direction);
                }
                Member::Axis(axis) => {
                    let _ = axis.split(&first_pane, new_pane, direction);
                }
                Member::Stack(_) => {
                    self.root = Member::new_axis(first_pane, new_pane.clone(), direction);
                }
            }
        }

        self.mark_positions(cx);
    }

    pub fn bounding_box_for_pane(&self, pane: &Entity<Pane>) -> Option<Bounds<Pixels>> {
        match &self.root {
            Member::Pane(_) => None,
            Member::Stack(_) => None,
            Member::Axis(axis) => axis.bounding_box_for_pane(pane),
        }
    }

    pub fn full_height_column_count(&self) -> usize {
        self.root.full_height_column_count()
    }

    pub fn pane_at_pixel_position(&self, coordinate: Point<Pixels>) -> Option<&Entity<Pane>> {
        match &self.root {
            Member::Pane(pane) => Some(pane),
            // Until live-rendering of stacks ships
            // (`phase-19/stacked-panes-render`), the stack is presented
            // as a single slot whose visible member is the active pane.
            Member::Stack(stack) => stack.panes.get(stack.active),
            Member::Axis(axis) => axis.pane_at_pixel_position(coordinate),
        }
    }

    /// Moves active pane to span the entire border in the given direction,
    /// similar to Vim ctrl+w shift-[hjkl] motion.
    ///
    /// Returns:
    /// - Ok(true) if it found and moved a pane
    /// - Ok(false) if it found but did not move the pane
    /// - Err(_) if it did not find the pane
    pub fn move_to_border(
        &mut self,
        active_pane: &Entity<Pane>,
        direction: SplitDirection,
        cx: &mut App,
    ) -> Result<bool> {
        if let Some(pane) = self.find_pane_at_border(direction)
            && pane == active_pane
        {
            return Ok(false);
        }

        if !self.remove_internal(active_pane)? {
            return Ok(false);
        }

        if let Member::Axis(root) = &mut self.root
            && direction.axis() == root.axis
        {
            let idx = if direction.increasing() {
                root.members.len()
            } else {
                0
            };
            root.insert_pane(idx, active_pane);
            self.mark_positions(cx);
            return Ok(true);
        }

        let members = if direction.increasing() {
            vec![self.root.clone(), Member::Pane(active_pane.clone())]
        } else {
            vec![Member::Pane(active_pane.clone()), self.root.clone()]
        };
        self.root = Member::Axis(PaneAxis::new(direction.axis(), members));
        self.mark_positions(cx);
        Ok(true)
    }

    fn find_pane_at_border(&self, direction: SplitDirection) -> Option<&Entity<Pane>> {
        match &self.root {
            Member::Pane(pane) => Some(pane),
            Member::Stack(stack) => stack.panes.get(stack.active),
            Member::Axis(axis) => axis.find_pane_at_border(direction),
        }
    }

    /// Returns:
    /// - Ok(true) if it found and removed a pane
    /// - Ok(false) if it found but did not remove the pane
    /// - Err(_) if it did not find the pane
    pub fn remove(&mut self, pane: &Entity<Pane>, cx: &mut App) -> Result<bool> {
        let result = self.remove_internal(pane);
        if let Ok(true) = result {
            self.mark_positions(cx);
        }
        result
    }

    fn remove_internal(&mut self, pane: &Entity<Pane>) -> Result<bool> {
        match &mut self.root {
            Member::Pane(_) => Ok(false),
            Member::Stack(stack) => {
                if !stack.panes.iter().any(|p| p == pane) {
                    return Ok(false);
                }
                match collapse_stack_after_removing(stack, pane) {
                    Some(surviving) => {
                        self.root = surviving;
                        Ok(true)
                    }
                    // A stack always has >= 2 panes; removing one cannot
                    // produce an empty stack. Defensive arm to keep the
                    // root in a usable state if the invariant is ever
                    // violated.
                    None => Ok(false),
                }
            }
            Member::Axis(axis) => {
                if let Some(last_pane) = axis.remove(pane)? {
                    self.root = last_pane;
                }
                Ok(true)
            }
        }
    }

    pub fn resize(
        &mut self,
        pane: &Entity<Pane>,
        direction: Axis,
        amount: Pixels,
        bounds: &Bounds<Pixels>,
        cx: &mut App,
    ) {
        match &mut self.root {
            Member::Pane(_) => {}
            Member::Stack(_) => {}
            Member::Axis(axis) => {
                let _ = axis.resize(pane, direction, amount, bounds);
            }
        };
        self.mark_positions(cx);
    }

    pub fn reset_pane_sizes(&mut self, cx: &mut App) {
        match &mut self.root {
            Member::Pane(_) => {}
            Member::Stack(_) => {}
            Member::Axis(axis) => {
                let _ = axis.reset_pane_sizes();
            }
        };
        self.mark_positions(cx);
    }

    pub fn swap(&mut self, from: &Entity<Pane>, to: &Entity<Pane>, cx: &mut App) {
        match &mut self.root {
            Member::Pane(_) => {}
            Member::Stack(stack) => {
                for pane in stack.panes.iter_mut() {
                    if pane == from {
                        *pane = to.clone();
                    } else if pane == to {
                        *pane = from.clone();
                    }
                }
            }
            Member::Axis(axis) => axis.swap(from, to),
        };
        self.mark_positions(cx);
    }

    pub fn mark_positions(&mut self, cx: &mut App) {
        self.root.mark_positions(self.is_center, cx);
    }

    pub fn render(
        &self,
        zoomed: Option<&AnyWeakView>,
        render_cx: &dyn PaneLeaderDecorator,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        self.root.render(0, zoomed, render_cx, window, cx).element
    }

    pub fn panes(&self) -> Vec<&Entity<Pane>> {
        let mut panes = Vec::new();
        self.root.collect_panes(&mut panes);
        panes
    }

    pub fn first_pane(&self) -> Entity<Pane> {
        self.root.first_pane()
    }

    pub fn last_pane(&self) -> Entity<Pane> {
        self.root.last_pane()
    }

    pub fn find_pane_in_direction(
        &mut self,
        active_pane: &Entity<Pane>,
        direction: SplitDirection,
        cx: &App,
    ) -> Option<&Entity<Pane>> {
        let bounding_box = self.bounding_box_for_pane(active_pane)?;
        let cursor = active_pane.read(cx).pixel_position_of_cursor(cx);
        let center = match cursor {
            Some(cursor) if bounding_box.contains(&cursor) => cursor,
            _ => bounding_box.center(),
        };

        let distance_to_next = crate::HANDLE_HITBOX_SIZE;

        let target = match direction {
            SplitDirection::Left => {
                Point::new(bounding_box.left() - distance_to_next.into(), center.y)
            }
            SplitDirection::Right => {
                Point::new(bounding_box.right() + distance_to_next.into(), center.y)
            }
            SplitDirection::Up => {
                Point::new(center.x, bounding_box.top() - distance_to_next.into())
            }
            SplitDirection::Down => {
                Point::new(center.x, bounding_box.bottom() + distance_to_next.into())
            }
        };
        self.pane_at_pixel_position(target)
    }

    pub fn invert_axies(&mut self, cx: &mut App) {
        self.root.invert_pane_axies();
        self.mark_positions(cx);
    }
}

#[derive(Debug, Clone)]
pub enum Member {
    Axis(PaneAxis),
    Pane(Entity<Pane>),
    /// A stack of panes sharing the same slot. Only the pane at `active`
    /// is visible; the others are kept alive (subscriptions intact) for
    /// instant cycle. Invariant: `panes.len() >= 2` and `active < panes.len()`.
    /// Constructed via [`Member::new_stack`] which enforces both.
    Stack(PaneStack),
}

/// Container for a [`Member::Stack`] arm. Holds the ordered pane list plus
/// the visible index. Mirrors the structural shape of [`PaneAxis`] (a
/// `Vec<…>` + side-data) so codepaths that walk pane trees can treat it
/// uniformly.
#[derive(Debug, Clone)]
pub struct PaneStack {
    pub panes: Vec<Entity<Pane>>,
    pub active: usize,
}

impl PaneStack {
    /// Build a stack, normalising `active` into bounds. Empty / single-pane
    /// stacks are rejected by [`Member::new_stack`]; this constructor
    /// assumes the caller has already validated the `panes.len() >= 2`
    /// invariant.
    pub fn new(panes: Vec<Entity<Pane>>, active: usize) -> Self {
        let active = if panes.is_empty() {
            0
        } else {
            active.min(panes.len() - 1)
        };
        Self { panes, active }
    }
}

/// Remove `pane` from `stack` and return the surviving [`Member`]: a
/// reduced [`Member::Stack`] when 2+ panes remain, a [`Member::Pane`]
/// when exactly one remains, or `None` when the stack is now empty.
/// Used by codepaths (split / remove) that target a pane that happens to
/// live inside a stack — the stack collapses to satisfy the
/// `panes.len() >= 2` invariant.
fn collapse_stack_after_removing(stack: &PaneStack, pane: &Entity<Pane>) -> Option<Member> {
    let mut surviving: Vec<Entity<Pane>> = stack
        .panes
        .iter()
        .filter(|p| *p != pane)
        .cloned()
        .collect();
    match surviving.len() {
        0 => None,
        1 => Some(Member::Pane(surviving.remove(0))),
        _ => {
            let active = stack.active.min(surviving.len() - 1);
            Some(Member::Stack(PaneStack::new(surviving, active)))
        }
    }
}

impl Member {
    /// Construct a stacked member from `panes`. Returns
    /// [`Member::Pane`] when the stack would have a single member (the
    /// `Stack` invariant is `panes.len() >= 2`); returns `None` when the
    /// input is empty. `active` is clamped into bounds.
    pub fn new_stack(panes: Vec<Entity<Pane>>, active: usize) -> Option<Self> {
        match panes.len() {
            0 => None,
            1 => {
                let mut panes = panes;
                Some(Member::Pane(panes.remove(0)))
            }
            _ => Some(Member::Stack(PaneStack::new(panes, active))),
        }
    }

    /// Return the visible pane for a leaf-like member. Panics on
    /// [`Member::Axis`] — sites that walk an axis must descend explicitly;
    /// this helper exists for match arms that genuinely don't care
    /// whether the leaf is a [`Member::Pane`] or a [`Member::Stack`]
    /// (e.g. "which pane should we focus right now?").
    pub fn active_pane(&self) -> &Entity<Pane> {
        match self {
            Member::Pane(pane) => pane,
            Member::Stack(stack) => &stack.panes[stack.active],
            Member::Axis(_) => panic!(
                "Member::active_pane called on Axis; callers must descend the axis explicitly"
            ),
        }
    }

    pub fn mark_positions(&mut self, in_center_group: bool, cx: &mut App) {
        match self {
            Member::Axis(pane_axis) => {
                for member in pane_axis.members.iter_mut() {
                    member.mark_positions(in_center_group, cx);
                }
            }
            Member::Pane(entity) => entity.update(cx, |pane, _| {
                pane.in_center_group = in_center_group;
            }),
            Member::Stack(stack) => {
                for pane in stack.panes.iter() {
                    pane.update(cx, |pane, _| {
                        pane.in_center_group = in_center_group;
                    });
                }
            }
        }
    }

    fn full_height_column_count(&self) -> usize {
        match self {
            Member::Pane(_) => 1,
            Member::Stack(_) => 1,
            Member::Axis(axis) => axis.full_height_column_count(),
        }
    }
}

#[derive(Clone, Copy)]
pub struct PaneRenderContext<'a> {
    pub project: &'a Entity<Project>,
    pub follower_states: &'a HashMap<CollaboratorId, FollowerState>,
    pub active_call: Option<&'a dyn AnyActiveCall>,
    pub active_pane: &'a Entity<Pane>,
    pub app_state: &'a Arc<AppState>,
    pub workspace: &'a WeakEntity<Workspace>,
}

#[derive(Default)]
pub struct LeaderDecoration {
    border: Option<Hsla>,
    status_box: Option<AnyElement>,
}

pub trait PaneLeaderDecorator {
    fn decorate(&self, pane: &Entity<Pane>, cx: &App) -> LeaderDecoration;
    fn active_pane(&self) -> &Entity<Pane>;
    fn workspace(&self) -> &WeakEntity<Workspace>;
}

pub struct ActivePaneDecorator<'a> {
    active_pane: &'a Entity<Pane>,
    workspace: &'a WeakEntity<Workspace>,
}

impl<'a> ActivePaneDecorator<'a> {
    pub fn new(active_pane: &'a Entity<Pane>, workspace: &'a WeakEntity<Workspace>) -> Self {
        Self {
            active_pane,
            workspace,
        }
    }
}

impl PaneLeaderDecorator for ActivePaneDecorator<'_> {
    fn decorate(&self, _: &Entity<Pane>, _: &App) -> LeaderDecoration {
        LeaderDecoration::default()
    }
    fn active_pane(&self) -> &Entity<Pane> {
        self.active_pane
    }

    fn workspace(&self) -> &WeakEntity<Workspace> {
        self.workspace
    }
}

impl PaneLeaderDecorator for PaneRenderContext<'_> {
    fn decorate(&self, pane: &Entity<Pane>, cx: &App) -> LeaderDecoration {
        let follower_state = self.follower_states.iter().find_map(|(leader_id, state)| {
            if state.center_pane == *pane {
                Some((*leader_id, state))
            } else {
                None
            }
        });
        let Some((leader_id, follower_state)) = follower_state else {
            return LeaderDecoration::default();
        };

        let mut leader_color;
        let status_box;
        match leader_id {
            CollaboratorId::PeerId(peer_id) => {
                let Some(leader) = self
                    .active_call
                    .as_ref()
                    .and_then(|call| call.remote_participant_for_peer_id(peer_id, cx))
                else {
                    return LeaderDecoration::default();
                };

                let is_in_unshared_view = follower_state.active_view_id.is_some_and(|view_id| {
                    !follower_state
                        .items_by_leader_view_id
                        .contains_key(&view_id)
                });

                let mut leader_join_data = None;
                let leader_status_box = match leader.location {
                    ParticipantLocation::SharedProject {
                        project_id: leader_project_id,
                    } => {
                        if Some(leader_project_id) == self.project.read(cx).remote_id() {
                            is_in_unshared_view.then(|| {
                                Label::new(format!(
                                    "{} is in an unshared pane",
                                    leader.user.github_login
                                ))
                            })
                        } else {
                            leader_join_data = Some((leader_project_id, leader.user.id));
                            Some(Label::new(format!(
                                "Follow {} to their active project",
                                leader.user.github_login,
                            )))
                        }
                    }
                    ParticipantLocation::UnsharedProject => Some(Label::new(format!(
                        "{} is viewing an unshared Zed project",
                        leader.user.github_login
                    ))),
                    ParticipantLocation::External => Some(Label::new(format!(
                        "{} is viewing a window outside of Zed",
                        leader.user.github_login
                    ))),
                };
                status_box = leader_status_box.map(|status| {
                    div()
                        .absolute()
                        .w_96()
                        .bottom_3()
                        .right_3()
                        .elevation_2(cx)
                        .p_1()
                        .child(status)
                        .when_some(
                            leader_join_data,
                            |this, (leader_project_id, leader_user_id)| {
                                let app_state = self.app_state.clone();
                                this.cursor_pointer().on_mouse_down(
                                    MouseButton::Left,
                                    move |_, window, cx| {
                                        crate::join_in_room_project(
                                            leader_project_id,
                                            leader_user_id,
                                            app_state.clone(),
                                            cx,
                                        )
                                        .detach_and_prompt_err(
                                            "Failed to join project",
                                            window,
                                            cx,
                                            |error, _, _| Some(format!("{error:#}")),
                                        );
                                    },
                                )
                            },
                        )
                        .into_any_element()
                });
                leader_color = cx
                    .theme()
                    .players()
                    .color_for_participant(leader.participant_index.0)
                    .cursor;
            }
            CollaboratorId::Agent => {
                status_box = None;
                leader_color = cx.theme().players().agent().cursor;
            }
        }

        let is_in_panel = follower_state.dock_pane.is_some();
        if is_in_panel {
            leader_color.fade_out(0.75);
        } else {
            leader_color.fade_out(0.3);
        }

        LeaderDecoration {
            status_box,
            border: Some(leader_color),
        }
    }

    fn active_pane(&self) -> &Entity<Pane> {
        self.active_pane
    }

    fn workspace(&self) -> &WeakEntity<Workspace> {
        self.workspace
    }
}

impl Member {
    fn new_axis(old_pane: Entity<Pane>, new_pane: Entity<Pane>, direction: SplitDirection) -> Self {
        use Axis::*;
        use SplitDirection::*;

        let axis = match direction {
            Up | Down => Vertical,
            Left | Right => Horizontal,
        };

        let members = match direction {
            Up | Left => vec![Member::Pane(new_pane), Member::Pane(old_pane)],
            Down | Right => vec![Member::Pane(old_pane), Member::Pane(new_pane)],
        };

        Member::Axis(PaneAxis::new(axis, members))
    }

    fn first_pane(&self) -> Entity<Pane> {
        match self {
            Member::Axis(axis) => axis.members[0].first_pane(),
            Member::Pane(pane) => pane.clone(),
            Member::Stack(stack) => stack.panes[stack.active].clone(),
        }
    }

    fn last_pane(&self) -> Entity<Pane> {
        match self {
            Member::Axis(axis) => axis.members.last().unwrap().last_pane(),
            Member::Pane(pane) => pane.clone(),
            Member::Stack(stack) => stack.panes[stack.active].clone(),
        }
    }

    pub fn render(
        &self,
        basis: usize,
        zoomed: Option<&AnyWeakView>,
        render_cx: &dyn PaneLeaderDecorator,
        window: &mut Window,
        cx: &mut App,
    ) -> PaneRenderResult {
        match self {
            Member::Pane(pane) => {
                if zoomed == Some(&pane.downgrade().into()) {
                    return PaneRenderResult {
                        element: div().into_any(),
                        contains_active_pane: false,
                    };
                }

                let decoration = render_cx.decorate(pane, cx);
                let is_active = pane == render_cx.active_pane();

                PaneRenderResult {
                    element: div()
                        .relative()
                        .flex_1()
                        .size_full()
                        .child(
                            AnyView::from(pane.clone())
                                .cached(StyleRefinement::default().v_flex().size_full()),
                        )
                        .when_some(decoration.border, |this, color| {
                            this.child(
                                div()
                                    .absolute()
                                    .size_full()
                                    .left_0()
                                    .top_0()
                                    .border_2()
                                    .border_color(color),
                            )
                        })
                        .children(decoration.status_box)
                        .into_any(),
                    contains_active_pane: is_active,
                }
            }
            Member::Axis(axis) => axis.render(basis + 1, zoomed, render_cx, window, cx),
            // Foundation only: render the visible member without the
            // tab strip. The strip + click-to-switch ships in
            // `TASK:phase-19/stacked-panes-render`; until then the
            // stack behaves visually like the active pane on its own.
            Member::Stack(stack) => {
                let active = match stack.panes.get(stack.active) {
                    Some(pane) => pane,
                    None => return PaneRenderResult {
                        element: div().into_any(),
                        contains_active_pane: false,
                    },
                };
                Member::Pane(active.clone()).render(basis + 1, zoomed, render_cx, window, cx)
            }
        }
    }

    fn collect_panes<'a>(&'a self, panes: &mut Vec<&'a Entity<Pane>>) {
        match self {
            Member::Axis(axis) => {
                for member in &axis.members {
                    member.collect_panes(panes);
                }
            }
            Member::Pane(pane) => panes.push(pane),
            // Inactive members must still appear in `Workspace::panes`
            // so their event subscriptions stay live and the cycle
            // verb can switch to them in O(1).
            Member::Stack(stack) => {
                for pane in &stack.panes {
                    panes.push(pane);
                }
            }
        }
    }

    fn invert_pane_axies(&mut self) {
        match self {
            Self::Axis(axis) => {
                axis.axis = axis.axis.invert();
                for member in axis.members.iter_mut() {
                    member.invert_pane_axies();
                }
            }
            Self::Pane(_) => {}
            Self::Stack(_) => {}
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaneAxis {
    pub axis: Axis,
    pub members: Vec<Member>,
    pub flexes: Arc<Mutex<Vec<f32>>>,
    pub bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
}

impl PaneAxis {
    pub fn new(axis: Axis, members: Vec<Member>) -> Self {
        let flexes = Arc::new(Mutex::new(vec![1.; members.len()]));
        let bounding_boxes = Arc::new(Mutex::new(vec![None; members.len()]));
        Self {
            axis,
            members,
            flexes,
            bounding_boxes,
        }
    }

    pub fn load(axis: Axis, members: Vec<Member>, flexes: Option<Vec<f32>>) -> Self {
        let mut flexes = flexes.unwrap_or_else(|| vec![1.; members.len()]);
        if flexes.len() != members.len()
            || (flexes.iter().copied().sum::<f32>() - flexes.len() as f32).abs() >= 0.001
        {
            flexes = vec![1.; members.len()];
        }

        let flexes = Arc::new(Mutex::new(flexes));
        let bounding_boxes = Arc::new(Mutex::new(vec![None; members.len()]));
        Self {
            axis,
            members,
            flexes,
            bounding_boxes,
        }
    }

    fn split(
        &mut self,
        old_pane: &Entity<Pane>,
        new_pane: &Entity<Pane>,
        direction: SplitDirection,
    ) -> bool {
        for (mut idx, member) in self.members.iter_mut().enumerate() {
            match member {
                Member::Axis(axis) => {
                    if axis.split(old_pane, new_pane, direction) {
                        return true;
                    }
                }
                Member::Pane(pane) => {
                    if pane == old_pane {
                        if direction.axis() == self.axis {
                            if direction.increasing() {
                                idx += 1;
                            }
                            self.insert_pane(idx, new_pane);
                        } else {
                            *member =
                                Member::new_axis(old_pane.clone(), new_pane.clone(), direction);
                        }
                        return true;
                    }
                }
                Member::Stack(stack) => {
                    if stack.panes.iter().any(|p| p == old_pane) {
                        let surviving = collapse_stack_after_removing(stack, old_pane);
                        let new_leaf = Member::Pane(new_pane.clone());
                        match surviving {
                            Some(surviving) => {
                                if direction.axis() == self.axis {
                                    *member = surviving;
                                    if direction.increasing() {
                                        idx += 1;
                                    }
                                    self.insert_pane(idx, new_pane);
                                } else {
                                    let pair = if direction.increasing() {
                                        vec![surviving, new_leaf]
                                    } else {
                                        vec![new_leaf, surviving]
                                    };
                                    *member = Member::Axis(PaneAxis::new(direction.axis(), pair));
                                }
                            }
                            // Stack invariant guarantees `surviving` is
                            // `Some`; defensive fallback only.
                            None => {
                                *member = new_leaf;
                            }
                        }
                        return true;
                    }
                }
            }
        }
        false
    }

    fn insert_pane(&mut self, idx: usize, new_pane: &Entity<Pane>) {
        self.members.insert(idx, Member::Pane(new_pane.clone()));
        *self.flexes.lock() = vec![1.; self.members.len()];
    }

    fn find_pane_at_border(&self, direction: SplitDirection) -> Option<&Entity<Pane>> {
        if self.axis != direction.axis() {
            return None;
        }
        let member = if direction.increasing() {
            self.members.last()
        } else {
            self.members.first()
        };
        member.and_then(|e| match e {
            Member::Pane(pane) => Some(pane),
            Member::Stack(stack) => stack.panes.get(stack.active),
            Member::Axis(_) => None,
        })
    }

    fn remove(&mut self, pane_to_remove: &Entity<Pane>) -> Result<Option<Member>> {
        let mut found_pane = false;
        let mut remove_member = None;
        for (idx, member) in self.members.iter_mut().enumerate() {
            match member {
                Member::Axis(axis) => {
                    if let Ok(last_pane) = axis.remove(pane_to_remove) {
                        if let Some(last_pane) = last_pane {
                            *member = last_pane;
                        }
                        found_pane = true;
                        break;
                    }
                }
                Member::Pane(pane) => {
                    if pane == pane_to_remove {
                        found_pane = true;
                        remove_member = Some(idx);
                        break;
                    }
                }
                Member::Stack(stack) => {
                    if stack.panes.iter().any(|p| p == pane_to_remove) {
                        match collapse_stack_after_removing(stack, pane_to_remove) {
                            Some(surviving) => {
                                *member = surviving;
                                found_pane = true;
                            }
                            // Stack invariant guarantees a surviving
                            // member; if it ever does not, drop the
                            // slot entirely.
                            None => {
                                found_pane = true;
                                remove_member = Some(idx);
                            }
                        }
                        break;
                    }
                }
            }
        }

        if found_pane {
            if let Some(idx) = remove_member {
                self.members.remove(idx);
                *self.flexes.lock() = vec![1.; self.members.len()];
            }

            if self.members.len() == 1 {
                let result = self.members.pop();
                *self.flexes.lock() = vec![1.; self.members.len()];
                Ok(result)
            } else {
                Ok(None)
            }
        } else {
            anyhow::bail!("Pane not found");
        }
    }

    fn reset_pane_sizes(&self) {
        *self.flexes.lock() = vec![1.; self.members.len()];
        for member in self.members.iter() {
            match member {
                Member::Axis(axis) => axis.reset_pane_sizes(),
                Member::Pane(_) => {}
                Member::Stack(_) => {}
            }
        }
    }

    fn resize(
        &mut self,
        pane: &Entity<Pane>,
        axis: Axis,
        amount: Pixels,
        bounds: &Bounds<Pixels>,
    ) -> Option<bool> {
        let container_size = self
            .bounding_boxes
            .lock()
            .iter()
            .filter_map(|e| *e)
            .reduce(|acc, e| acc.union(&e))
            .unwrap_or(*bounds)
            .size;

        let found_pane = self.members.iter().any(|member| match member {
            Member::Pane(p) => p == pane,
            // The stack occupies a leaf slot; resizing targets that
            // slot when the dragged pane is the stack's visible member.
            Member::Stack(stack) => stack.panes.get(stack.active) == Some(pane),
            Member::Axis(_) => false,
        });

        if found_pane && self.axis != axis {
            return Some(false); // pane found but this is not the correct axis direction
        }
        let mut found_axis_index: Option<usize> = None;
        if !found_pane {
            for (i, pa) in self.members.iter_mut().enumerate() {
                if let Member::Axis(pa) = pa
                    && let Some(done) = pa.resize(pane, axis, amount, bounds)
                {
                    if done {
                        return Some(true); // pane found and operations already done
                    } else if self.axis != axis {
                        return Some(false); // pane found but this is not the correct axis direction
                    } else {
                        found_axis_index = Some(i); // pane found and this is correct direction
                    }
                }
            }
            found_axis_index?; // no pane found
        }

        let min_size = match axis {
            Axis::Horizontal => px(HORIZONTAL_MIN_SIZE),
            Axis::Vertical => px(VERTICAL_MIN_SIZE),
        };
        let mut flexes = self.flexes.lock();

        let ix = if found_pane {
            self.members.iter().position(|m| match m {
                Member::Pane(p) => p == pane,
                Member::Stack(stack) => stack.panes.get(stack.active) == Some(pane),
                Member::Axis(_) => false,
            })
        } else {
            found_axis_index
        };

        if ix.is_none() {
            return Some(true);
        }

        let ix = ix.unwrap_or(0);

        let size = move |ix, flexes: &[f32]| {
            container_size.along(axis) * (flexes[ix] / flexes.len() as f32)
        };

        // Don't allow resizing to less than the minimum size, if elements are already too small
        if min_size - px(1.) > size(ix, flexes.as_slice()) {
            return Some(true);
        }

        let flex_changes = |pixel_dx, target_ix, next: isize, flexes: &[f32]| {
            let flex_change = flexes.len() as f32 * pixel_dx / container_size.along(axis);
            let current_target_flex = flexes[target_ix] + flex_change;
            let next_target_flex = flexes[(target_ix as isize + next) as usize] - flex_change;
            (current_target_flex, next_target_flex)
        };

        let apply_changes =
            |current_ix: usize, proposed_current_pixel_change: Pixels, flexes: &mut [f32]| {
                let next_target_size = Pixels::max(
                    size(current_ix + 1, flexes) - proposed_current_pixel_change,
                    min_size,
                );
                let current_target_size = Pixels::max(
                    size(current_ix, flexes) + size(current_ix + 1, flexes) - next_target_size,
                    min_size,
                );

                let current_pixel_change = current_target_size - size(current_ix, flexes);

                let (current_target_flex, next_target_flex) =
                    flex_changes(current_pixel_change, current_ix, 1, flexes);

                flexes[current_ix] = current_target_flex;
                flexes[current_ix + 1] = next_target_flex;
            };

        if ix + 1 == flexes.len() {
            apply_changes(ix - 1, -1.0 * amount, flexes.as_mut_slice());
        } else {
            apply_changes(ix, amount, flexes.as_mut_slice());
        }
        Some(true)
    }

    fn swap(&mut self, from: &Entity<Pane>, to: &Entity<Pane>) {
        for member in self.members.iter_mut() {
            match member {
                Member::Axis(axis) => axis.swap(from, to),
                Member::Pane(pane) => {
                    if pane == from {
                        *member = Member::Pane(to.clone());
                    } else if pane == to {
                        *member = Member::Pane(from.clone())
                    }
                }
                Member::Stack(stack) => {
                    for pane in stack.panes.iter_mut() {
                        if pane == from {
                            *pane = to.clone();
                        } else if pane == to {
                            *pane = from.clone();
                        }
                    }
                }
            }
        }
    }

    fn bounding_box_for_pane(&self, pane: &Entity<Pane>) -> Option<Bounds<Pixels>> {
        debug_assert!(self.members.len() == self.bounding_boxes.lock().len());

        for (idx, member) in self.members.iter().enumerate() {
            match member {
                Member::Pane(found) => {
                    if pane == found {
                        return self.bounding_boxes.lock()[idx];
                    }
                }
                Member::Stack(stack) => {
                    if stack.panes.get(stack.active) == Some(pane) {
                        return self.bounding_boxes.lock()[idx];
                    }
                }
                Member::Axis(axis) => {
                    if let Some(rect) = axis.bounding_box_for_pane(pane) {
                        return Some(rect);
                    }
                }
            }
        }
        None
    }

    fn pane_at_pixel_position(&self, coordinate: Point<Pixels>) -> Option<&Entity<Pane>> {
        debug_assert!(self.members.len() == self.bounding_boxes.lock().len());

        let bounding_boxes = self.bounding_boxes.lock();

        for (idx, member) in self.members.iter().enumerate() {
            if let Some(coordinates) = bounding_boxes[idx]
                && coordinates.contains(&coordinate)
            {
                return match member {
                    Member::Pane(found) => Some(found),
                    Member::Stack(stack) => stack.panes.get(stack.active),
                    Member::Axis(axis) => axis.pane_at_pixel_position(coordinate),
                };
            }
        }
        None
    }

    fn full_height_column_count(&self) -> usize {
        match self.axis {
            Axis::Horizontal => self
                .members
                .iter()
                .map(Member::full_height_column_count)
                .sum::<usize>()
                .max(1),
            Axis::Vertical => self
                .members
                .iter()
                .map(Member::full_height_column_count)
                .max()
                .unwrap_or(1),
        }
    }

    fn render(
        &self,
        basis: usize,
        zoomed: Option<&AnyWeakView>,
        render_cx: &dyn PaneLeaderDecorator,
        window: &mut Window,
        cx: &mut App,
    ) -> PaneRenderResult {
        debug_assert!(self.members.len() == self.flexes.lock().len());
        let mut active_pane_ix = None;
        let mut active_subtree_ix = None;
        let mut contains_active_pane = false;
        let mut is_leaf_pane = vec![false; self.members.len()];

        let rendered_children = self
            .members
            .iter()
            .enumerate()
            .map(|(ix, member)| {
                match member {
                    Member::Pane(pane) => {
                        is_leaf_pane[ix] = true;
                        if pane == render_cx.active_pane() {
                            active_pane_ix = Some(ix);
                            active_subtree_ix = Some(ix);
                            contains_active_pane = true;
                        }
                    }
                    Member::Stack(stack) => {
                        // A stack is a leaf slot at this axis level —
                        // it occupies one cell whose visible content is
                        // the active stack member.
                        is_leaf_pane[ix] = true;
                        if stack.panes.get(stack.active) == Some(render_cx.active_pane()) {
                            active_pane_ix = Some(ix);
                            active_subtree_ix = Some(ix);
                            contains_active_pane = true;
                        }
                    }
                    Member::Axis(_) => {
                        is_leaf_pane[ix] = false;
                    }
                }

                let result = member.render((basis + ix) * 10, zoomed, render_cx, window, cx);
                if result.contains_active_pane {
                    contains_active_pane = true;
                    // First child whose subtree contains the active pane
                    // wins. (No axis branches both ways — the active pane
                    // sits in exactly one subtree.)
                    active_subtree_ix.get_or_insert(ix);
                }
                result.element.into_any_element()
            })
            .collect::<Vec<_>>();

        let element = pane_axis(
            self.axis,
            basis,
            self.flexes.clone(),
            self.bounding_boxes.clone(),
            render_cx.workspace().clone(),
        )
        .with_is_leaf_pane_mask(is_leaf_pane)
        .children(rendered_children)
        .with_active_pane(active_pane_ix)
        .with_active_subtree(active_subtree_ix, render_cx.active_pane().clone())
        .into_any_element();

        PaneRenderResult {
            element,
            contains_active_pane,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Up,
    Down,
    Left,
    Right,
}

impl std::fmt::Display for SplitDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SplitDirection::Up => write!(f, "up"),
            SplitDirection::Down => write!(f, "down"),
            SplitDirection::Left => write!(f, "left"),
            SplitDirection::Right => write!(f, "right"),
        }
    }
}

impl SplitDirection {
    pub fn all() -> [Self; 4] {
        [Self::Up, Self::Down, Self::Left, Self::Right]
    }

    pub fn vertical(cx: &mut App) -> Self {
        match WorkspaceSettings::get_global(cx).pane_split_direction_vertical {
            PaneSplitDirectionVertical::Left => SplitDirection::Left,
            PaneSplitDirectionVertical::Right => SplitDirection::Right,
        }
    }

    pub fn horizontal(cx: &mut App) -> Self {
        match WorkspaceSettings::get_global(cx).pane_split_direction_horizontal {
            PaneSplitDirectionHorizontal::Down => SplitDirection::Down,
            PaneSplitDirectionHorizontal::Up => SplitDirection::Up,
        }
    }

    pub fn edge(&self, rect: Bounds<Pixels>) -> Pixels {
        match self {
            Self::Up => rect.origin.y,
            Self::Down => rect.bottom_left().y,
            Self::Left => rect.bottom_left().x,
            Self::Right => rect.bottom_right().x,
        }
    }

    pub fn along_edge(&self, bounds: Bounds<Pixels>, length: Pixels) -> Bounds<Pixels> {
        match self {
            Self::Up => Bounds {
                origin: bounds.origin,
                size: size(bounds.size.width, length),
            },
            Self::Down => Bounds {
                origin: point(bounds.bottom_left().x, bounds.bottom_left().y - length),
                size: size(bounds.size.width, length),
            },
            Self::Left => Bounds {
                origin: bounds.origin,
                size: size(length, bounds.size.height),
            },
            Self::Right => Bounds {
                origin: point(bounds.bottom_right().x - length, bounds.bottom_left().y),
                size: size(length, bounds.size.height),
            },
        }
    }

    pub fn axis(&self) -> Axis {
        match self {
            Self::Up | Self::Down => Axis::Vertical,
            Self::Left | Self::Right => Axis::Horizontal,
        }
    }

    pub fn increasing(&self) -> bool {
        match self {
            Self::Left | Self::Up => false,
            Self::Down | Self::Right => true,
        }
    }

    pub fn opposite(&self) -> SplitDirection {
        match self {
            Self::Down => Self::Up,
            Self::Up => Self::Down,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

mod element {
    use std::mem;
    use std::{cell::RefCell, iter, rc::Rc, sync::Arc};

    use gpui::{
        Along, AnyElement, App, Axis, BorderStyle, Bounds, Element, Entity, GlobalElementId,
        HitboxBehavior, IntoElement, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
        Pixels, Point, Size, Style, WeakEntity, Window, px, relative, size,
    };
    use gpui::{CursorStyle, Hitbox};
    use parking_lot::Mutex;
    use settings::Settings;
    use smallvec::SmallVec;
    use ui::prelude::*;
    use util::ResultExt;

    use crate::{Pane, Workspace};

    use crate::WorkspaceSettings;

    use super::{HANDLE_HITBOX_SIZE, HORIZONTAL_MIN_SIZE, VERTICAL_MIN_SIZE};

    const DIVIDER_SIZE: f32 = 1.0;

    pub(super) fn pane_axis(
        axis: Axis,
        basis: usize,
        flexes: Arc<Mutex<Vec<f32>>>,
        bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
        workspace: WeakEntity<Workspace>,
    ) -> PaneAxisElement {
        PaneAxisElement {
            axis,
            basis,
            flexes,
            bounding_boxes,
            children: SmallVec::new(),
            active_pane_ix: None,
            active_subtree_ix: None,
            active_pane: None,
            workspace,
            is_leaf_pane_mask: Vec::new(),
        }
    }

    pub struct PaneAxisElement {
        axis: Axis,
        basis: usize,
        /// Equivalent to ColumnWidths (but in terms of flexes instead of percentages)
        /// For example, flexes "1.33, 1, 1", instead of "40%, 30%, 30%"
        flexes: Arc<Mutex<Vec<f32>>>,
        bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
        children: SmallVec<[AnyElement; 2]>,
        /// Set only when the active pane is a *direct* leaf child of this
        /// axis. Used by `inactive_opacity` / `border_size` overlays and by
        /// the unconditional full-extent divider highlight.
        active_pane_ix: Option<usize>,
        /// Set when the active pane is anywhere inside the subtree at this
        /// child index (including nested deeper). Used to clip the divider
        /// highlight at outer axes to the active pane's actual extent.
        active_subtree_ix: Option<usize>,
        /// Handle to the active pane; used at paint time to look up its
        /// screen-coordinate bounds for clipping outer dividers.
        active_pane: Option<Entity<Pane>>,
        workspace: WeakEntity<Workspace>,
        // Track which children are leaf panes (Member::Pane) vs axes (Member::Axis)
        is_leaf_pane_mask: Vec<bool>,
    }

    pub struct PaneAxisLayout {
        dragged_handle: Rc<RefCell<Option<usize>>>,
        children: Vec<PaneAxisChildLayout>,
    }

    struct PaneAxisChildLayout {
        bounds: Bounds<Pixels>,
        element: AnyElement,
        handle: Option<PaneAxisHandleLayout>,
        is_leaf_pane: bool,
    }

    struct PaneAxisHandleLayout {
        hitbox: Hitbox,
        divider_bounds: Bounds<Pixels>,
    }

    impl PaneAxisElement {
        pub fn with_active_pane(mut self, active_pane_ix: Option<usize>) -> Self {
            self.active_pane_ix = active_pane_ix;
            self
        }

        pub fn with_active_subtree(
            mut self,
            subtree_ix: Option<usize>,
            active_pane: Entity<Pane>,
        ) -> Self {
            self.active_subtree_ix = subtree_ix;
            self.active_pane = Some(active_pane);
            self
        }

        pub fn with_is_leaf_pane_mask(mut self, mask: Vec<bool>) -> Self {
            self.is_leaf_pane_mask = mask;
            self
        }

        fn compute_resize(
            flexes: &Arc<Mutex<Vec<f32>>>,
            e: &MouseMoveEvent,
            ix: usize,
            axis: Axis,
            child_start: Point<Pixels>,
            container_size: Size<Pixels>,
            workspace: WeakEntity<Workspace>,
            window: &mut Window,
            cx: &mut App,
        ) {
            let min_size = match axis {
                Axis::Horizontal => px(HORIZONTAL_MIN_SIZE),
                Axis::Vertical => px(VERTICAL_MIN_SIZE),
            };
            let mut flexes = flexes.lock();
            debug_assert!(flex_values_in_bounds(flexes.as_slice()));

            // Math to convert a flex value to a pixel value
            let size = move |ix, flexes: &[f32]| {
                container_size.along(axis) * (flexes[ix] / flexes.len() as f32)
            };

            // Don't allow resizing to less than the minimum size, if elements are already too small
            if min_size - px(1.) > size(ix, flexes.as_slice()) {
                return;
            }

            // This is basically a "bucket" of pixel changes that need to be applied in response to this
            // mouse event. Probably a small, fractional number like 0.5 or 1.5 pixels
            let mut proposed_current_pixel_change =
                (e.position - child_start).along(axis) - size(ix, flexes.as_slice());

            // This takes a pixel change, and computes the flex changes that correspond to this pixel change
            // as well as the next one, for some reason
            let flex_changes = |pixel_dx, target_ix, next: isize, flexes: &[f32]| {
                let flex_change = pixel_dx / container_size.along(axis);
                let current_target_flex = flexes[target_ix] + flex_change;
                let next_target_flex = flexes[(target_ix as isize + next) as usize] - flex_change;
                (current_target_flex, next_target_flex)
            };

            // Generate the list of flex successors, from the current index.
            // If you're dragging column 3 forward, out of 6 columns, then this code will produce [4, 5, 6]
            // If you're dragging column 3 backward, out of 6 columns, then this code will produce [2, 1, 0]
            let mut successors = iter::from_fn({
                let forward = proposed_current_pixel_change > px(0.);
                let mut ix_offset = 0;
                let len = flexes.len();
                move || {
                    let result = if forward {
                        (ix + 1 + ix_offset < len).then(|| ix + ix_offset)
                    } else {
                        (ix as isize - ix_offset as isize >= 0).then(|| ix - ix_offset)
                    };

                    ix_offset += 1;

                    result
                }
            });

            // Now actually loop over these, and empty our bucket of pixel changes
            while proposed_current_pixel_change.abs() > px(0.) {
                let Some(current_ix) = successors.next() else {
                    break;
                };

                let next_target_size = Pixels::max(
                    size(current_ix + 1, flexes.as_slice()) - proposed_current_pixel_change,
                    min_size,
                );

                let current_target_size = Pixels::max(
                    size(current_ix, flexes.as_slice()) + size(current_ix + 1, flexes.as_slice())
                        - next_target_size,
                    min_size,
                );

                let current_pixel_change =
                    current_target_size - size(current_ix, flexes.as_slice());

                let (current_target_flex, next_target_flex) =
                    flex_changes(current_pixel_change, current_ix, 1, flexes.as_slice());

                flexes[current_ix] = current_target_flex;
                flexes[current_ix + 1] = next_target_flex;

                proposed_current_pixel_change -= current_pixel_change;
            }

            workspace
                .update(cx, |this, cx| this.serialize_workspace(window, cx))
                .log_err();
            cx.stop_propagation();
            window.refresh();
        }

        fn layout_handle(
            axis: Axis,
            pane_bounds: Bounds<Pixels>,
            window: &mut Window,
            _cx: &mut App,
        ) -> PaneAxisHandleLayout {
            let handle_bounds = Bounds {
                origin: pane_bounds.origin.apply_along(axis, |origin| {
                    origin + pane_bounds.size.along(axis) - px(HANDLE_HITBOX_SIZE / 2.)
                }),
                size: pane_bounds
                    .size
                    .apply_along(axis, |_| px(HANDLE_HITBOX_SIZE)),
            };
            let divider_bounds = Bounds {
                origin: pane_bounds
                    .origin
                    .apply_along(axis, |origin| origin + pane_bounds.size.along(axis)),
                size: pane_bounds.size.apply_along(axis, |_| px(DIVIDER_SIZE)),
            };

            PaneAxisHandleLayout {
                hitbox: window.insert_hitbox(handle_bounds, HitboxBehavior::BlockMouse),
                divider_bounds,
            }
        }
    }

    impl IntoElement for PaneAxisElement {
        type Element = Self;

        fn into_element(self) -> Self::Element {
            self
        }
    }

    impl Element for PaneAxisElement {
        type RequestLayoutState = ();
        type PrepaintState = PaneAxisLayout;

        fn id(&self) -> Option<ElementId> {
            Some(self.basis.into())
        }

        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }

        fn request_layout(
            &mut self,
            _global_id: Option<&GlobalElementId>,
            _inspector_id: Option<&gpui::InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (gpui::LayoutId, Self::RequestLayoutState) {
            let style = Style {
                flex_grow: 1.,
                flex_shrink: 1.,
                flex_basis: relative(0.).into(),
                size: size(relative(1.).into(), relative(1.).into()),
                ..Style::default()
            };
            (window.request_layout(style, None, cx), ())
        }

        fn prepaint(
            &mut self,
            global_id: Option<&GlobalElementId>,
            _inspector_id: Option<&gpui::InspectorElementId>,
            bounds: Bounds<Pixels>,
            _state: &mut Self::RequestLayoutState,
            window: &mut Window,
            cx: &mut App,
        ) -> PaneAxisLayout {
            let dragged_handle = window.with_element_state::<Rc<RefCell<Option<usize>>>, _>(
                global_id.unwrap(),
                |state, _cx| {
                    let state = state.unwrap_or_else(|| Rc::new(RefCell::new(None)));
                    (state.clone(), state)
                },
            );
            let flexes = self.flexes.lock().clone();
            let len = self.children.len();
            debug_assert!(flexes.len() == len);
            debug_assert!(flex_values_in_bounds(flexes.as_slice()));

            let total_flex = len as f32;

            let mut origin = bounds.origin;
            let space_per_flex = bounds.size.along(self.axis) / total_flex;

            let mut bounding_boxes = self.bounding_boxes.lock();
            bounding_boxes.clear();

            let mut layout = PaneAxisLayout {
                dragged_handle,
                children: Vec::new(),
            };
            for (ix, mut child) in mem::take(&mut self.children).into_iter().enumerate() {
                let child_flex = flexes[ix];

                let child_size = bounds
                    .size
                    .apply_along(self.axis, |_| space_per_flex * child_flex)
                    .map(|d| d.round());

                let child_bounds = Bounds {
                    origin,
                    size: child_size,
                };

                bounding_boxes.push(Some(child_bounds));
                child.layout_as_root(child_size.into(), window, cx);
                child.prepaint_at(origin, window, cx);

                origin = origin.apply_along(self.axis, |val| val + child_size.along(self.axis));

                let is_leaf_pane = self.is_leaf_pane_mask.get(ix).copied().unwrap_or(true);

                layout.children.push(PaneAxisChildLayout {
                    bounds: child_bounds,
                    element: child,
                    handle: None,
                    is_leaf_pane,
                })
            }

            for (ix, child_layout) in layout.children.iter_mut().enumerate() {
                if ix < len - 1 {
                    child_layout.handle = Some(Self::layout_handle(
                        self.axis,
                        child_layout.bounds,
                        window,
                        cx,
                    ));
                }
            }

            layout
        }

        fn paint(
            &mut self,
            _id: Option<&GlobalElementId>,
            _inspector_id: Option<&gpui::InspectorElementId>,
            bounds: gpui::Bounds<ui::prelude::Pixels>,
            _: &mut Self::RequestLayoutState,
            layout: &mut Self::PrepaintState,
            window: &mut Window,
            cx: &mut App,
        ) {
            for child in &mut layout.children {
                child.element.paint(window, cx);
            }

            // Codon defaults `inactive_opacity` to a subtle 0.85 (overlay
            // ~15% opaque over inactive leaf panes). With only two panes,
            // the divider highlights both sides equally, so the dim is
            // what tells the user which side is focused. Explicit user
            // settings (including 1.0 to disable) still win.
            const CODON_DEFAULT_INACTIVE_OPACITY: f32 = 0.85;
            let overlay_opacity = WorkspaceSettings::get(None, cx)
                .active_pane_modifiers
                .inactive_opacity
                .map(|val| val.0.clamp(0.0, 1.0))
                .and_then(|val| (val <= 1.).then_some(val))
                .or(Some(CODON_DEFAULT_INACTIVE_OPACITY));

            let mut overlay_background = cx.theme().colors().editor_background;
            if let Some(opacity) = overlay_opacity {
                overlay_background.fade_out(opacity);
            }

            let overlay_border = WorkspaceSettings::get(None, cx)
                .active_pane_modifiers
                .border_size
                .and_then(|val| (val >= 0.).then_some(val));

            for (ix, child) in &mut layout.children.iter_mut().enumerate() {
                if overlay_opacity.is_some() || overlay_border.is_some() {
                    // the overlay has to be painted in origin+1px with size width-1px
                    // in order to accommodate the divider between panels
                    let overlay_bounds = Bounds {
                        origin: child
                            .bounds
                            .origin
                            .apply_along(Axis::Horizontal, |val| val + px(1.)),
                        size: child
                            .bounds
                            .size
                            .apply_along(Axis::Horizontal, |val| val - px(1.)),
                    };

                    if overlay_opacity.is_some()
                        && child.is_leaf_pane
                        && self.active_pane_ix != Some(ix)
                    {
                        window.paint_quad(gpui::fill(overlay_bounds, overlay_background));
                    }

                    if let Some(border) = overlay_border
                        && self.active_pane_ix == Some(ix)
                        && child.is_leaf_pane
                    {
                        window.paint_quad(gpui::quad(
                            overlay_bounds,
                            0.,
                            gpui::transparent_black(),
                            border,
                            cx.theme().colors().border_selected,
                            BorderStyle::Solid,
                        ));
                    }
                }

                if let Some(handle) = child.handle.as_mut() {
                    let cursor_style = match self.axis {
                        Axis::Vertical => CursorStyle::ResizeRow,
                        Axis::Horizontal => CursorStyle::ResizeColumn,
                    };

                    if layout
                        .dragged_handle
                        .borrow()
                        .is_some_and(|dragged_ix| dragged_ix == ix)
                    {
                        window.set_window_cursor_style(cursor_style);
                    } else {
                        window.set_cursor_style(cursor_style, &handle.hitbox);
                    }

                    // tmux-style pane-active-border. Three cases:
                    //   1. Active pane is a *direct* leaf neighbour of this
                    //      divider → highlight the divider's full extent
                    //      (the perpendicular span is the active pane's
                    //      span by construction).
                    //   2. Active pane sits *inside* a nested subtree on
                    //      one side of this divider → highlight only the
                    //      segment of the divider that overlaps the active
                    //      pane's perpendicular bounds, default-colour the
                    //      rest. Needs the active pane's screen bounds,
                    //      which the workspace already tracks.
                    //   3. Neither → default colour, default thickness.
                    const HIGHLIGHT_THICKNESS: f32 = 3.0;
                    let direct_active = self
                        .active_pane_ix
                        .is_some_and(|active| ix == active || ix + 1 == active);
                    let subtree_active = !direct_active
                        && self
                            .active_subtree_ix
                            .is_some_and(|active| ix == active || ix + 1 == active);

                    let highlight_color = cx.theme().colors().border_focused;
                    let default_color = cx.theme().colors().pane_group_border;
                    let extra = px(HIGHLIGHT_THICKNESS - DIVIDER_SIZE);

                    if direct_active {
                        let highlight_bounds = Bounds {
                            origin: handle
                                .divider_bounds
                                .origin
                                .apply_along(self.axis, |o| o - extra / 2.0),
                            size: handle
                                .divider_bounds
                                .size
                                .apply_along(self.axis, |_| px(HIGHLIGHT_THICKNESS)),
                        };
                        window.paint_quad(gpui::fill(highlight_bounds, highlight_color));
                    } else if subtree_active
                        && let Some(active_pane) = self.active_pane.as_ref()
                        && let Some(active_bounds) = self
                            .workspace
                            .upgrade()
                            .and_then(|ws| ws.read(cx).center().bounding_box_for_pane(active_pane))
                    {
                        // Default divider first; clipped highlight on top.
                        window.paint_quad(gpui::fill(handle.divider_bounds, default_color));
                        let perp = self.axis.invert();
                        let div_start = handle.divider_bounds.origin.along(perp);
                        let div_end = div_start + handle.divider_bounds.size.along(perp);
                        let act_start = active_bounds.origin.along(perp);
                        let act_end = act_start + active_bounds.size.along(perp);
                        let lo = div_start.max(act_start);
                        let hi = div_end.min(act_end);
                        if hi > lo {
                            let segment_bounds = Bounds {
                                origin: handle
                                    .divider_bounds
                                    .origin
                                    .apply_along(self.axis, |o| o - extra / 2.0)
                                    .apply_along(perp, |_| lo),
                                size: handle
                                    .divider_bounds
                                    .size
                                    .apply_along(self.axis, |_| px(HIGHLIGHT_THICKNESS))
                                    .apply_along(perp, |_| hi - lo),
                            };
                            window.paint_quad(gpui::fill(segment_bounds, highlight_color));
                        }
                    } else {
                        window.paint_quad(gpui::fill(handle.divider_bounds, default_color));
                    }

                    window.on_mouse_event({
                        let dragged_handle = layout.dragged_handle.clone();
                        let flexes = self.flexes.clone();
                        let workspace = self.workspace.clone();
                        let handle_hitbox = handle.hitbox.clone();
                        move |e: &MouseDownEvent, phase, window, cx| {
                            if phase.bubble() && handle_hitbox.is_hovered(window) {
                                dragged_handle.replace(Some(ix));
                                if e.click_count >= 2 {
                                    let mut borrow = flexes.lock();
                                    *borrow = vec![1.; borrow.len()];
                                    workspace
                                        .update(cx, |this, cx| this.serialize_workspace(window, cx))
                                        .log_err();

                                    window.refresh();
                                }
                                cx.stop_propagation();
                            }
                        }
                    });
                    window.on_mouse_event({
                        let workspace = self.workspace.clone();
                        let dragged_handle = layout.dragged_handle.clone();
                        let flexes = self.flexes.clone();
                        let child_bounds = child.bounds;
                        let axis = self.axis;
                        move |e: &MouseMoveEvent, phase, window, cx| {
                            let dragged_handle = dragged_handle.borrow();
                            if phase.bubble() && *dragged_handle == Some(ix) {
                                Self::compute_resize(
                                    &flexes,
                                    e,
                                    ix,
                                    axis,
                                    child_bounds.origin,
                                    bounds.size,
                                    workspace.clone(),
                                    window,
                                    cx,
                                )
                            }
                        }
                    });
                }
            }

            window.on_mouse_event({
                let dragged_handle = layout.dragged_handle.clone();
                move |_: &MouseUpEvent, phase, _window, _cx| {
                    if phase.bubble() {
                        dragged_handle.replace(None);
                    }
                }
            });
        }
    }

    impl ParentElement for PaneAxisElement {
        fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
            self.children.extend(elements)
        }
    }

    fn flex_values_in_bounds(flexes: &[f32]) -> bool {
        (flexes.iter().copied().sum::<f32>() - flexes.len() as f32).abs() < 0.001
    }
}
