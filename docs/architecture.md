# Omarchy Mail architecture

Omarchy Mail is a native GTK4/libadwaita application. The UI is kept separate
from the mail and persistence boundaries so IMAP/SMTP workers can evolve
without making the GTK main loop responsible for network activity.

## Current boundaries

- `models.rs` contains account, server, and cached message value types.
- `database.rs` owns XDG data placement, schema migrations, cache metadata,
  FTS5 indexing, and pending-action storage. Passwords are intentionally not
  represented in its schema.
- `mail/credentials.rs` stores passwords using the Linux Secret Service via
  the `keyring` crate and reserves separate keyring slots for OAuth2 access
  tokens. `AuthMethod` is deliberately separate from TLS transport settings;
  provider authorization UI and SASL XOAUTH2 are not enabled yet.
- `mail/mime.rs` parses MIME messages, retains normalized `Content-ID` values,
  and sanitises HTML before the UI sees it. Inline image bytes are cached as
  ordinary disposable attachments and rendered by GTK only from that local
  cache; remote URLs and scripts never reach a web runtime.
- `mail/imap.rs`, `mail/smtp.rs`, and `mail/sync.rs` keep protocol work on
  worker threads, with transport-aware IMAP connections, mailbox discovery,
  bounded folder fetches, queued action reconciliation, attachment caching,
  per-account error reports, and long-lived IDLE monitors with a polling
  fallback and capped reconnect backoff.
- `theme.rs` reads the staged Omarchy `colors.toml`, installs GTK CSS, and
  watches the active palette for live theme changes.
- `ui/window.rs` contains the first vertical slice of the desktop experience.

## Data boundaries

Configuration and cache data use `XDG_DATA_HOME/omarchy-mail/mail.db` (or
`~/.local/share/omarchy-mail/mail.db`). Secret material is held by the Secret
Service under the `org.omarchy.Mail` service name, with separate
`imap:<account>` and `smtp:<account>` entries. No message bodies or
credentials are written to logs by default. Attachment bytes are disposable
cache data under `XDG_CACHE_HOME/omarchy-mail/attachments/` and their safe
filenames and metadata are retained with the cached message in SQLite.

Each enabled account owns an isolated monitor thread. The monitor performs a
bounded initial sync, waits for INBOX changes using IMAP IDLE for at most 25
minutes, then reconnects and reconciles again. The initial pass also warms the
standard non-Inbox folders with a 100-message bound per folder; custom folders
remain on-demand. If IDLE is unavailable it polls every five minutes;
connection and authentication failures use capped exponential backoff. Stop
signals are checked between waits so removing an account does not start another
sync.

The database is a cache/state store, not an authority over the IMAP server.
Remote UID/UIDVALIDITY columns identify queued actions safely; the
synchronisation worker deletes an action only after the IMAP server accepts
it. A UIDVALIDITY mismatch leaves the action queued rather than risking a
change to a recycled UID. Server folder metadata is stored separately from
message rows so remote names such as
`[Gmail]/Sent Mail` can be preserved while the UI stays calm. Local drafts
are stored as messages in the `Drafts` folder and are included in the FTS index.

## Theme boundary

`packaging/omarchy-mail.css.tpl` is installed as
`~/.config/omarchy/themed/omarchy-mail.css.tpl`. Omarchy regenerates the
matching `omarchy-mail.css` in the staged current theme on theme changes. The
application also reads `colors.toml` directly and watches it, so a running
window updates without a reboot and remains functional before the first
regeneration.
