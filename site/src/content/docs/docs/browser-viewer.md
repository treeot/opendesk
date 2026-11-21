---
title: Browser viewer
description: Control a computer running OpenDesk from a web browser, without installing anything. What it can do, what it can't, and what you're trusting.
---

The [browser viewer](/connect/) lets you control a computer from a browser,
for example on a borrowed laptop where you can't install anything. The
computer you connect to still needs the OpenDesk app to host.

## Connect

1. Open [opendesk.treeot.dev/connect/](/connect/).
2. Enter the **Device ID** and **Password** shown in OpenDesk on the other
   computer. If it asks for one, add the **2FA code**.
3. Click **Connect**, and wait for the other side to click **Allow** (unless
   it has [unattended access](/docs/unattended/) on).

The page shows its own **YOUR ID** at the top. That's the ID the host sees
in its approval dialog. The browser keeps it, so the host sees the same ID
next time.

Under **Relay** you can change which relay the page uses. It has to be the
same relay the host uses, and it must start with `wss://`.

## What works

- The remote screen, mouse and keyboard.
- The host's **sound**.
- **File transfer**, both ways, with a prompt before anything is accepted.
  Drop files anywhere on the screen to send them.
- **Remote files**: browse the host's folders, download a file, upload into
  the folder you're viewing, and create, rename or delete items.
- **Chat** with the person at the host.
- **Special keys** your browser keeps for itself, like Ctrl+Alt+Del,
  Alt+Tab and the Windows or Command key, from the **Keys** menu.
- **Clipboard** text sync, when you turn it on.
- **Quality**, **text readability** and **view only**, as in the app.
- **Recording** the remote screen to a `.webm` file.
- **Fullscreen** and choosing a monitor when the host has more than one.
- The relay round trip, next to the device ID.
- Reconnecting after a short network drop.

## What doesn't, yet

- **Always relayed.** Browsers can't join the direct UDP path, so the session
  always goes through the relay. It's still end-to-end encrypted, but
  usually slower than the app's direct connection.
- **It can't host.** You can control a computer from the browser, not share
  the browser's own screen.
- **Smaller uploads.** A browser holds each file in memory to send it, so
  one pick or drop is limited to 512 MB, and folders can't be sent.

It needs a browser with WebCodecs: a recent Chrome, Edge, Firefox or Safari.
In a browser without it, the page says so and the **Connect** button stays
off.

## What you're trusting

The encryption is the same Rust code the desktop app uses, compiled to
WebAssembly, so the relay still sees only sealed bytes.

The difference is where that code comes from. GitHub Pages serves the page
fresh on every visit, so each time you load it you're trusting GitHub and
this repository, not a binary you built or checked once. If that matters for
your setup, use the desktop app, or build the browser viewer yourself and
host it; the [README](https://github.com/treeot/opendesk#browser-viewer)
explains how.
