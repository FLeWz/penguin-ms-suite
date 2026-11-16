# penguin-ms-suite

Native Linux Microsoft 365 client. Rust + GTK4/libadwaita, no embedded browser.
Thin sidebar: Outlook, Teams, Calendar, Excel. Data via Microsoft Graph.

## Features
- Outlook: inbox (latest 50), read message bodies.
- Teams: chat list, read/send messages, emoji picker, link previews, send attachments (attach button, drag and drop, or paste files and images; small images go inline, other files upload to OneDrive and are shared with the chat members only), photo and video previews (click a photo for full size; videos download to `~/.cache/penguin-ms-suite/media`, pruned after 7 days, and play in-app or in the default player).
- Calendar: month picker, events for selected day (local time).
- Excel: .xlsx files from OneDrive, pick sheet, view used range (first 100 rows x 26 cols). Read-only.

## Azure setup (one time)
1. Entra ID > App registrations > New registration. Account type as needed.
2. Authentication > Allow public client flows = Yes; add platform "Mobile and desktop applications" with redirect URI `http://localhost`.
3. API permissions (delegated, Microsoft Graph): User.Read, Mail.Read,
   Calendars.Read, Chat.ReadWrite, Files.Read. (Admin consent may be needed.)
4. Put the IDs in `.env` (see below). They are compiled into the binary. Runtime override:
   - env `PENGUIN_MS_CLIENT_ID` / `PENGUIN_MS_TENANT`, or
   - `~/.config/penguin-ms-suite/config.json`: `{"client_id": "...", "tenant": "common"}`

## Configuration (.env)
The Application (client) ID and Directory (tenant) ID are read at build time
and compiled into the binary; they are not in the source. `.env` is gitignored.

    cp .env.example .env      # then edit the two values
    cargo build --release

Variables: `PENGUIN_MS_CLIENT_ID`, `PENGUIN_MS_TENANT`. The build fails if they
are missing. Real environment variables take precedence over `.env`.
Distribute the built binary, not `.env`.

## Permissions (Microsoft Graph, delegated)
Added to the app registration. "Admin consent required" is as shown in the Entra portal.

| Area | Permission | Admin consent required |
|---|---|---|
| Profile | User.Read | No |
| Profile | User.ReadBasic.All | No |
| Mail | Mail.Read, Mail.ReadWrite, Mail.Send | No |
| Mail | Mail.ReadWrite.Shared | No |
| Mail | MailboxSettings.ReadWrite | No |
| Calendar | Calendars.Read, Calendars.ReadWrite | No |
| Calendar | Calendars.ReadWrite.Shared | No |
| Meetings | OnlineMeetings.ReadWrite | No |
| Teams chat | Chat.ReadWrite | No |
| Teams | Team.ReadBasic.All, Channel.ReadBasic.All | No |
| Teams | ChannelMessage.Send | No |
| Teams | ChannelMessage.Read.All | **Yes** (not granted for SkyLabs d.o.o.) |
| Teams | Presence.ReadWrite | No |
| Files | Files.Read, Files.ReadWrite | No |
| Files | Files.ReadWrite.All, Sites.ReadWrite.All | No |
| Contacts | Contacts.ReadWrite | No |

Notes:
- `ChannelMessage.Read.All` is the only permission flagged as needing admin consent, and it is not granted. Do not request it until an admin approves it.
- The portal says "No" for the others, but in practice the tenant still showed "Need admin approval" when sign-in requested the first five (User.Read, Mail.Read, Calendars.Read, Chat.ReadWrite, Files.Read). Only `User.Read` signed in without admin approval, so the tenant's user-consent policy appears stricter than the column suggests.
- The app requests only what it uses, set in `DEFAULT_SCOPES` in `src/auth.rs` (currently every permission above except `ChannelMessage.Read.All`). Override at runtime with `PENGUIN_MS_SCOPES`.
- After scopes change, sign out and sign in again: the saved session keeps the old scopes.
- Used by the app today (read-only): Mail.Read, Calendars.Read, Chat.ReadWrite, Files.Read. The rest are registered for upcoming write features.

## Requirements

Minimum versions: Rust 1.85+ (edition 2024), GTK 4.10+, libadwaita 1.4+.

Build dependencies:

| What | Arch (pacman) | Debian / Ubuntu (apt) |
|---|---|---|
| Rust toolchain | `rust` (or `rustup`) | `rustc cargo` (or `rustup`) |
| C compiler, make (the TLS crate `ring` compiles C) | `base-devel` | `build-essential` |
| pkg-config | `pkgconf` | `pkg-config` |
| GTK 4 headers + library | `gtk4` | `libgtk-4-dev` |
| libadwaita headers + library | `libadwaita` | `libadwaita-1-dev` |

Runtime (besides the libraries above):

| What | Arch | Debian / Ubuntu |
|---|---|---|
| `xdg-open`: sign-in page, links, external video player | `xdg-utils` | `xdg-utils` |
| A notification daemon (desktop notifications for new messages) | provided by your desktop (KDE, GNOME, ...) | same |
| Video playback in the viewer: container support (mp4, webm) | `gst-plugins-good` | `gstreamer1.0-plugins-good` |
| Video playback: H.264 (most Teams videos). Optional: without it the app opens the default player instead | `gst-libav` | `gstreamer1.0-libav` |

Install:

    # Arch
    sudo pacman -S --needed rust base-devel pkgconf gtk4 libadwaita xdg-utils gst-plugins-good gst-libav

    # Debian / Ubuntu
    sudo apt install rustc cargo build-essential pkg-config libgtk-4-dev libadwaita-1-dev \
        xdg-utils gstreamer1.0-plugins-good gstreamer1.0-libav

Distribution versions:
- Arch: current packages satisfy everything. Verified on this project's dev machine
  (gtk4 4.20, libadwaita 1.8, rust 1.92).
- Debian 13 (trixie) and Ubuntu 24.10+: packages should be new enough (from package
  metadata, not built there).
- Ubuntu 24.04: GTK and libadwaita are new enough but the packaged Rust (1.75) is
  too old. Install Rust with `rustup` instead of `rustc cargo`.
- Debian 12 and Ubuntu 22.04: libadwaita is older than 1.4, so the app will not build.

The Debian and Ubuntu commands above were not run in this project; package names and
versions come from the distributions' package lists. If a build fails on missing
`.pc` files, check with `pkg-config --modversion gtk4 libadwaita-1`.

## Build / run

    cargo build --release
    ./target/release/penguin-ms-suite

Sign-in opens the system browser (OAuth2 authorization code + PKCE, loopback redirect). Refresh token is stored with mode 0600
at `~/.config/penguin-ms-suite/refresh_token`.

## Performance notes
- Pages are built lazily on first visit.
- Network on worker threads; UI thread never blocks.
- Teams polling is adaptive: chat list every 10 s in the background and 5 s when
  Teams is visible and focused; the open chat every 2 s (newest 3 messages only).
  Errors back off up to 60 s. A push relay can replace this later (it falls back
  to polling if the relay is down): see `docs/relay-architecture.md`.
- Cairo renderer by default (set `GSK_RENDERER` to override).
