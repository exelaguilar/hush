//! System tray icon and menu. Events wake the UI (even while the window is
//! hidden) through `request_repaint`.

use crate::icon_art::render_icon_state;
use std::{cell::Cell, sync::mpsc};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

pub enum TrayAction {
    Open,
    OpenMicTest,
    ToggleCleanMic,
    ToggleMute,
    Quit,
}

pub struct Tray {
    _icon: TrayIcon,
    clean_mic: CheckMenuItem,
    mute: CheckMenuItem,
    /// Whether the icon currently shows the active (teal) state.
    icon_active: Cell<bool>,
    actions: mpsc::Receiver<TrayAction>,
}

impl Tray {
    pub fn new(
        clean_mic_on: bool,
        muted: bool,
        wake: impl Fn() + Send + Sync + Clone + 'static,
    ) -> Option<Self> {
        let open = MenuItem::new("Open Hush", true, None);
        let mic_test = MenuItem::new("Mic Test…", true, None);
        let clean_mic = CheckMenuItem::new("Clean Mic", true, clean_mic_on, None);
        let mute = CheckMenuItem::new("Mute microphone", true, muted, None);
        let quit = MenuItem::new("Quit Hush", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open,
            &mic_test,
            &PredefinedMenuItem::separator(),
            &clean_mic,
            &mute,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .ok()?;

        let icon = tray_icon::Icon::from_rgba(render_icon_state(32, clean_mic_on && !muted), 32, 32).ok()?;
        let tray = TrayIconBuilder::new()
            .with_tooltip("Hush")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .ok()?;

        let (sender, actions) = mpsc::channel();
        let (open_id, mic_test_id, toggle_id, mute_id, quit_id) = (
            open.id().clone(),
            mic_test.id().clone(),
            clean_mic.id().clone(),
            mute.id().clone(),
            quit.id().clone(),
        );
        let menu_sender = sender.clone();
        let menu_wake = wake.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = if event.id == open_id {
                TrayAction::Open
            } else if event.id == mic_test_id {
                TrayAction::OpenMicTest
            } else if event.id == toggle_id {
                TrayAction::ToggleCleanMic
            } else if event.id == mute_id {
                TrayAction::ToggleMute
            } else if event.id == quit_id {
                TrayAction::Quit
            } else {
                return;
            };
            if matches!(action, TrayAction::Open | TrayAction::OpenMicTest) {
                crate::instance::show_own_window();
            }
            let _ = menu_sender.send(action);
            menu_wake();
        }));
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            let open = matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            );
            if open {
                crate::instance::show_own_window();
                let _ = sender.send(TrayAction::Open);
                wake();
            }
        }));

        Some(Self {
            _icon: tray,
            clean_mic,
            mute,
            icon_active: Cell::new(clean_mic_on && !muted),
            actions,
        })
    }

    pub fn poll(&self) -> Option<TrayAction> {
        self.actions.try_recv().ok()
    }

    /// Keeps the menu checkmarks and icon in sync with the current state.
    /// The icon is grey while Clean Mic is off, the microphone is muted, or
    /// the microphone needs attention.
    pub fn sync(&self, clean_mic_on: bool, muted: bool, attention: bool) {
        if self.clean_mic.is_checked() != clean_mic_on {
            self.clean_mic.set_checked(clean_mic_on);
        }
        if self.mute.is_checked() != muted {
            self.mute.set_checked(muted);
        }
        let active = clean_mic_on && !muted && !attention;
        if self.icon_active.get() != active {
            self.icon_active.set(active);
            if let Ok(icon) = tray_icon::Icon::from_rgba(render_icon_state(32, active), 32, 32) {
                let _ = self._icon.set_icon(Some(icon));
            }
        }
    }

    pub fn set_tooltip(&self, text: &str) {
        let _ = self._icon.set_tooltip(Some(text));
    }
}
