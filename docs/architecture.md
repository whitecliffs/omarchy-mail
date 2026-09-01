# Omarchy Mail architecture

Omarchy Mail is a native GTK4/libadwaita application. The UI is kept separate
from the mail and persistence boundaries so IMAP/SMTP workers can evolve
without making the GTK main loop responsible for network activity.

## Current boundaries

- `models.rs` contains account, server, and cached message value types.
- `database.rs` owns XDG data placement, schema migrations, cache metadata,
  FTS5 indexing, and pending-action storage. Passwords are intentionally not
  represented in its schema.
- `mail/credentials.rs` stores passwords and OAuth2 access tokens using
  isolated Linux Secret Service entries via the `keyring` crate. `AuthMethod`
  is deliberately separate from TLS transport settings. IMAP performs native
  XOAUTH2 challenge authentication and SMTP selects lettre's XOAUTH2
  mechanism; provider browser authorization is deferred until provider client
  registration can be configured without shipping secrets.
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
Queued outgoing messages are durable user data in the `pending_sends` table;
their selected attachments are copied to
`XDG_DATA_HOME/omarchy-mail/outbox/` before the queue row is committed. This
means the original file can move without breaking a later retry.

Each enabled account owns an isolated monitor thread. The monitor performs a
bounded initial sync, waits for INBOX changes using IMAP IDLE for at most 25
minutes, then reconnects and reconciles again. The initial pass also warms the
standard non-Inbox folders with a 100-message bound per folder; custom folders
remain on-demand. If IDLE is unavailable it polls every five minutes;
connection and authentication failures use capped exponential backoff. Stop
signals are checked between waits so removing an account does not start another
sync.

Attachment bytes are disposable. If a cached path is missing, the reader
offers Download and starts a complete RFC822 fetch on a worker. The fetch
selects the recorded mailbox, verifies UIDVALIDITY, reparses the message, and
upserts the refreshed metadata through the normal cache path. This keeps cache
eviction recoverable without making SQLite authoritative over IMAP.

SMTP failures caused by network/transport conditions are retried with capped
backoff by an account-local outbox monitor. Permanent response or local file
errors remain visible in Outbox until the user chooses Retry now or Discard;
credentials are never copied into queued message data. Cc and Bcc are stored as
separate recipient lists; lettre keeps Bcc out of the serialized message while
including it in the SMTP envelope.

IMAP sync asks each selected mailbox for its complete UID set while fetching
only a bounded recent window of full messages. After a successful upsert, the
cache removes rows with absent UIDs or an old UIDVALIDITY, and mailbox
discovery removes folders no longer returned by LIST. This keeps startup and
scrolling light without allowing deleted server mail to remain indefinitely.

The database is a cache/state store, not an authority over the IMAP server.
Remote UID/UIDVALIDITY columns identify queued actions safely; the
synchronisation worker deletes an action only after the IMAP server accepts
it. A UIDVALIDITY mismatch leaves the action queued rather than risking a
change to a recycled UID. Server folder metadata is stored separately from
message rows so remote names such as
`[Gmail]/Sent Mail` can be preserved while the UI stays calm. Local drafts
are stored as messages in the `Drafts` folder, included in the FTS index, and
reopened by parsing their compact To/Cc/Bcc recipient summary.

## Theme boundary

`packaging/omarchy-mail.css.tpl` is installed as
`~/.config/omarchy/themed/omarchy-mail.css.tpl`. Omarchy regenerates the
matching `omarchy-mail.css` in the staged current theme on theme changes. The
application also reads `colors.toml` directly and watches it, so a running
window updates without a reboot and remains functional before the first
regeneration.
