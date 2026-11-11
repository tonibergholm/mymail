<img src="assets/icon.svg" width="64" height="64" alt="">

# MyMail

A small, fast Gmail client for macOS and Linux (Omarchy / Wayland), written in
Rust with [GPUI](https://gpui.rs) and [GPUI Kit](https://gpui-kit.com).

It does five things: search, read, reply, forward, archive. It works with
Gmail and Google Workspace accounts, and you can sign in to several.

## Keys

| Key | Action |
|-----|--------|
| `j` / `k` (or arrows) | Next / previous message |
| `e` | Archive |
| `r` | Reply |
| `f` | Forward |
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

5. Run the app and click **+ Account**. Sign in from the browser window that
   opens. Repeat for each account.

The app requests one scope, `gmail.modify` (read, label, send). Refresh tokens
are stored in `accounts.json` in the same directory, with mode 0600.

## Install

```sh
./scripts/install.sh
```

On macOS this builds `MyMail.app` and copies it to `/Applications`. On Linux it
installs `~/.local/bin/mymail`, an icon, and a desktop entry, so MyMail shows up
in your launcher (Walker on Omarchy).

To run without installing: `cargo run --release`.

Linux needs the usual GPUI build dependencies (Vulkan, Wayland or X11,
xkbcommon, fontconfig). See the
[Zed Linux build guide](https://github.com/zed-industries/zed/blob/main/docs/src/development/linux.md).

## License

MIT
