# Manual testing

Automated tests cover the protocol, relay, and host/viewer sessions end to end
(`cargo test --workspace`). Screen capture, input injection, and audio need a
real desktop session, so test them by hand before a release.

## Keeping macOS permissions across rebuilds

macOS ties Screen Recording and Accessibility grants to the app's code
signature. `npm run tauri dev` and the default ad-hoc signing
(`signingIdentity: "-"`) change identity on every rebuild, so the permissions
reset and the Permissions screen shows them as required again. To make the
grants persist, sign local builds with a stable self-signed certificate:

1. Create the certificate once, in Keychain Access: Keychain Access menu →
   Certificate Assistant → Create a Certificate. Name it `OpenDesk Dev`,
   Identity Type "Self Signed Root", Certificate Type "Code Signing". Create it.
2. Build and run a signed debug app (not `tauri dev`), picking up the cert:

   ```
   cd viewer
   APPLE_SIGNING_IDENTITY="OpenDesk Dev" npm run app
   ```

   This builds `src-tauri/target/debug/bundle/macos/OpenDesk.app`, signs it with
   the cert, and opens it. Grant Screen Recording and Accessibility once; later
   rebuilds signed with the same cert keep them.

The committed `signingIdentity` stays `-` so CI and releases are unaffected; the
env var only changes your local build. This is for persistent permissions, not
distribution (that still needs real signing and notarization).

## One machine (two instances)

Debug builds read `OPENDESK_DATA_DIR`, so a second instance gets its own
identity and config.

1. Relay: `cargo run -p relay` (listens on `ws://127.0.0.1:8080/ws`, the
   default relay URL).
2. Instance A (host): `cd viewer && npm run tauri dev`. It starts hosting on
   launch; note the 9-digit ID and session password on the home screen.
3. Instance B (viewer), in another terminal, while A's dev server is running:
   `OPENDESK_DATA_DIR=/tmp/opendesk-b viewer/src-tauri/target/debug/viewer`
4. In B, enter A's ID and password, then accept on A.

On one machine you are viewing your own screen, so the stream shows itself
recursively. That's expected.

## Two machines

Run the relay where both machines can reach it, then point both apps at it
(home screen → **Relay regions**, e.g. `ws://<relay-ip>:8080/ws` on a LAN or your
`wss://` deployment).

| # | Step | Expected |
|---|------|----------|
| 1 | Open the app on both. Viewer: enter host ID + password, Connect | Host shows accept dialog |
| 2 | Accept | Viewer shows the host screen within a few seconds; the toolbar shows Direct or Relayed and a round-trip time |
| 3 | Move mouse, click, type (incl. symbols like `@#{}`) | Host reacts; characters match |
| 4 | Turn clipboard sync on in the toolbar (it starts off), copy text on each side | Clipboard syncs both ways |
| 5 | Send a file both ways | Consent prompt, file arrives, size matches |
| 6 | Switch monitor (if host has 2+) | Stream switches |
| 7 | Play a video or song on the host | Viewer hears it; mute works. macOS asks for System Audio Recording once. macOS < 14.6 and Linux without PulseAudio/PipeWire send no audio |
| 8 | Wrong password | Rejected, viewer does **not** retry |
| 9 | Click Disconnect on the host | Viewer shows "The host ended the session." and doesn't reconnect |
| 10 | Kill host network for 10 s, restore | Viewer reconnects; host re-registers. With the network gone for good, both sides notice within ~45 s |
| 11 | Unattended on, then 2FA (scan the QR, confirm a code) | Connects without accept; wrong code rejected. A device on the Known devices list still gets the accept prompt |
| 12 | Record session, stop | "Recording saved to …" appears; the `.webm` plays |
| 13 | With unattended on, tick **Start OpenDesk when I sign in**, then sign out and back in | No window opens; a tray icon appears and the viewer can connect without accept. Closing the window keeps hosting; tray **Quit** stops it |
| 14 | Linux only: in a session with no keyring running (no GNOME Keyring or KWallet), save unattended access | Saving fails with "couldn't … system keyring" and a hint; if it was already on, the card says it isn't working and connections need accept |
| 15 | Copy the host's `config.json` to the other machine, restart the app there | It shows a different ID from the host; both stay online |
| 16 | On the host, **Get a new ID**, confirm | The ID changes and the host is online under it; the old ID no longer connects |
| 17 | Mac host: in the viewer's monitor menu pick **Add display…** | The host shows "The viewer added a virtual display"; the viewer switches to a new display about the size of its window, and clicks land where they should on it |
| 18 | Pick **Add display…** again, then once more | A second display is added; the third try says the session can't add more |
| 19 | With a virtual display shown, click **Remove**, then end the session with another still added | The stream goes back to the main display and the removed one disappears; after Disconnect the host has no extra displays left (System Settings → Displays) |
| 20 | Untick **Let viewers add virtual displays** on the host, reconnect | The viewer's monitor menu has no **Add display…** |
| 21 | Headless host (no monitor or dummy plug), then connect. Mac: with the setting on, and again with it off | Setting on: the viewer sees a 1920x1080 virtual display. Off, or a Windows host without the driver: the viewer stops with "That computer has no screen connected…" instead of the Screen Recording message |
| 22 | Windows host: on Home click **Install virtual display driver**, read the licence, **Accept and install**, say No at the Windows prompt; then again and say Yes | No: "Nothing was changed…". Yes: "The virtual display driver is installed." and Device Manager → Display adapters lists "USB Mobile Monitor Virtual Display"; Home now offers **Uninstall driver** |
| 23 | Windows host with the driver: rows 17 to 20 | As on a Mac, except sizes snap to the nearest of the driver's modes (1280x720 … 3840x2160) and the monitor menu shows the real size; Settings → Display shows the new displays while added and none after Disconnect |
| 24 | Windows host: add two displays, then **Remove** the first | "Remove the newest virtual display first…"; the stream stays on it. Removing the second, then the first, works |
| 25 | Windows host: add one display yourself first (`deviceinstaller64 enableidd 1` from Amyuni's zip), then connect and add and remove one, and Disconnect | Yours stays throughout; OpenDesk's comes and goes |
| 26 | Windows host: add a display, then quit OpenDesk from the tray | The display goes away with the app |
| 27 | Headless Windows host (no monitor or dummy plug) with the driver and the setting on | The viewer sees a 1920x1080 virtual display; after Disconnect it is gone |
| 28 | Windows host: **Uninstall driver** | It disappears from Device Manager; Home offers **Install virtual display driver** again |

`cargo run -p host-core --example virtual_display` on a Mac adds a 1280x720
display and a 2560x1440 HiDPI one for two seconds each and removes them. On
Windows (with the driver) it adds the same two, checks the older one can't
be removed first, and removes both.

macOS asks for **Screen Recording** and **Accessibility** permission the first
time; restart the app after granting them. When you report an issue, include
the OS versions, the number of any failing step, and the app log:

- macOS: `~/Library/Logs/com.opendesk.app/`
- Windows: `%LOCALAPPDATA%\com.opendesk.app\logs\`
- Linux: `~/.local/share/com.opendesk.app/logs/`

The relay logs to stderr; set `RUST_LOG=debug` for more detail.
# Windows sign-in-screen service

The privileged path needs a real Windows VM and an administrator account.

1. Start with the setting off; confirm the normal host starts after sign-in.
2. Save unattended access (including 2FA if it is part of the test), enable
   “Run OpenDesk at the Windows sign-in screen”, and approve the UAC prompt.
3. Confirm Home reports “Hosted by the OpenDesk service” and that a second
   normal host is not registered.
4. Lock Windows, sign out, show a UAC prompt, and reboot. From another viewer,
   verify capture and keyboard/mouse input on each protected desktop.
5. Use Ctrl+Alt+Del, verify the Windows security screen appears, and complete
   an unattended sign-in including the 2FA code.
6. Disable the setting, verify the service is stopped and removed, and confirm
   the ordinary host resumes after the next sign-in.
