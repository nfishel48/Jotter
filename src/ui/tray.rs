use tray_icon::{
    Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem},
};

/// What the user asked for via the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    ToggleRecord,
    ShowSettings,
    Quit,
}

impl MenuAction {
    /// The menu item's id, as reported to telemetry.
    ///
    /// Matches the string ids the items are built with below, so a chart of tray
    /// usage reads the same as the menu.
    pub fn telemetry_id(self) -> &'static str {
        match self {
            Self::ToggleRecord => "record",
            Self::ShowSettings => "settings",
            Self::Quit => "quit",
        }
    }
}

/// The tray icon plus the handles needed to mutate it later.
///
/// `record_item` is kept so its label can be flipped between "Start" and "Stop"
/// — the menu is the only recording UI visible when the settings window is
/// hidden, which is its normal state.
pub struct Tray {
    pub _icon: TrayIcon,
    pub record_item: MenuItem,
}

impl Tray {
    pub fn set_recording(&self, recording: bool) {
        self.record_item.set_text(if recording {
            "Stop Recording"
        } else {
            "Start Recording"
        });
    }
}

/// Returns true if the icon was left-clicked (i.e. show the window).
pub fn handle_icon_events() -> bool {
    let mut show = false;
    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            ..
        } = event
        {
            show = true;
        }
    }
    show
}

/// Drain every pending menu event.
///
/// Returns all of them rather than just the last: dropping a Quit because a
/// ShowSettings arrived in the same poll would be a real bug.
pub fn handle_menu_events() -> Vec<MenuAction> {
    let mut actions = Vec::new();
    while let Ok(event) = MenuEvent::receiver().try_recv() {
        match event.id.0.as_str() {
            "record" => actions.push(MenuAction::ToggleRecord),
            "settings" => actions.push(MenuAction::ShowSettings),
            "quit" => actions.push(MenuAction::Quit),
            _ => {}
        }
    }
    actions
}

pub fn build_tray(icon: Icon) -> Tray {
    let menu = Menu::new();
    let record_item = MenuItem::with_id("record", "Start Recording", true, None);
    menu.append(&record_item).unwrap();
    menu.append(&MenuItem::with_id("settings", "Settings", true, None))
        .unwrap();
    menu.append(&MenuItem::with_id("quit", "Quit", true, None))
        .unwrap();

    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_tooltip("Jotter")
        .build()
        .expect("failed to build tray icon");

    Tray {
        _icon: tray,
        record_item,
    }
}

/// The tray icon, compiled into the binary.
///
/// Embedded rather than read from disk because there is no layout that finds it
/// on every platform: a macOS .app has it in Contents/Resources, a Linux
/// install has the binary in /usr/bin with the asset somewhere else entirely,
/// and a bare `cargo run` has only the repo. Reading it at runtime meant the
/// tray panicked wherever the guess was wrong.
const ICON_PNG: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon.png"));

pub fn load_icon() -> Icon {
    // Both expects are unreachable unless assets/icon.png is itself broken,
    // which `cargo build` would have to have accepted first.
    let image = image::load_from_memory(ICON_PNG)
        .expect("failed to decode embedded icon")
        .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).expect("failed to create icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `load_icon` is otherwise only reachable from a running tray, so a broken
    /// or truncated asset would surface as a panic at launch on a user's
    /// machine rather than in CI.
    #[test]
    fn embedded_icon_decodes() {
        let image = image::load_from_memory(ICON_PNG)
            .expect("embedded icon is not a decodable image")
            .into_rgba8();
        assert_eq!(image.dimensions(), (64, 64));
        assert!(Icon::from_rgba(image.into_raw(), 64, 64).is_ok());
    }
}
