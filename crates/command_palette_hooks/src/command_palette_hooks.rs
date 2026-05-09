//! Provides hooks for customizing the behavior of the command palette.

#![deny(missing_docs)]

use std::{any::TypeId, rc::Rc};

use collections::HashSet;
use derive_more::{Deref, DerefMut};
use gpui::{Action, App, BorrowAppContext, Global, Task, WeakEntity};
use workspace::Workspace;

/// Initializes the command palette hooks.
pub fn init(cx: &mut App) {
    cx.set_global(GlobalCommandPaletteFilter::default());
}

/// A filter for the command palette.
#[derive(Default)]
pub struct CommandPaletteFilter {
    hidden_namespaces: HashSet<&'static str>,
    hidden_action_types: HashSet<TypeId>,
    /// Actions that have explicitly been shown. These should be shown even if
    /// they are in a hidden namespace.
    shown_action_types: HashSet<TypeId>,
}

#[derive(Deref, DerefMut, Default)]
struct GlobalCommandPaletteFilter(CommandPaletteFilter);

impl Global for GlobalCommandPaletteFilter {}

impl CommandPaletteFilter {
    /// Returns the global [`CommandPaletteFilter`], if one is set.
    pub fn try_global(cx: &App) -> Option<&CommandPaletteFilter> {
        cx.try_global::<GlobalCommandPaletteFilter>()
            .map(|filter| &filter.0)
    }

    /// Returns a mutable reference to the global [`CommandPaletteFilter`].
    pub fn global_mut(cx: &mut App) -> &mut Self {
        cx.global_mut::<GlobalCommandPaletteFilter>()
    }

    /// Updates the global [`CommandPaletteFilter`] using the given closure.
    pub fn update_global<F>(cx: &mut App, update: F)
    where
        F: FnOnce(&mut Self, &mut App),
    {
        if cx.has_global::<GlobalCommandPaletteFilter>() {
            cx.update_global(|this: &mut GlobalCommandPaletteFilter, cx| update(&mut this.0, cx))
        }
    }

    /// Returns whether the given [`Action`] is hidden by the filter.
    pub fn is_hidden(&self, action: &dyn Action) -> bool {
        let name = action.name();
        let namespace = name.split("::").next().unwrap_or("malformed action name");

        // If this action has specifically been shown then it should be visible.
        if self.shown_action_types.contains(&action.type_id()) {
            return false;
        }

        self.hidden_namespaces.contains(namespace)
            || self.hidden_action_types.contains(&action.type_id())
    }

    /// Hides all actions in the given namespace.
    pub fn hide_namespace(&mut self, namespace: &'static str) {
        self.hidden_namespaces.insert(namespace);
    }

    /// Shows all actions in the given namespace.
    pub fn show_namespace(&mut self, namespace: &'static str) {
        self.hidden_namespaces.remove(namespace);
    }

    /// Hides all actions with the given types.
    pub fn hide_action_types<'a>(&mut self, action_types: impl IntoIterator<Item = &'a TypeId>) {
        for action_type in action_types {
            self.hidden_action_types.insert(*action_type);
            self.shown_action_types.remove(action_type);
        }
    }

    /// Shows all actions with the given types.
    pub fn show_action_types<'a>(&mut self, action_types: impl IntoIterator<Item = &'a TypeId>) {
        for action_type in action_types {
            self.shown_action_types.insert(*action_type);
            self.hidden_action_types.remove(action_type);
        }
    }
}

/// The result of intercepting a command palette command.
#[derive(Debug)]
pub struct CommandInterceptItem {
    /// The action produced as a result of the interception.
    pub action: Box<dyn Action>,
    /// The display string to show in the command palette for this result.
    pub string: String,
    /// The character positions in the string that match the query.
    /// Used for highlighting matched characters in the command palette UI.
    pub positions: Vec<usize>,
}

/// The result of intercepting a command palette command.
#[derive(Default, Debug)]
pub struct CommandInterceptResult {
    /// The items
    pub results: Vec<CommandInterceptItem>,
    /// Whether or not to continue to show the normal matches
    pub exclusive: bool,
}

/// An interceptor for the command palette.
#[derive(Clone)]
pub struct GlobalCommandPaletteInterceptor(
    Rc<dyn Fn(&str, WeakEntity<Workspace>, &mut App) -> Task<CommandInterceptResult>>,
);

impl Global for GlobalCommandPaletteInterceptor {}

impl GlobalCommandPaletteInterceptor {
    /// Sets the global interceptor.
    ///
    /// This will override the previous interceptor, if it exists.
    pub fn set(
        cx: &mut App,
        interceptor: impl Fn(&str, WeakEntity<Workspace>, &mut App) -> Task<CommandInterceptResult>
        + 'static,
    ) {
        cx.set_global(Self(Rc::new(interceptor)));
    }

    /// Clears the global interceptor.
    pub fn clear(cx: &mut App) {
        if cx.has_global::<Self>() {
            cx.remove_global::<Self>();
        }
    }

    /// Intercepts the given query from the command palette.
    pub fn intercept(
        query: &str,
        workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Option<Task<CommandInterceptResult>> {
        let interceptor = cx.try_global::<Self>()?;
        let handler = interceptor.0.clone();
        Some(handler(query, workspace, cx))
    }
}

// --- Codon: Selection-aware action filtering ---

/// The kind of object a selection refers to.
/// Used by actions to declare what they accept and by the command palette
/// to filter applicable verbs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    /// Text range in an editor buffer
    Text,
    /// A file entry
    File,
    /// A directory entry
    Dir,
    /// A git diff hunk
    Hunk,
    /// A git commit
    Commit,
    /// A git branch
    Branch,
    /// A terminal command+output block
    Block,
    /// A URL
    Url,
    /// A diagnostic message
    Diagnostic,
    /// An agent conversation message
    Message,
}

/// Registry mapping action types to the ObjectKinds they accept.
///
/// - Actions with no entry: accept any selection (shown always)
/// - Actions with empty accepts: nullary verbs (shown always)
/// - Actions with specific kinds: shown only when selection matches
#[derive(Default)]
pub struct ActionAcceptsRegistry {
    accepts: collections::HashMap<TypeId, &'static [ObjectKind]>,
}

impl Global for ActionAcceptsRegistry {}

impl ActionAcceptsRegistry {
    /// Register an action type with the ObjectKinds it accepts.
    pub fn register<A: Action>(&mut self, accepts: &'static [ObjectKind]) {
        self.accepts.insert(TypeId::of::<A>(), accepts);
    }

    /// Check if an action is applicable for the given selection kind.
    pub fn is_applicable(
        &self,
        action_type_id: TypeId,
        selection_kind: Option<ObjectKind>,
    ) -> bool {
        match self.accepts.get(&action_type_id) {
            None => true,
            Some(accepted) => {
                if accepted.is_empty() {
                    true
                } else {
                    match selection_kind {
                        None => true,
                        Some(kind) => accepted.contains(&kind),
                    }
                }
            }
        }
    }
}
