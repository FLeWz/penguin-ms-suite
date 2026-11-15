# penguin-ms-suite

Native Linux Microsoft 365 client. Rust + GTK4/libadwaita, no embedded browser.
Thin sidebar: Outlook, Teams, Calendar, Excel. Data via Microsoft Graph.

## Features
- Outlook: inbox (latest 50), read message bodies.
- Teams: chat list, read messages, send messages.
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

## Build / run
Needs GTK 4.10+ and libadwaita 1.4+ dev packages.

    cargo build --release
    ./target/release/penguin-ms-suite

Sign-in opens the system browser (OAuth2 authorization code + PKCE, loopback redirect). Refresh token is stored with mode 0600
at `~/.config/penguin-ms-suite/refresh_token`.

## Performance notes
- Pages are built lazily on first visit.
- Network on worker threads; UI thread never blocks.
- Cairo renderer by default (set `GSK_RENDERER` to override).
