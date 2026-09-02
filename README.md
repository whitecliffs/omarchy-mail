# Omarchy Mail

Omarchy Mail is a lightweight, native email client for Omarchy Linux. It is
designed around calm typography, clear hierarchy, and the practical reliability
of a local cache rather than a dashboard full of unrelated features.

The current working slice includes the GTK4/libadwaita shell, responsive
three-pane layout, first-run account flow, Secret Service credential storage,
SQLite/FTS cache, safe MIME boundary, provider discovery, background IMAP
fetch workers, TLS/STARTTLS SMTP sending, native notifications, local message
actions, server mailbox discovery with on-demand folder fetching, unified and
per-account cached-folder navigation, resumable local drafts,
reply/reply-all/forward prefilling, file attachments, compose/settings surfaces
with separate Cc/Bcc fields, resumable draft editing, lightweight
plain-text/HTML composition with safe formatting, links, and account signatures,
composable local search filters for account, folder, unread, starred, attachment,
and date, safe inline image rendering, privacy-first remote-image controls,
attachment re-fetch after cache eviction, a durable offline Outbox,
and Omarchy theme integration. The cache now reconciles deleted messages and
server-side mailbox removals using complete UID/UIDVALIDITY snapshots. IMAP
and SMTP can use provider-issued OAuth2 access tokens through native XOAUTH2
transport support; provider browser authorization remains a follow-up because
it requires provider-specific client registration. Read/star/move
actions are queued locally and replayed after reconnect when the recorded
mailbox UIDVALIDITY still matches the server. Message context menus now offer
server-safe Move to… and Copy to… destinations, while each account exposes
custom-folder create, rename, and delete operations. Message lists support
multi-selection with batch read/unread, star, archive, and trash actions, and
load cached folders in bounded 100-message pages.

Each enabled account has an isolated monitor. It uses IMAP IDLE when available,
refreshes the connection before common server idle limits, falls back to a
five-minute poll for older servers, and retries transient failures with capped
backoff. The first successful sync also warms the standard Sent, Drafts,
Archive, Spam, and Trash folders with a bounded cache; custom folders remain
on-demand.

HTML mail is sanitized and rendered by the system WebKitGTK 6 engine inside
the native GTK window, so real-world tables, CSS typography, buttons, and
responsive layouts retain their authored structure. Safe `cid:` inline images
are inlined from the disposable attachment cache. Remote images are blocked by
default and can be loaded once for a message or trusted for a sender; blocked
images remain in place as placeholders. Incoming attachments are cached under
`$XDG_CACHE_HOME/omarchy-mail/attachments/` (or `~/.cache/omarchy-mail/`) and
can be saved from the reader.

The local search index supports responsive prefix searches while treating
typed FTS punctuation as ordinary text. During synchronisation, one malformed
MIME message is skipped and reported without preventing the rest of the
mailbox from being cached.

Message moves and copies are account-local actions. A move updates the cached
message immediately and queues an IMAP UID COPY plus delete/expunge; a copy
leaves the source visible and queues only UID COPY. Both are replayed only
when the source mailbox UIDVALIDITY still matches. Custom folder mutations
wait for the server’s CREATE, RENAME, or DELETE acknowledgement before the
folder cache changes.

The main window adapts to narrow Hyprland tiles: the reader becomes a focused
second view with a Messages back action, and very narrow windows expose the
sidebar through a compact navigation button.

The composer defaults to plain text. HTML mode adds only the small controls
needed for everyday mail—bold, italic, underline, bullets, links, and a
signature insert action—and always sends a plain-text alternative as well.
HTML is sanitized before it is stored in a draft or handed to SMTP.

## Build and preview

System requirements are the Omarchy GTK stack, WebKitGTK 6, Rust, and a Secret
Service provider. On this machine GTK4 4.22.4, libadwaita 1.9.3, SQLite,
libsecret, and Rust 1.98 are available. Install the renderer on Arch with:

```bash
sudo pacman -S --needed webkitgtk-6.0
```

```bash
cargo test
cargo run
```

For a visual preview without an account:

```bash
OMARCHY_MAIL_DEMO=1 cargo run
```

## Development installation

```bash
./scripts/install-dev.sh
omarchy-mail
./scripts/uninstall-dev.sh
```

The uninstall script removes only the development binary, desktop metadata,
icon, and theme template. It does not remove mail data from
`~/.local/share/omarchy-mail/`.

For an Arch package, use `packaging/PKGBUILD` with a release source tarball.
The desktop entry uses the normal XDG application directory and appears in
Omarchy’s launcher without a custom launcher integration.

The test suite includes fixture messages for UTF-8 headers, multipart
attachments, inline and remote images, truncated MIME, and a serialized SMTP
round trip. It also exercises the real IMAP and SMTP socket transports against
temporary localhost protocol servers, including UIDVALIDITY, flags, folders,
authentication, recipients, and Bcc handling. It remains fully offline and
does not require real credentials.

## Accounts and data

Add accounts through the graphical setup flow. IMAP and SMTP usernames and
passwords can differ; secrets are stored separately in the Linux Secret
Service and never in SQLite. The account model separates authentication
method from transport security. Password entries and OAuth2 token entries use
isolated Secret Service slots; the account flow accepts either credential type,
while provider browser authorization is intentionally not hard-coded into the
source tree. Open Settings and choose Edit beside an account to review or
change its display name, IMAP/SMTP servers, ports, security, usernames, or
authentication method. Credential fields stay blank by design: leave them
empty to preserve an existing keyring entry, or enter only the missing
credential to repair it. Check IMAP connection tests the stored or newly
entered IMAP credential without blocking the UI. The email address remains the
account identity and is not changed by this editor. The local database is at
`$XDG_DATA_HOME/omarchy-mail/mail.db` or `~/.local/share/omarchy-mail/mail.db`.
Non-secret preferences, including per-account signatures, are stored at
`$XDG_CONFIG_HOME/omarchy-mail/preferences.json` or
`~/.config/omarchy-mail/preferences.json`.
Draft attachments are safely copied under
`$XDG_DATA_HOME/omarchy-mail/drafts/` (or `~/.local/share/omarchy-mail/drafts/`)
so an autosaved draft remains usable if the original file moves.

## Theme integration

The app follows the active Omarchy palette from
`~/.local/state/omarchy/current/theme/colors.toml`. The dev installer also
registers `~/.config/omarchy/themed/omarchy-mail.css.tpl`, using Omarchy’s
supported generated-template mechanism. Re-applying or changing an Omarchy
theme regenerates the template output, and the running app watches the active
palette for live updates.

## Troubleshooting

- If an account reports that no IMAP or SMTP password is stored, open Settings,
  choose Edit, enter the missing credential, and save. The Check IMAP
  connection action can verify the incoming settings before saving.
- If account setup cannot save a password, ensure a Secret Service provider is
  running in the user session and retry.
- If the app starts with fallback colours, inspect
  `~/.local/state/omarchy/current/theme/colors.toml` and relaunch.
- Use `OMARCHY_MAIL_DEMO=1` to inspect the UI without network credentials.

See [docs/architecture.md](docs/architecture.md) for the main design
decisions and boundaries.
