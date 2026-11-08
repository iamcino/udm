//! System tray integration (Phase 2 spike, Windows first).
//!
//! Scope for this first version, following the popup/tray research and the
//! Codex review of it (review-artifacts/qwen-popup-tray-extension-response-
//! 2026-11-04.md, codex-qwen-popup-tray-review-2026-11-04.md):
//!
//! * Tray icon + minimal menu only: Show/Hide, Pause all, Resume all, Exit
//!   (research table B10). No "Add URL" yet, no completion notifications,
//!   no tooltip-as-notification promises.
//! * No change to existing window behaviour: the window close button still
//!   exits the app. Minimise/close-to-tray policy is an open product
//!   decision and is NOT implemented here.
//! * No second GUI runtime: menu/icon events are drained from the
//!   `tray-icon` channels inside the existing 150 ms GUI poll in state.rs.
//! * No registry writes, no admin rights, no global hotkeys.
//! * Tray creation failure must never take the app down: `Tray::new`
//!   returns `None` and the app simply runs without a tray icon.
//!
//! The icon is the 32 px tray PNG from the logo set, embedded in the
//! binary (the dist folder does not ship the assets directory) and decoded
//! with the `image` crate (PNG only).
//!
//! Non-Windows platforms get an inert stub for now; a Linux tray (ksni)
//! is out of scope for this spike.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Left click / double click on the tray icon: bring the window up.
    Show,
    /// "Show UDM" / "Hide UDM" menu item.
    ToggleShowHide,
    PauseAll,
    ResumeAll,
    Exit,
}

#[cfg(windows)]
mod imp {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::TrayAction;

    const ID_SHOW_HIDE: &str = "udm.tray.show_hide";
    const ID_PAUSE_ALL: &str = "udm.tray.pause_all";
    const ID_RESUME_ALL: &str = "udm.tray.resume_all";
    const ID_EXIT: &str = "udm.tray.exit";

    fn text(vi: bool, en: &'static str, vi_text: &'static str) -> &'static str {
        if vi {
            vi_text
        } else {
            en
        }
    }

    pub struct Tray {
        // Kept alive for the lifetime of the app; dropping the TrayIcon
        // removes the icon from the notification area.
        _icon: TrayIcon,
        show_hide: MenuItem,
        pause_all: MenuItem,
        resume_all: MenuItem,
        exit: MenuItem,
        visible: bool,
        vi: bool,
    }

    impl Tray {
        pub fn new(vi: bool) -> Option<Tray> {
            let icon = load_icon()?;
            let show_hide = MenuItem::with_id(
                ID_SHOW_HIDE,
                text(vi, "Hide UDM", "Ẩn UDM"),
                true,
                None,
            );
            let pause_all = MenuItem::with_id(
                ID_PAUSE_ALL,
                text(vi, "Pause all", "Tạm dừng tất cả"),
                true,
                None,
            );
            let resume_all = MenuItem::with_id(
                ID_RESUME_ALL,
                text(vi, "Resume all", "Tiếp tục tất cả"),
                true,
                None,
            );
            let exit = MenuItem::with_id(ID_EXIT, text(vi, "Exit", "Thoát"), true, None);
            let menu = Menu::new();
            menu.append_items(&[
                &show_hide,
                &PredefinedMenuItem::separator(),
                &pause_all,
                &resume_all,
                &PredefinedMenuItem::separator(),
                &exit,
            ])
            .ok()?;
            let icon_handle = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("UDM — Universal Download Manager")
                .with_icon(icon)
                .build()
                .ok()?;
            tracing::info!(vi, "tray icon initialized");
            Some(Tray {
                _icon: icon_handle,
                show_hide,
                pause_all,
                resume_all,
                exit,
                visible: true,
                vi,
            })
        }

        /// Drain pending tray/menu events without blocking. Called from
        /// the GUI poll timer, so this runs on the event-loop thread.
        pub fn drain(&self) -> Vec<TrayAction> {
            let mut actions = Vec::new();
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                let action = if event.id() == self.show_hide.id() {
                    TrayAction::ToggleShowHide
                } else if event.id() == self.pause_all.id() {
                    TrayAction::PauseAll
                } else if event.id() == self.resume_all.id() {
                    TrayAction::ResumeAll
                } else if event.id() == self.exit.id() {
                    TrayAction::Exit
                } else {
                    continue;
                };
                actions.push(action);
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                match event {
                    TrayIconEvent::Click { .. } | TrayIconEvent::DoubleClick { .. } => {
                        actions.push(TrayAction::Show)
                    }
                    _ => {}
                }
            }
            actions
        }

        pub fn is_visible(&self) -> bool {
            self.visible
        }

        pub fn set_visible(&mut self, visible: bool) {
            self.visible = visible;
            self.show_hide.set_text(if visible {
                text(self.vi, "Hide UDM", "Ẩn UDM")
            } else {
                text(self.vi, "Show UDM", "Hiện UDM")
            });
        }
    }

    fn load_icon() -> Option<Icon> {
        const PNG: &[u8] = include_bytes!("../assets/tray/tray-32.png");
        let img = image::load_from_memory_with_format(PNG, image::ImageFormat::Png)
            .ok()?
            .into_rgba8();
        let (width, height) = (img.width(), img.height());
        Icon::from_rgba(img.into_raw(), width, height).ok()
    }
}

#[cfg(not(windows))]
mod imp {
    use super::TrayAction;

    pub struct Tray;

    impl Tray {
        pub fn new(_vi: bool) -> Option<Tray> {
            None
        }

        pub fn drain(&self) -> Vec<TrayAction> {
            Vec::new()
        }

        pub fn is_visible(&self) -> bool {
            true
        }

        pub fn set_visible(&mut self, _visible: bool) {}
    }
}

pub use imp::Tray;
