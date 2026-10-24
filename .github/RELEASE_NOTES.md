Beta build for testing. Expect rough edges, and please report what breaks.

## What's new

- **Reach a service on the remote (new):** port forwarding sends a local port
  to the other computer's own `localhost` (for example its Ollama on 11434),
  so you can use `localhost:11434` here. Turn on **Port forwarding** in the
  host's permissions first, then use the **Forward** button in the toolbar.
- **Sharper video (new):** HEVC (H.265) is used when both computers support it,
  for a clearer picture at the same bandwidth, and falls back to H.264 on its
  own. A new **Quality** control in the toolbar caps the bitrate for slow links.
- **The pointer matches the remote:** it now shows the other computer's actual
  cursor (text beam, resize, hand) right away instead of waiting on the video.
- **Smoother to drive:** fullscreen fits your screen with no bars, the toolbar
  hides itself (reach the top edge to bring it back), and right-click and the
  occasional stuck key are fixed.
- **macOS menu bar** with the usual app, Edit, View, Window, and Help menus.
- **Security:** view-only now also blocks privacy mode and restart; the file
  prompt shows where a file will be saved; the relay is hardened against abuse;
  and dependencies were updated for new advisories.

If you installed an earlier beta, this one arrives as an "Update available"
banner on the home screen. After updating, macOS asks for Screen Recording and
Accessibility once more. From this build on, signed releases keep those grants
across future updates.

## Install

Pick the file for your system below. These builds are not code-signed, so
your OS will warn the first time you open them:

- **macOS** (`.dmg`, Apple silicon, macOS 12.3+): drag OpenDesk to
  Applications. The first time, right-click it and choose **Open** (or allow
  it in System Settings → Privacy & Security → **Open Anyway**). If macOS
  says it's damaged, run
  `xattr -dr com.apple.quarantine /Applications/OpenDesk.app` and open it
  again. Grant **Screen Recording** and **Accessibility** when asked, then
  restart the app.
- **Windows** (`.exe` or `.msi`): on "Windows protected your PC", click
  **More info → Run anyway**.
- **Linux** (`.AppImage`, `.deb`, or `.rpm`): for the AppImage,
  `chmod +x OpenDesk*.AppImage` and run it.

The app connects to the project's test relay out of the box. Nothing needs
port forwarding.

## Try it

1. Install on two computers and open OpenDesk on both.
2. On the one you want to control, note the **ID** and **session password**.
3. On the other, enter them and click **Connect**, then accept on the first.

The full checklist is in
[docs/TESTING.md](https://github.com/treeot/opendesk/blob/main/docs/TESTING.md).

## Needs testing

These features are new and haven't run on real hardware yet:

- **HEVC (macOS and Windows):** the H.265 path has been built but not yet
  decoded end to end on real hardware. If video looks wrong, it falls back to
  H.264; please report it. The browser viewer uses HEVC only where it can.
- **Port forwarding:** new; test reaching a local service on the remote.
- **Windows:** hardware video encoding (NVIDIA / Intel / AMD) and the remote's
  cursor shape. Check the log for `video encoder:` to see which encoder ran.
- **Linux:** hearing the host's system sound (PulseAudio / PipeWire).
- **macOS:** system sound needs macOS 14.6+; older versions send no audio.

## Report a problem

[Open an issue](https://github.com/treeot/opendesk/issues/new?template=bug_report.yml)
with your OS versions, what you did, and the app log
(macOS `~/Library/Logs/com.opendesk.app/`, Windows
`%LOCALAPPDATA%\com.opendesk.app\logs\`, Linux
`~/.local/share/com.opendesk.app/logs/`).
Report security problems privately (see
[SECURITY.md](https://github.com/treeot/opendesk/blob/main/docs/SECURITY.md)).
