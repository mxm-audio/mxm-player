//! Showing a plugin's own interface, in a floating window it owns.
//!
//! # Floating, not embedded
//!
//! The plugin creates and owns a top-level window; the player never gives it a parent. Embedding —
//! a plugin's window living inside ours — was rejected for reasons that are recorded in
//! `plans/plan-plugin-gui-hosting.md` §6 (in the private archive), the shortest of which is that
//! **Wayland has no cross-process embedding primitive at all**, so CLAP does not embed there.
//! Floating works everywhere and needs no per-platform code here.
//!
//! # Three invariants, each of which fails silently if broken
//!
//! - **Exactly one `destroy` per successful `create`, on every path.** Including after
//!   `closed(was_destroyed = true)`, which *requires* it as an acknowledgement rather than
//!   forbidding it. nice-plug refuses a second `create` while its editor handle is still set, so a
//!   missed acknowledgement means the editor never opens again for the life of the instance.
//! - **Ownership never gates the editor.** If the player cannot make its window the owner, the
//!   editor still opens. Refusing a working interface over a window-manager property would trade a
//!   real capability for a cosmetic one.
//! - **Every `clap.gui` call happens on the main thread.** `PluginInstance` is `!Send` and lives on
//!   the GUI thread, so that is structural here rather than a rule to remember — but the plugin's
//!   callbacks arrive on any thread, and those are queued in `HostShared` and applied from
//!   `Engine::service_editor`.

use clack_extensions::gui::{GuiApiType, GuiConfiguration, GuiSize, PluginGui};
use clack_host::prelude::*;

use super::Engine;
use crate::host::{MxmHost, PlayerHostState};
use crate::ownership;

/// Whose editor: the source's, or one effect's in the chain.
///
/// **As many at once as there are plugins** — the owner's ruling, 2026-09-04, replacing the
/// one-at-a-time rule this player shipped with for a day. Tweaking a synth against the effect
/// after it is one job, and a player that made you close one window to see the other was making
/// the user pay for the engine's convenience.
///
/// Nothing about the CLAP side needed the restriction: each plugin instance owns its own window,
/// its own `create`/`destroy` pair and its own ownership claim. What was single was the engine's
/// *bookkeeping* — one `EditorState`, one drained flag set — so that is what became per target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorTarget {
    #[default]
    Source,
    Fx(usize),
}

/// Whether a plugin's editor is open, and what the player knows about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorState {
    /// Whether `create` has succeeded and `destroy` has not yet been called.
    pub open: bool,
    /// Whether the window is currently shown. Hiding does not destroy it — see
    /// [`Engine::hide_editor`] — so `open` and `visible` are different questions.
    pub visible: bool,
    /// Whether the player made its own window the editor window's owner.
    ///
    /// `false` is a normal outcome, not an error: on platforms where it is unsupported, and for
    /// plugins whose window cannot be positively identified. The reason travels with it.
    pub owned: Ownership,
    /// Whose window it is. Meaningful only while `open`.
    pub target: EditorTarget,
}

/// The result of trying to make the player's window own the editor's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Ownership {
    /// Not attempted, because no editor is open.
    #[default]
    NotOpen,
    /// The editor window is owned by the player's: it stays above it, minimises with it, and takes
    /// no taskbar entry of its own.
    Owned,
    /// Not owned, and why. Shown to the user rather than only logged, because the visible symptom
    /// — the editor sinking behind the player — otherwise looks like a bug.
    Unowned(&'static str),
}

impl Ownership {
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Unowned(why) => Some(why),
            _ => None,
        }
    }
}

/// Why an editor could not be shown. Every one of these is displayed, never swallowed:
/// `src/envelope.rs` establishes that a refusal without a reason is a bug.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorRefusal {
    NoPlugin,
    /// The plugin does not implement `clap.gui` at all.
    NoGuiExtension,
    /// It implements it, but not as a floating window on this platform's API.
    NoFloatingSupport,
    /// `create` returned false.
    CreateFailed,
    /// `show` returned false. The window exists, so it has already been destroyed again.
    ShowFailed,
}

impl EditorRefusal {
    pub fn message(&self) -> &'static str {
        match self {
            Self::NoPlugin => "No plugin is loaded.",
            Self::NoGuiExtension => "This plugin has no interface of its own. Use Parameters.",
            Self::NoFloatingSupport => {
                "This plugin's interface cannot open as its own window on this platform. \
                 Use Parameters."
            }
            Self::CreateFailed => "The plugin refused to create its interface.",
            Self::ShowFailed => "The plugin created its interface but could not show it.",
        }
    }
}

/// The window API this platform uses, as CLAP names it.
///
/// Not a `cfg` chain of our own: clack already knows the mapping, and getting it wrong here would
/// mean asking a plugin for a window kind the platform cannot produce.
fn platform_api() -> GuiApiType<'static> {
    #[cfg(target_os = "windows")]
    {
        GuiApiType::WIN32
    }
    #[cfg(target_os = "macos")]
    {
        GuiApiType::COCOA
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        GuiApiType::X11
    }
}

impl Engine {
    /// The instance an editor target names, if it exists.
    fn instance_for(&mut self, target: EditorTarget) -> Option<&mut PluginInstance<MxmHost>> {
        match target {
            EditorTarget::Source => self.loaded.as_mut().map(|l| &mut l.instance),
            EditorTarget::Fx(index) => self.fx.get_mut(index).map(|s| &mut s.instance),
        }
    }

    /// The host state an editor target's plugin reports into.
    fn shared_for(&self, target: EditorTarget) -> Option<&PlayerHostState> {
        match target {
            EditorTarget::Source => Some(&self.shared),
            EditorTarget::Fx(index) => self.fx.get(index).map(|s| &*s.shared),
        }
    }

    /// Opens the source plugin's editor as a floating window.
    ///
    /// `host_window` is the player's own window, used to claim ownership. Ownership failing does
    /// not fail the call — see the module header.
    pub fn open_editor(
        &mut self,
        host_window: Option<ownership::HostWindow>,
    ) -> Result<EditorState, EditorRefusal> {
        self.open_editor_for(EditorTarget::Source, host_window)
    }

    /// Opens one effect's editor. The only way an effect is played in this player, which shows
    /// no effect parameters of its own.
    pub fn open_fx_editor(
        &mut self,
        index: usize,
        host_window: Option<ownership::HostWindow>,
    ) -> Result<EditorState, EditorRefusal> {
        self.open_editor_for(EditorTarget::Fx(index), host_window)
    }

    fn open_editor_for(
        &mut self,
        target: EditorTarget,
        host_window: Option<ownership::HostWindow>,
    ) -> Result<EditorState, EditorRefusal> {
        // Already built and merely hidden: show it rather than creating a second window for the
        // same plugin, which nice-plug would refuse anyway.
        if let Some(state) = self.editor_for(target) {
            if !state.visible {
                self.show_editor_for(target);
            }
            return Ok(self.editor_state_for(target));
        }
        let instance = self.instance_for(target).ok_or(EditorRefusal::NoPlugin)?;
        let gui = Self::gui_of(instance).ok_or(EditorRefusal::NoGuiExtension)?;
        let api = platform_api();
        let config = GuiConfiguration {
            api_type: api,
            is_floating: true,
        };

        let mut handle = instance.plugin_handle();
        if !gui.is_api_supported(&mut handle, config) {
            return Err(EditorRefusal::NoFloatingSupport);
        }

        // Ownership is claimed *during* create, while the window exists but is not yet visible.
        // baseview builds the window inside `create` for a floating editor — `show` only reveals
        // it — so this is the only point in the lifecycle where "owned before it appears" is
        // achievable. The watcher takes a before-snapshot now and resolves after `create`.
        let watcher = ownership::Watcher::start();

        if gui.create(&mut handle, config).is_err() {
            return Err(EditorRefusal::CreateFailed);
        }

        let owned = match host_window {
            Some(host) => watcher.claim(host),
            None => Ownership::Unowned("The player has no window to own it."),
        };

        if gui.show(&mut handle).is_err() {
            // The window exists, so the create must still be acknowledged. This is the path that
            // is easy to leak, and leaking it means the editor never opens again.
            gui.destroy(&mut handle);
            return Err(EditorRefusal::ShowFailed);
        }

        let state = EditorState {
            open: true,
            visible: true,
            owned,
            target,
        };
        self.editors.push(state.clone());
        Ok(state)
    }

    /// The open editor for `target`, if there is one.
    fn editor_for(&self, target: EditorTarget) -> Option<&EditorState> {
        self.editors.iter().find(|e| e.target == target)
    }

    fn editor_for_mut(&mut self, target: EditorTarget) -> Option<&mut EditorState> {
        self.editors.iter_mut().find(|e| e.target == target)
    }

    /// Hides the editor without destroying it.
    ///
    /// This is what "Hide editor" does. `destroy` is deliberately **not** called: the window stays
    /// built, so showing it again is instant, and the plugin keeps whatever transient state its
    /// interface holds. Tearing it down and rebuilding it on every toggle would be work nobody
    /// asked for.
    pub fn hide_editor(&mut self) {
        self.hide_editor_for(EditorTarget::Source);
    }

    /// The same, for whichever window is meant.
    pub fn hide_editor_for(&mut self, target: EditorTarget) {
        match self.editor_for_mut(target) {
            Some(state) if state.visible => state.visible = false,
            _ => return,
        }
        self.with_gui(target, |gui, handle| {
            let _ = gui.hide(handle);
        });
    }

    /// Shows an editor that was hidden rather than destroyed.
    pub fn show_editor(&mut self) {
        self.show_editor_for(EditorTarget::Source);
    }

    /// The same, for whichever window is meant.
    pub fn show_editor_for(&mut self, target: EditorTarget) {
        match self.editor_for_mut(target) {
            Some(state) if !state.visible => state.visible = true,
            _ => return,
        }
        self.with_gui(target, |gui, handle| {
            let _ = gui.show(handle);
        });
    }

    /// Hides and destroys the editor. Safe to call when none is open.
    ///
    /// **This is the only place `destroy` is called**, so the exactly-one rule is a property of
    /// the code rather than a discipline. It runs when the plugin goes away — an unload, or the
    /// plugin closing its own window — not when the user merely hides it.
    pub fn close_editor(&mut self) {
        self.close_editor_for(EditorTarget::Source);
    }

    /// Closes every open editor, in the order they were opened.
    pub fn close_all_editors(&mut self) {
        for target in self.open_editor_targets() {
            self.close_editor_for(target);
        }
    }

    /// The targets with an editor open, which is what anything iterating them wants — collected,
    /// because acting on one needs `&mut self` and the list lives behind `&self`.
    pub fn open_editor_targets(&self) -> Vec<EditorTarget> {
        self.editors.iter().map(|e| e.target).collect()
    }

    /// Hides and destroys one editor. Safe to call when that one is not open.
    pub fn close_editor_for(&mut self, target: EditorTarget) {
        let Some(position) = self.editors.iter().position(|e| e.target == target) else {
            return;
        };
        self.editors.remove(position);

        let Some(instance) = self.instance_for(target) else {
            // The instance is already gone, which destroyed the editor with it.
            return;
        };
        let Some(gui) = Self::gui_of(instance) else {
            return;
        };

        let mut handle = instance.plugin_handle();
        let _ = gui.hide(&mut handle);
        gui.destroy(&mut handle);
    }

    /// Applies whatever the plugin asked for since the last frame.
    ///
    /// Called from the GUI thread every frame. The requests themselves arrive on arbitrary plugin
    /// threads and are recorded in atomics by `HostShared`; this is where they become `clap.gui`
    /// calls, which may only happen here.
    ///
    /// Returns whether anything changed, so the caller can repaint: showing or hiding a foreign
    /// top-level window invalidates ours without generating any input for egui to react to, and a
    /// reactive repaint loop would otherwise leave the player showing a bare background.
    pub fn service_editor(&mut self) -> bool {
        // **Every plugin, not only the ones with a window open.** A plugin whose editor was closed
        // may still have set a flag a moment before, and a flag left set fires the next time that
        // editor opens — a window that hides itself the instant it appears. Draining them all is
        // what makes that impossible.
        let mut changed = false;
        let mut targets = vec![EditorTarget::Source];
        targets.extend((0..self.fx.len()).map(EditorTarget::Fx));
        for target in targets {
            changed |= self.service_editor_for(target);
        }
        changed
    }

    fn service_editor_for(&mut self, target: EditorTarget) -> bool {
        use std::sync::atomic::Ordering;

        // Everything is drained into locals first. The flags live behind the target's host state,
        // and acting on them needs `&mut self` — so reading and acting cannot overlap.
        let Some(shared) = self.shared_for(target) else {
            return false;
        };
        let (closed, resize, hide, show) = {
            let requests = &shared.requests;
            let notifications = &shared.notifications;

            let closed = notifications.gui_closed.swap(false, Ordering::Acquire);
            if closed {
                notifications
                    .gui_closed_was_destroyed
                    .store(false, Ordering::Relaxed);
            }

            let resize = requests
                .gui_resize_pending
                .swap(false, Ordering::Acquire)
                .then(|| {
                    let packed = requests.gui_resize.load(Ordering::Relaxed);
                    GuiSize {
                        width: (packed >> 32) as u32,
                        height: (packed & 0xFFFF_FFFF) as u32,
                    }
                });

            (
                closed,
                resize,
                requests.gui_hide.swap(false, Ordering::Acquire),
                requests.gui_show.swap(false, Ordering::Acquire),
            )
        };

        // `closed` first, and it wins outright: it is the one that must never be dropped. A plugin
        // that closed its own window owes us a `destroy`, and skipping it leaves nice-plug's editor
        // handle set so every later `create` is refused — silently, and for the rest of the
        // instance's life. The other three are moot once the editor is gone.
        if closed {
            self.close_editor_for(target);
            return true;
        }

        if self.editor_for(target).is_none() {
            return false;
        }

        if let Some(size) = resize {
            self.with_gui(target, |gui, handle| {
                let _ = gui.set_size(handle, size);
            });
        }

        if hide {
            self.hide_editor_for(target);
        }

        if show {
            self.show_editor_for(target);
        }

        resize.is_some() || hide || show
    }

    /// Whether the loaded plugin can show a floating editor, without creating one.
    ///
    /// Used to decide whether the "Show editor" control is offered at all, so the answer has to be
    /// available before anything is created.
    pub fn editor_available(&self) -> Result<(), EditorRefusal> {
        let loaded = self.loaded.as_ref().ok_or(EditorRefusal::NoPlugin)?;
        let gui: PluginGui = loaded
            .instance
            .plugin_shared_handle()
            .get_extension()
            .ok_or(EditorRefusal::NoGuiExtension)?;

        // `is_api_supported` needs a main-thread handle, and this is the main thread — but the
        // borrow is immutable here, so the check is deferred to `open_editor`. What this rules out
        // cheaply is the common case: a plugin with no `clap.gui` at all.
        let _ = gui;
        Ok(())
    }

    /// Whether the loaded plugin will open a **floating** editor, without creating one.
    ///
    /// This is the question the nice-plug fork's patch (the monorepo's vendored nice-plug) exists
    /// to change: upstream refuses every floating configuration, so before the patch this is
    /// `false` for mxm-mono-01 and the player can never show its interface. Separate from
    /// [`Engine::editor_available`] because it needs a main-thread handle and therefore
    /// `&mut self`.
    pub fn editor_floating_supported(&mut self) -> bool {
        self.floating_supported_for(EditorTarget::Source)
    }

    /// The same question for one effect, so a strip can offer its editor button honestly.
    pub fn fx_editor_floating_supported(&mut self, index: usize) -> bool {
        self.floating_supported_for(EditorTarget::Fx(index))
    }

    fn floating_supported_for(&mut self, target: EditorTarget) -> bool {
        let Some(instance) = self.instance_for(target) else {
            return false;
        };
        let Some(gui) = Self::gui_of(instance) else {
            return false;
        };
        let mut handle = instance.plugin_handle();
        gui.is_api_supported(
            &mut handle,
            GuiConfiguration {
                api_type: platform_api(),
                is_floating: true,
            },
        )
    }

    /// The **source's** editor state. What the app bar's own button reads, and what it must read:
    /// that button is the instrument's, and it once said *Hide editor* because an effect's window
    /// was open, then hid the effect's (the owner, 2026-09-04).
    pub fn editor_state(&self) -> EditorState {
        self.editor_state_for(EditorTarget::Source)
    }

    /// One target's editor state, or the closed state when it has no window.
    pub fn editor_state_for(&self, target: EditorTarget) -> EditorState {
        self.editor_for(target).cloned().unwrap_or(EditorState {
            target,
            ..EditorState::default()
        })
    }

    /// Whether any editor is open at all.
    pub fn any_editor_open(&self) -> bool {
        !self.editors.is_empty()
    }

    fn gui_of(instance: &PluginInstance<MxmHost>) -> Option<PluginGui> {
        instance.plugin_shared_handle().get_extension()
    }

    fn with_gui(
        &mut self,
        target: EditorTarget,
        f: impl FnOnce(&PluginGui, &mut PluginMainThreadHandle<'_>),
    ) {
        let Some(instance) = self.instance_for(target) else {
            return;
        };
        let Some(gui) = Self::gui_of(instance) else {
            return;
        };
        let mut handle = instance.plugin_handle();
        f(&gui, &mut handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_refusal_carries_a_reason() {
        // `src/envelope.rs` establishes the contract: a refusal without a reason is a bug. This is
        // the same rule applied to one more capability.
        for refusal in [
            EditorRefusal::NoPlugin,
            EditorRefusal::NoGuiExtension,
            EditorRefusal::NoFloatingSupport,
            EditorRefusal::CreateFailed,
            EditorRefusal::ShowFailed,
        ] {
            let message = refusal.message();
            assert!(!message.is_empty());
            assert!(
                message.ends_with('.'),
                "{refusal:?} reads as a fragment: {message:?}"
            );
        }
    }

    #[test]
    fn unowned_carries_a_reason_and_owned_does_not() {
        assert_eq!(Ownership::Owned.reason(), None);
        assert_eq!(Ownership::NotOpen.reason(), None);
        assert_eq!(Ownership::Unowned("because").reason(), Some("because"));
    }

    #[test]
    fn a_fresh_editor_state_is_closed_and_unowned() {
        let state = EditorState::default();
        assert!(!state.open);
        assert_eq!(state.owned, Ownership::NotOpen);
        assert_eq!(state.target, EditorTarget::Source);
    }
}
