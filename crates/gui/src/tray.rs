//! System tray integration (Phase 2 spike, Windows first).
//!
//! Scope for this first version, following the popup/tray research and the
//! Codex review of it (review-artifacts/qwen-popup-tray-extension-response-
//! 2026-11-04.md, codex-qwen-popup-tray-review-2026-11-04.md):
//!
//! * Tray icon + minimal menu only: Show/Hide, Pause all, Resume all, Exit
//!   (research table B10). No "Add URL" yet, no completion notifications,
//!   no tooltip-as-notification promises.
//! * Window lifecycle policy (close/minimise to tray, start hidden) was
//!   an open product decision at spike time; the owner settled it in the
//!   G3 package and it is implemented in state.rs via Settings. The tray
//!   itself only owns the icon, the menu (including the read-only summary
//!   block added by G3) and the Show/Hide label state.
//! * G3 menu summary: disabled items at the top of the menu carry a totals
//!   line plus up to three active-file lines, refreshed through
//!   `Tray::set_summary` from the GUI poll (throttled in state.rs).
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
    /// Double click on the tray icon: bring the window up. A single click
    /// is deliberately neutral (G3 part C1, owner ruling): it neither
    /// shows the window nor opens the popup.
    Show,
    /// "Active downloads..." menu item: toggle the downloads popup. The
    /// popup opens only from explicit menu choices (here and in the app's
    /// main menu), never from an icon click.
    ShowPopup,
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
        summary_total: MenuItem,
        summary_files: Vec<MenuItem>,
        popup_item: MenuItem,
        pause_all: MenuItem,
        resume_all: MenuItem,
        exit: MenuItem,
        visible: bool,
        vi: bool,
        summary_cache: String,
    }

    const ID_SUMMARY_TOTAL: &str = "udm.tray.summary_total";
    const ID_SUMMARY_FILE: [&str; 3] = [
        "udm.tray.summary_file_0",
        "udm.tray.summary_file_1",
        "udm.tray.summary_file_2",
    ];
    pub const DEFAULT_TOOLTIP: &str = "UDM \u{2014} Universal Download Manager";
    const ID_POPUP: &str = "udm.tray.popup";

    impl Tray {
        pub fn new(vi: bool) -> Option<Tray> {
            let icon = load_icon()?;
            let show_hide =
                MenuItem::with_id(ID_SHOW_HIDE, text(vi, "Hide UDM", "Ẩn UDM"), true, None);
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
            let summary_total = MenuItem::with_id(ID_SUMMARY_TOTAL, "", false, None);
            let summary_files: Vec<MenuItem> = ID_SUMMARY_FILE
                .iter()
                .map(|id| MenuItem::with_id(*id, "", false, None))
                .collect();
            let mut top: Vec<&dyn tray_icon::menu::IsMenuItem> = vec![&summary_total];
            for f in &summary_files {
                top.push(f);
            }
            let summary_sep = PredefinedMenuItem::separator();
            top.push(&summary_sep);
            let popup_item = MenuItem::with_id(
                ID_POPUP,
                text(
                    vi,
                    "Active downloads\u{2026}",
                    "T\u{1ea3}i \u{0111}ang ch\u{1ea1}y\u{2026}",
                ),
                true,
                None,
            );
            let exit = MenuItem::with_id(ID_EXIT, text(vi, "Exit", "Thoát"), true, None);
            let menu = Menu::new();
            menu.append_items(&top).ok()?;
            menu.append_items(&[
                &popup_item,
                &PredefinedMenuItem::separator(),
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
                summary_total,
                summary_files,
                popup_item,
                pause_all,
                resume_all,
                exit,
                visible: true,
                vi,
                summary_cache: String::new(),
            })
        }

        /// Drain pending tray/menu events without blocking. Called from
        /// the GUI poll timer, so this runs on the event-loop thread.
        pub fn drain(&self) -> Vec<TrayAction> {
            let mut actions = Vec::new();
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                let action = if event.id() == self.show_hide.id() {
                    TrayAction::ToggleShowHide
                } else if event.id() == self.popup_item.id() {
                    TrayAction::ShowPopup
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
                    // Single click stays neutral by owner ruling (G3 C1);
                    // only a double click raises the main window.
                    TrayIconEvent::DoubleClick { .. } => actions.push(TrayAction::Show),
                    _ => {}
                }
            }
            actions
        }

        /// G3 summary block: set the totals line, up to three active-file
        /// lines (missing lines are blanked) and the tooltip (`None`
        /// restores the static product tooltip). Identical content is a
        /// no-op, so callers may invoke this on every poll tick; state.rs
        /// additionally throttles to about one update per second.
        pub fn set_summary(&mut self, total: &str, files: &[String], tooltip: Option<&str>) {
            let mut key = String::from(total);
            for f in files {
                key.push('\n');
                key.push_str(f);
            }
            key.push('|');
            key.push_str(tooltip.unwrap_or(""));
            if key == self.summary_cache {
                return;
            }
            self.summary_cache = key;
            self.summary_total.set_text(total);
            for (i, item) in self.summary_files.iter().enumerate() {
                item.set_text(files.get(i).map(String::as_str).unwrap_or(""));
            }
            let _ = self
                ._icon
                .set_tooltip(Some(tooltip.unwrap_or(DEFAULT_TOOLTIP)));
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

    /// Bottom-right anchor of the primary work area for a popup of the
    /// given physical size, with a small margin - the tray corner.
    pub fn work_area_bottom_right(width: i32, height: i32) -> (i32, i32) {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};
        let mut area = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                &mut area as *mut RECT as *mut core::ffi::c_void,
                0,
            );
        }
        (area.right - width - 12, area.bottom - height - 12)
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

        pub fn set_summary(&mut self, _total: &str, _files: &[String], _tooltip: Option<&str>) {}
    }

    pub fn work_area_bottom_right(_width: i32, _height: i32) -> (i32, i32) {
        (100, 100)
    }
}

pub use imp::{work_area_bottom_right, Tray};
