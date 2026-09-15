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

pub fn load_icon(path: &std::path::Path) -> Icon {
    let image = image::open(path)
        .expect("failed to open icon path")
        .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).expect("failed to create icon")
}
