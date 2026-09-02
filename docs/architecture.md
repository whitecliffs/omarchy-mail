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
  mechanism. `mail/oauth.rs` implements provider-specific PKCE authorization
  with a loopback callback, and refreshes access tokens without placing client
  secrets or tokens in application files.
- `mail/mime.rs` parses MIME messages, retains normalized `Content-ID` values,
  and sanitises HTML before the UI sees it. HTML is rendered by WebKitGTK 6
  inside the GTK window so email-authored tables, CSS, buttons, and font rules
  use a complete layout engine. Inline CID image bytes are converted to data
  URLs from the disposable attachment cache. Remote image URLs are blocked by
  default and replaced with in-place placeholders until the user allows them;
  WebKit JavaScript, file access, storage, media, and WebGL are disabled.
- The composer keeps its small rich-text model in GTK text tags, with a plain
  text body always available as the compatibility part. Optional draft HTML is
  sanitized at the persistence boundary, and SMTP emits `multipart/alternative`
  before adding attachments. Link targets are restricted to `http(s)` and
  `mailto:` schemes; signatures are inserted from the selected account.
- `mail/imap.rs`, `mail/smtp.rs`, and `mail/sync.rs` keep protocol work on
  worker threads, with transport-aware IMAP connections, mailbox discovery,
  bounded folder fetches, queued action reconciliation, attachment caching,
  per-account error reports, and long-lived IDLE monitors with a polling
  fallback and capped reconnect backoff. A malformed MIME payload is isolated
  to its message during a mailbox fetch, counted in the sync report, and does
  not discard the other messages returned by that server response.
- `theme.rs` reads the staged Omarchy `colors.toml`, installs GTK CSS, and
  watches the active palette for live theme changes.
- `ui/window.rs` contains the desktop experience, including adaptive pane
  navigation and composable local search filters. Network and disk work stays
  outside the GTK event loop.

## Data boundaries

Configuration and cache data use `XDG_DATA_HOME/omarchy-mail/mail.db` (or
`~/.local/share/omarchy-mail/mail.db`). Secret material is held by the Secret
Service under the `org.omarchy.Mail` service name, with separate
`imap:<account>` and `smtp:<account>` entries. No message bodies or
credentials are written to logs by default. Attachment bytes are disposable
cache data under `XDG_CACHE_HOME/omarchy-mail/attachments/` and their safe
filenames and metadata are retained with the cached message in SQLite.
Account editing is intentionally separate from database persistence: the
Settings editor keeps the email identity stable, writes only changed secrets
to Secret Service, and updates server metadata in SQLite. Blank credential
fields never read a password into GTK and preserve an existing keyring entry;
the IMAP connection check uses either the newly entered secret or the stored
one on a worker thread.
Non-secret display and composing preferences use
`XDG_CONFIG_HOME/omarchy-mail/preferences.json`; signatures are keyed by
account email so changing a server password does not affect them. Missing or
invalid preferences fall back to privacy-first defaults. Sender-level remote
image permissions are stored in the same non-secret preference file; one-time
message permissions live only for the running window.
Queued outgoing messages are durable user data in the `pending_sends` table;
their selected attachments are copied to
`XDG_DATA_HOME/omarchy-mail/outbox/` before the queue row is committed. This
means the original file can move without breaking a later retry.
Local draft attachments are copied into stable per-draft directories under
`XDG_DATA_HOME/omarchy-mail/drafts/` before their metadata is committed to the
draft row. Autosave replaces that directory atomically, and deleting or sending
the draft removes the app-owned copy.

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

Search terms are converted to SQLite FTS5 prefix expressions after quoting
embedded syntax characters. This keeps the index responsive while ensuring
operators typed into the search field remain plain search text.

Remote refreshes update message metadata and attachment state without
discarding an optimistic local read/star toggle. Pending flag actions are
reapplied after each cache refresh and remain authoritative in the UI until
the corresponding IMAP operation succeeds.

Mailbox management uses the same authority boundary. CREATE, RENAME, and
DELETE are issued on a worker and update the local folder map only after a
successful server response. Message Move to… and Copy to… actions carry the
source and destination local folder names plus the source UID identity. The
worker resolves those names to remote mailboxes, checks UIDVALIDITY, then
performs UID COPY; Move additionally marks the source UID deleted and expunges
it. Copy intentionally does not invent a destination UID in the cache, so the
destination appears on its next folder refresh rather than risking duplicate
or recycled local rows.

The message list uses GTK ListBox multiple selection. Selected IDs are kept
separately from the reader’s current message, allowing a reader to remain
open while a batch is prepared. Batch flag changes and moves update the local
cache immediately, then enqueue one UID-safe action per message. Ctrl+A
selects the visible list when it has focus, Escape clears selection, and
Delete or the a key performs the corresponding batch move when multiple rows
are selected.

Mailbox lists load 100 cached rows at a time through LIMIT/OFFSET queries.
Additional pages are fetched on a worker and appended only after the current
scope still matches, while search stays bounded to its first indexed page.
This keeps GTK row creation and initial cache reads bounded for large
mailboxes without making the SQLite cache authoritative.

The protocol boundary has offline socket integration coverage in the IMAP and
SMTP modules. The tests run the production clients against temporary local
servers, so login, folder discovery, UID FETCH literals, MIME parsing, SMTP
authentication, envelope recipients, and Bcc header privacy are checked
without depending on a live provider.

## Theme boundary

`packaging/omarchy-mail.css.tpl` is installed as
`~/.config/omarchy/themed/omarchy-mail.css.tpl`. Omarchy regenerates the
matching `omarchy-mail.css` in the staged current theme on theme changes. The
application also reads `colors.toml` directly and watches it, so a running
window updates without a reboot and remains functional before the first
regeneration.
