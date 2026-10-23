# OpenDesk releases

Releases are unsigned Tauri bundles for macOS, Linux, and Windows.

## Cut a release

1. Once: set the repo variable `OPENDESK_DEFAULT_RELAY` (Settings → Secrets
   and variables → Actions → Variables) to the public relay, e.g.
   `wss://relay.example.com/ws`. It becomes the app's default relay; the
   release fails without it.
2. Once: add the updater signing key as repo secrets (see
   [Auto-update](#auto-update)). Without it releases still build, but
   installed apps won't be offered them.
3. Push a version tag `vX.Y.Z` or `vX.Y.Z-pre.N`, for example
   `git tag v0.1.0-beta.5 && git push origin v0.1.0-beta.5`. A tag with a
   suffix is published as a pre-release. The workflow sets the app version
   from the tag; leave `version` in `tauri.conf.json` at the plain `0.1.0`
   (local builds use it). MSI only takes numeric versions, so the workflow
   also sets its version to `0.1.0.5` for `-beta.5`.
4. The `Release unsigned bundles` workflow builds all three platforms in parallel.
5. When the builds finish, the workflow creates the GitHub release with
   `.github/RELEASE_NOTES.md` (install steps for unsigned builds) plus the
   generated changelog, attaches the installers, and points the updater
   manifest at the new release.

The workflow builds from `viewer` and uploads only these files:

- macOS: `.dmg` (Apple silicon, macOS 12.3+), plus the updater's
  `.app.tar.gz` and its `.sig`
- Linux: AppImage, `.deb`, and `.rpm`, each with a `.sig`
- Windows: NSIS `.exe` and MSI, each with a `.sig`

## Auto-update

The app checks for a new release at startup and every six hours using
[tauri-plugin-updater](https://v2.tauri.app/plugin/updater/). When one is
available, the home screen shows a small banner; nothing appears during a
session. "Install and restart" downloads the bundle, checks its signature
against the public key in `tauri.conf.json` (`plugins.updater.pubkey`),
installs it, and relaunches. Failures are logged to the console and the
banner offers a retry.

The app reads one URL:

    https://github.com/treeot/opendesk/releases/download/updater/latest.json

GitHub's `/releases/latest/download/` link skips pre-releases, and every beta
is one, so it can't be used. Instead, the workflow keeps `latest.json` on a
separate pre-release tagged `updater` and replaces it on each publish
(`.github/scripts/updater-manifest.mjs` writes it). The first release run
creates that release. It's a plain file download, so there's no API rate
limit and no token in the app. The manifest lists the files of the
versioned release, so all channels see the newest tag, beta or not. A
re-run of an older tag never replaces a newer manifest. Don't turn on
immutable releases for the repo, or the manifest can't be replaced.

### Signing key

Updates are signed with a minisign key (this is not OS code signing). The
private key lives outside the repo at `~/.tauri/opendesk-updater.key` and
has an empty password. Add it as a repo secret once:

    gh secret set TAURI_SIGNING_PRIVATE_KEY --repo treeot/opendesk < ~/.tauri/opendesk-updater.key

`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` is only needed if you regenerate the key
with a password. If the key secret is missing, the build logs a warning,
skips the updater files, and leaves the manifest alone.

Keep a backup of the key. If it's lost, generate a new pair
(`npx tauri signer generate -w ~/.tauri/opendesk-updater.key`), put the new
public key in `tauri.conf.json`, and have everyone install the next release
by hand: installed apps only trust the old key.

Local `npm run tauri build` fails once the updater is configured unless the
key is set. Either export it
(`TAURI_SIGNING_PRIVATE_KEY=~/.tauri/opendesk-updater.key TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""`)
or skip updater files with
`npm run tauri -- build --config '{"bundle":{"createUpdaterArtifacts":false}}'`.

### Platform notes

- macOS: the updater replaces `/Applications/OpenDesk.app` in place. The updater
  doesn't set the quarantine flag, so there should be no Gatekeeper prompt
  after an update. Because the app is only ad-hoc signed, every build has a different
  signature, and macOS ties Screen Recording and Accessibility grants to it.
  After an update the switches may still look on in System Settings but no
  longer apply. The permissions screen opens at launch when that happens;
  if granting doesn't stick, remove OpenDesk from both lists (or run
  `tccutil reset ScreenCapture com.opendesk.app` and
  `tccutil reset Accessibility com.opendesk.app`) and grant again. The same
  happens with a manual reinstall; only a Developer ID signature avoids it.
- Windows: the update runs the NSIS or MSI installer (whichever was used to
  install) in passive mode, which shows a progress bar and restarts the app.
- Linux: only the AppImage updates itself. `.deb` and `.rpm` installs ask
  for an administrator password through the system's polkit prompt.
- Apps installed before auto-update shipped (up to `v0.1.0-beta.4`) need one
  manual install of a newer release.

## Unsigned builds

There's no paid code signing or notarization on any platform. macOS builds
are ad-hoc signed (`signingIdentity: "-"` in `tauri.conf.json`), which turns
Gatekeeper's "damaged" error into a "can't verify the developer" prompt that
users can approve. Use these builds for testing, or distribute them to users who
accept the OS trust warnings. They carry no verified publisher identity.

- macOS may show Gatekeeper warnings. In Finder, use Control-click → Open, or approve the app in System Settings → Privacy & Security.
- Windows may show a SmartScreen warning. Use More info → Run anyway only if you trust the release source.
- Linux desktop environments may warn about an untrusted downloaded file. Mark the AppImage executable (`chmod +x OpenDesk*.AppImage`) and launch it, or install the `.deb` with the system package installer.

Optionally, macOS releases can be signed with a stable self-signed certificate
so users' Screen Recording and Accessibility grants survive updates (TCC keys on
the certificate, not the per-build hash; Gatekeeper still warns). Set the repo
secrets `MACOS_SIGN_CERT` (base64 `.p12`), `MACOS_SIGN_CERT_PASSWORD`, and
`MACOS_SIGN_IDENTITY` (the certificate's name). Reuse the same certificate every
release. Unset, builds stay ad-hoc.
