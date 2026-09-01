# Omarchy Mail

Omarchy Mail is a lightweight, native email client for Omarchy Linux. It is
designed around calm typography, clear hierarchy, and the practical reliability
of a local cache rather than a dashboard full of unrelated features.

The current working slice includes the GTK4/libadwaita shell, responsive
three-pane layout, first-run account flow, Secret Service password storage,
SQLite/FTS cache, safe MIME boundary, provider discovery, background IMAP
fetch workers, TLS/STARTTLS SMTP sending, native notifications, local message
actions, server mailbox discovery with on-demand folder fetching, unified and
per-account cached-folder navigation, local drafts,
reply/reply-all/forward prefilling, file attachments, compose/settings
surfaces, lightweight All/Unread/Starred/Attachment filters, and Omarchy theme
integration. Full folder reconciliation, live
conversation rendering, inline image rendering, OAuth2, attachment re-fetch
after cache eviction, and a full offline outbox remain explicit follow-up
milestones. Read/star/move
actions are queued locally and replayed after reconnect when the recorded
mailbox UIDVALIDITY still matches the server.

HTML mail is sanitized and rendered with native GTK/Pango formatting rather
than a browser runtime. Incoming attachments are cached under
`$XDG_CACHE_HOME/omarchy-mail/attachments/` (or `~/.cache/omarchy-mail/`) and
can be saved from the reader.

## Build and preview

System requirements are the Omarchy GTK stack, Rust, and a Secret Service
provider. On this machine GTK4 4.22.4, libadwaita 1.9.3, SQLite, libsecret,
and Rust 1.98 are available.

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

## Accounts and data

Add accounts through the graphical setup flow. IMAP and SMTP usernames and
passwords can differ; secrets are stored separately in the Linux Secret
Service and never in SQLite. The local database is at
`$XDG_DATA_HOME/omarchy-mail/mail.db` or `~/.local/share/omarchy-mail/mail.db`.

## Theme integration

The app follows the active Omarchy palette from
`~/.local/state/omarchy/current/theme/colors.toml`. The dev installer also
registers `~/.config/omarchy/themed/omarchy-mail.css.tpl`, using Omarchy’s
supported generated-template mechanism. Re-applying or changing an Omarchy
theme regenerates the template output, and the running app watches the active
palette for live updates.

## Troubleshooting

- If account setup cannot save a password, ensure a Secret Service provider is
  running in the user session and retry.
- If the app starts with fallback colours, inspect
  `~/.local/state/omarchy/current/theme/colors.toml` and relaunch.
- Use `OMARCHY_MAIL_DEMO=1` to inspect the UI without network credentials.

See [docs/architecture.md](docs/architecture.md) for the main design
decisions and boundaries.
