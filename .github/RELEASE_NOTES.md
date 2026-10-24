Beta build for testing. Expect rough edges, and please report what breaks.

## What's new

- **Reorganized home and settings:** the home screen is now split into a
  connect pane and an address book, with settings on their own screen behind a
  gear icon. The address book shows your saved devices as tiles or a list
  (toggle between them) with online status.
- **Save a device's password:** tick **Remember password for this device** when
  connecting to reconnect in one click next time. Saved passwords live in the
  OS keyring, never in plain text.
- **Share an invite link:** **Copy invite** puts a link on your clipboard that
  fills in your ID for the other person; pasting one into the ID box connects
  straight to it.
- **Session recording:** record a session to a local WebM file, browse past
  recordings, and cap how much disk space they use (settings → Recordings).
- **Wake-on-LAN (new):** save a device's MAC address to wake it over the local
  network before you connect, from the **Wake** button in the address book.
- **Clearer permissions:** the hosting permissions setup was reworked, and the
  home screen now warns up front when screen recording or accessibility is
  missing so hosting does not silently fail.

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
- **Wake-on-LAN:** new; test waking a saved device on the same network.
- **Hardware H.264 on Linux (VAAPI):** experimental and off by default. Build
  host-core with `--features vaapi` (needs the ffmpeg development libraries and
  a VAAPI driver); it falls back to software if the encoder cannot start.
- **Port forwarding:** test reaching a local service on the remote.
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
