use tauri::{
    AppHandle, Emitter, Manager,
    menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem},
    tray::{TrayIconBuilder, TrayIconEvent, MouseButton, MouseButtonState},
    Runtime,
};
use tauri::image::Image;

pub fn create_tray_icon<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let show_item = MenuItemBuilder::with_id("show", "Show Goblin").build(app)?;
    let hide_item = MenuItemBuilder::with_id("hide", "Hide Goblin").build(app)?;
    let status_item = MenuItemBuilder::with_id("status", "Idle | deepseek-v4-pro").build(app)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItemBuilder::with_id("quit", "Quit Goblin").build(app)?;

    let menu = MenuBuilder::new(app)
        .items(&[
            &show_item,
            &hide_item,
            &status_item,
            &separator,
            &quit_item,
        ])
        .build()?;

    let icon_bytes = include_bytes!("../icons/32x32.png");
    let icon = Image::from_bytes(icon_bytes)?;

    let _tray = TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("Goblin AI Agent")
        .on_menu_event(move |app, event| {
            let id = event.id().as_ref();
            match id {
                "show" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
                "hide" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.hide();
                    }
                }
                "quit" => {
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    if window.is_visible().unwrap_or(false) {
                        let _ = window.hide();
                    } else {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
            }
        })
        .build(app)?;

    Ok(())
}

#[allow(dead_code)]
pub fn update_tray_status<R: Runtime>(app: &AppHandle<R>, status: &str) {
    if let Some(tray) = app.tray_by_id("main-tray") {
        let _status_item = MenuItemBuilder::with_id("status", status)
            .build(app)
            .ok();
        let _ = app.emit("tray-status-update", serde_json::json!({ "status": status }));
        drop(tray);
    }
}
