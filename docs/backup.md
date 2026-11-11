# Encrypted backup and restore

Settings → Backup creates one encrypted `.omailbackup` file that can be restored
into any Omarchy Mail installation, including one on a machine with a different
user name or home directory.

## What is included

- `mail.db` (accounts, server settings, cached mail, drafts, Outbox, calendar
  events, todos), captured as a consistent snapshot while the app runs
- draft and Outbox attachment files, calendar subscription caches
- `preferences.json` (signatures, calendar connections, layout) and `oauth.json`
- optionally, every Secret Service entry the app uses: IMAP/SMTP passwords,
  OAuth tokens, the iCloud app-specific password and Google Calendar sign-in

Downloaded attachment cache files are not included; they are fetched again on
demand.

## Format

`OMAILBK\x01` magic, Argon2id parameters (64 MiB, 3 passes), 16-byte salt and
a 7-byte nonce prefix, followed by a gzip tar archive encrypted with
ChaCha20-Poly1305 in 1 MiB STREAM chunks (big-endian 32-bit counter, last-chunk
flag). The header is authenticated with every chunk, so a wrong passphrase,
edits, or a truncated file are all rejected. The passphrase cannot be recovered.

## Restore

1. Choose the file and enter the passphrase. The backup is decrypted, checked
   (archive paths, format/schema version, SQLite `quick_check`) and staged in
   `~/.local/share/omarchy-mail-restore/`. Saved credentials are written straight
   from memory to the keyring; they never touch the disk unencrypted.
2. Restart. Before the database is opened, the current data and config are moved
   to `~/.local/share/omarchy-mail-recovery/before-restore-<time>/`, the staged
   data is moved into place, and stored attachment paths are rewritten for this
   machine.

A backup from a newer schema than the installed app is refused.

## Automatic backups

Settings → Backup → Automatic backups writes `omarchy-mail-auto-<date>.omailbackup`
to a chosen folder daily or weekly while the app runs, keeping the newest 3–30.
These always include saved credentials. The passphrase is stored in the keyring
(`automatic` / `backup-passphrase`); last run and any error are shown in Settings.
A failed run is retried a day later.

## Command line

```bash
omarchy-mail --backup ~/mail.omailbackup [--no-secrets]
omarchy-mail --restore ~/mail.omailbackup   # applied on next launch
```

The passphrase is prompted for without echo, or read from
`OMARCHY_MAIL_BACKUP_PASSPHRASE`.
