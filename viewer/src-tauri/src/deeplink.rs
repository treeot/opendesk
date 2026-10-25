//! Deep links: `opendesk://connect?id=<9 digits>[&relay=...]`.
//!
//! An invite link (see `viewer/src/lib/invite.ts`) uses this custom scheme, so
//! opening one, from a browser or anywhere else, brings OpenDesk forward and
//! prefills the device ID to connect to. The link never carries a password, so
//! it only fills the ID in; the user still enters the password and the host
//! still approves.
//!
//! Delivery differs by how the app was reached:
//! - already running: the `on_open_url` handler (macOS) or the single-instance
//!   argv (Windows/Linux) fires, and we emit a `deep-link` event to the window;
//! - cold start: the id is stashed in [`PendingDeepLink`] and the frontend
//!   claims it once on load with the `take_deep_link` command.

use std::sync::Mutex;

use tauri::{AppHandle, Emitter};

/// Holds a device ID from a deep link that arrived before the frontend was
/// ready to receive it, delivered once via [`take_deep_link`].
#[derive(Default)]
pub(crate) struct PendingDeepLink(pub(crate) Mutex<Option<String>>);

/// Pull the 9-digit device ID out of an `opendesk://connect?id=...` link (or any
/// string carrying an `id=` query parameter). Returns `None` when there is no
/// usable ID, mirroring `parseInvite` on the frontend.
pub(crate) fn extract_invite_id(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let marker = text.find("id=")?;
    // Require `id=` to start the query or follow a `?`/`&`, not be part of a
    // longer key like "uuid=".
    if marker > 0 {
        let prev = bytes[marker - 1];
        if prev != b'?' && prev != b'&' {
            return find_after(text, marker + 1);
        }
    }
    let digits: String = text[marker + 3..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.len() == 9 {
        Some(digits)
    } else {
        // Keep scanning in case a later `id=` matches.
        find_after(text, marker + 1)
    }
}

/// Continue searching for an `id=` match from `from` onward.
fn find_after(text: &str, from: usize) -> Option<String> {
    text.get(from..).and_then(extract_invite_id)
}

/// Find the first `opendesk://` deep link among process arguments (how
/// Windows and Linux deliver one to an already-running instance).
pub(crate) fn id_from_args<I, S>(args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().find_map(|arg| {
        let arg = arg.as_ref();
        arg.starts_with("opendesk://").then(|| extract_invite_id(arg)).flatten()
    })
}

/// Deliver a deep-link device ID: bring the window forward, emit the event for a
/// running frontend, and stash it for a frontend that has not loaded yet.
pub(crate) fn deliver(app: &AppHandle, id: String) {
    use tauri::Manager;
    crate::startup::show_main_window(app);
    if let Some(state) = app.try_state::<PendingDeepLink>() {
        if let Ok(mut pending) = state.0.lock() {
            *pending = Some(id.clone());
        }
    }
    let _ = app.emit("deep-link", id);
}

/// The frontend claims a pending deep-link ID once on load. Clears it.
#[tauri::command]
pub(crate) fn take_deep_link(state: tauri::State<'_, PendingDeepLink>) -> Option<String> {
    state.0.lock().ok().and_then(|mut pending| pending.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_id_from_connect_link() {
        assert_eq!(extract_invite_id("opendesk://connect?id=123456789").as_deref(), Some("123456789"));
    }

    #[test]
    fn extracts_id_with_relay_after() {
        assert_eq!(
            extract_invite_id("opendesk://connect?id=905218437&relay=wss%3A%2F%2Fr").as_deref(),
            Some("905218437"),
        );
    }

    #[test]
    fn ignores_non_nine_digit_and_unrelated_keys() {
        assert_eq!(extract_invite_id("opendesk://connect?id=12345"), None);
        assert_eq!(extract_invite_id("opendesk://connect?uuid=123456789"), None);
        assert_eq!(extract_invite_id("opendesk://connect"), None);
    }

    #[test]
    fn finds_id_when_another_param_comes_first() {
        assert_eq!(
            extract_invite_id("opendesk://connect?relay=wss%3A%2F%2Fr&id=118273645").as_deref(),
            Some("118273645"),
        );
    }

    #[test]
    fn id_from_args_picks_the_scheme_argument() {
        let args = ["--background", "opendesk://connect?id=730514826"];
        assert_eq!(id_from_args(args).as_deref(), Some("730514826"));
        assert_eq!(id_from_args(["--background", "other"]), None);
    }
}
