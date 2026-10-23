//! The macOS application menu bar.
//!
//! Custom items emit a [`MENU_EVENT`] carrying their id; the web UI listens and
//! acts on it (see `viewer/src/lib/api.ts`). The Session submenu's items stay
//! disabled until a session is live, which the UI drives through
//! [`set_session`] (the `set_session_menu` command). Only macOS builds a menu;
//! elsewhere [`set_session`] is a no-op because the state is never managed.

use tauri::menu::{CheckMenuItem, MenuItem};
use tauri::{AppHandle, Emitter, Manager};

/// Event name carrying the clicked item's id to the web UI.
pub const MENU_EVENT: &str = "menu";

/// Item ids shared with the front end.
pub mod id {
    pub const SETTINGS: &str = "settings";
    pub const FULLSCREEN: &str = "fullscreen";
    pub const DISCONNECT: &str = "disconnect";
    pub const ADD_SCREEN: &str = "add_screen";
    pub const CLIPBOARD_SYNC: &str = "clipboard_sync";
    pub const RECORD: &str = "record";
    pub const DOCS: &str = "docs";
    pub const REPORT_ISSUE: &str = "report_issue";
}

const DOCS_URL: &str = "https://opendesk.treeot.dev/docs/";
const ISSUE_URL: &str = "https://github.com/treeot/opendesk/issues/new";

/// The Session items that enable and disable as sessions come and go.
struct SessionItems {
    add_screen: MenuItem<tauri::Wry>,
    clipboard_sync: CheckMenuItem<tauri::Wry>,
    record: MenuItem<tauri::Wry>,
    disconnect: MenuItem<tauri::Wry>,
}

/// Build the app menu and install it. macOS only; other platforms keep the
/// default menu and never manage [`SessionItems`].
#[cfg(target_os = "macos")]
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{AboutMetadata, CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, SubmenuBuilder};

    let app_menu = SubmenuBuilder::new(app, "OpenDesk")
        .about(Some(AboutMetadata::default()))
        .separator()
        .item(&MenuItemBuilder::with_id(id::SETTINGS, "Settings…").accelerator("Cmd+,").build(app)?)
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;

    let edit_menu = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;

    let view_menu = SubmenuBuilder::new(app, "View")
        .item(&MenuItemBuilder::with_id(id::FULLSCREEN, "Enter Full Screen").accelerator("Cmd+Ctrl+F").build(app)?)
        .build()?;

    let add_screen = MenuItemBuilder::with_id(id::ADD_SCREEN, "Add Screen").enabled(false).build(app)?;
    let clipboard_sync = CheckMenuItemBuilder::with_id(id::CLIPBOARD_SYNC, "Clipboard Sync").enabled(false).build(app)?;
    let record = MenuItemBuilder::with_id(id::RECORD, "Record").enabled(false).build(app)?;
    let disconnect = MenuItemBuilder::with_id(id::DISCONNECT, "Disconnect").accelerator("Cmd+Shift+D").enabled(false).build(app)?;
    let session_menu = SubmenuBuilder::new(app, "Session")
        .item(&add_screen)
        .item(&clipboard_sync)
        .item(&record)
        .separator()
        .item(&disconnect)
        .build()?;

    let window_menu = SubmenuBuilder::new(app, "Window").minimize().separator().close_window().build()?;

    let help_menu = SubmenuBuilder::new(app, "Help")
        .item(&MenuItemBuilder::with_id(id::DOCS, "OpenDesk Documentation").build(app)?)
        .item(&MenuItemBuilder::with_id(id::REPORT_ISSUE, "Report an Issue…").build(app)?)
        .build()?;

    let menu = MenuBuilder::new(app)
        .items(&[&app_menu, &edit_menu, &view_menu, &session_menu, &window_menu, &help_menu])
        .build()?;
    app.set_menu(menu)?;
    app.manage(SessionItems { add_screen, clipboard_sync, record, disconnect });
    Ok(())
}

/// Route a menu click: open help links here, forward everything else to the UI.
pub fn on_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    match event.id().as_ref() {
        id::DOCS => open_url(app, DOCS_URL),
        id::REPORT_ISSUE => open_url(app, ISSUE_URL),
        other => {
            if let Err(error) = app.emit(MENU_EVENT, other) {
                log::error!("could not forward menu event {other}: {error}");
            }
        }
    }
}

fn open_url(app: &AppHandle, url: &str) {
    use tauri_plugin_opener::OpenerExt;
    if let Err(error) = app.opener().open_url(url, None::<&str>) {
        log::error!("could not open {url}: {error}");
    }
}

/// Enable or disable the Session items and reflect clipboard/recording state.
pub fn set_session(app: &AppHandle, active: bool, clipboard_sync: bool, recording: bool) {
    let Some(items) = app.try_state::<SessionItems>() else { return };
    let _ = items.add_screen.set_enabled(active);
    let _ = items.clipboard_sync.set_enabled(active);
    let _ = items.clipboard_sync.set_checked(clipboard_sync);
    let _ = items.record.set_enabled(active);
    let _ = items.record.set_text(if recording { "Stop Recording" } else { "Record" });
    let _ = items.disconnect.set_enabled(active);
}
