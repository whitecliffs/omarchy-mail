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
  the `keyring` crate.
- `mail/mime.rs` parses MIME messages and sanitises HTML before the UI sees it.
- `mail/imap.rs`, `mail/smtp.rs`, and `mail/sync.rs` keep protocol work on
  worker threads, with transport-aware IMAP connections, bounded fetches,
  reconnect retries, attachments, and per-account error reports.
- `theme.rs` reads the staged Omarchy `colors.toml`, installs GTK CSS, and
  watches the active palette for live theme changes.
- `ui/window.rs` contains the first vertical slice of the desktop experience.

## Data boundaries

Configuration and cache data use `XDG_DATA_HOME/omarchy-mail/mail.db` (or
`~/.local/share/omarchy-mail/mail.db`). Secret material is held by the Secret
Service under the `org.omarchy.Mail` service name, with separate
`imap:<account>` and `smtp:<account>` entries. No message bodies or
credentials are written to logs by default.

The database is a cache/state store, not an authority over the IMAP server.
Remote UID/UIDVALIDITY columns and pending actions are reserved for safe
reconciliation in the synchronisation worker. Local drafts are stored as
messages in the `Drafts` folder and are included in the FTS index.

## Theme boundary

`packaging/omarchy-mail.css.tpl` is installed as
`~/.config/omarchy/themed/omarchy-mail.css.tpl`. Omarchy regenerates the
matching `omarchy-mail.css` in the staged current theme on theme changes. The
application also reads `colors.toml` directly and watches it, so a running
window updates without a reboot and remains functional before the first
regeneration.
