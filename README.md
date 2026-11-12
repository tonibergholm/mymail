<img src="assets/icon.svg" width="64" height="64" alt="">

# MyMail

A small, fast Gmail client for macOS and Linux (Omarchy / Wayland), written in
Rust with [GPUI](https://gpui.rs) and [GPUI Kit](https://gpui-kit.com).

It does six things: search, read, write, reply, forward, archive, with
attachments both ways. It works with
Gmail and Google Workspace accounts, and you can sign in to several.

Click an attachment to save it to Downloads and open it. In the composer,
**Attach…** adds files (up to Gmail's 25 MB), and Forward keeps the original
message's attachments.

The inbox checks for new mail every 60 seconds; a check that finds nothing new
costs one small request. Messages you have opened are cached on disk, so they
reopen instantly and the last inbox shows right away at startup.

## Keys

| Key | Action |
|-----|--------|
| `j` / `k` (or arrows) | Next / previous message |
| `e` | Archive |
| `r` | Reply |
| `f` | Forward |
| `c` | New message |
| `/` | Search (Gmail syntax, e.g. `from:alice is:unread`) |

An empty search shows the inbox.

## Setup

Gmail needs an OAuth client of your own:

1. In [Google Cloud Console](https://console.cloud.google.com/), create a
   project and enable the **Gmail API**.
2. Configure the OAuth consent screen. For personal use, leave it in
   *Testing* and add your addresses as test users. For a Workspace domain
   you can choose *Internal* instead.
3. Create credentials: **OAuth client ID → Desktop app**.
4. Save the ID and secret to `client.json` in the config directory
   (`~/Library/Application Support/mymail/` on macOS, `~/.config/mymail/`
   on Linux):

   ```json
   { "client_id": "...apps.googleusercontent.com", "client_secret": "..." }
   ```

   Or set `MYMAIL_CLIENT_ID` and `MYMAIL_CLIENT_SECRET`.

5. Run the app and choose **Add account…** from the account menu. Sign in from the browser window that
   opens. Repeat for each account.

The app requests one scope, `gmail.modify` (read, label, send). Refresh tokens
are stored in `accounts.json` in the same directory, with mode 0600.

## Settings

Optional `settings.json` in the same directory:

```json
{ "poll_secs": 60 }
```

`poll_secs` is how often the inbox checks for new mail; `0` turns it off.

```json
{ "poll_secs": 60, "auto_update": true }
```

`auto_update` (default `true`) lets MyMail install new releases by itself. Set it
to `false` to turn that off; changes apply the next time the app starts.

Cached mail lives in `~/Library/Caches/mymail` (macOS) or `~/.cache/mymail`
(Linux), readable only by you. Entries older than 30 days are removed at
startup. Delete the folder to clear it.

## Install

```sh
./scripts/install.sh
```

On macOS this builds `MyMail.app` and copies it to `/Applications`. On Linux it
installs `~/.local/bin/mymail`, an icon, and a desktop entry, so MyMail shows up
in your launcher (Walker on Omarchy).

To run without installing: `cargo run --release`.

## Updates and releases

The window title shows the running version (`MyMail v0.2.0`), and so does
`mymail --version`.

An installed build (`/Applications/MyMail.app` on macOS, `~/.local/bin/mymail`
on Linux) checks GitHub Releases shortly after it starts and every six hours.
When a newer release exists, it downloads it, checks the SHA-256 digest GitHub
publishes for the file, and replaces itself. Then it restarts into the new
version, or waits until you close an open draft. Builds run with `cargo run` or
from `target/` never update themselves. The macOS release is Apple Silicon only,
and the Linux one is x86_64.

Releases are not notarized (the app is ad-hoc signed). The first install from a
browser download may need right-click, Open, and a one-time approval in System
Settings. Updates the app installs itself don't go through that prompt.

To publish a release: bump `version` in `Cargo.toml` (and `Cargo.lock`), merge,
then push a matching tag.

```sh
git tag v0.3.0 && git push origin v0.3.0
```

The Release workflow builds both platforms and creates the GitHub release.

Linux needs the usual GPUI build dependencies (Vulkan, Wayland or X11,
xkbcommon, fontconfig). See the
[Zed Linux build guide](https://github.com/zed-industries/zed/blob/main/docs/src/development/linux.md).

## License

MIT
