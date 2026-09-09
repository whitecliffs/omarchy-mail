# iCloud Calendar

Open Settings → Calendar, enter the Apple Account email and an app-specific
password, then choose Find iCloud calendars. Choose a calendar and Save Changes.
The password is stored in the system keyring, not the preferences or database.
Passwords are passed to the isolated calendar helper through stdin.

New events upload to the selected calendar. Existing local events stay local
unless you select “Also upload my existing local calendar events” before finding
calendars. Changes and deletions from iCloud are reflected in the local cache.
Editing/deleting existing events is currently done in Apple Calendar; the Omarchy
event editor currently creates events only. No existing remote ICS is rewritten.
Tasks remain local because modern Apple Reminders does not provide CalDAV access.

Sync runs on app startup, opening the planner, saving a new event, saving settings,
and every five minutes while the app is open. The planner also has Sync iCloud
and a status message. Failed uploads remain queued as unmapped local events;
stable resource names and conditional creation protect retries from duplicates.
A complete successful snapshot updates the cache in a database transaction.
Mapped events from other calendar accounts and unmapped local events are preserved.

Recurring events are expanded from one year ago to two years ahead; standalone
events are retained regardless of date. Remote timezone and recurrence parsing
uses icalendar and recurring-ical-events. Local new-event times use the computer's
timezone. Calendar reminders/alarms and invitations are not managed by this UI.

The development installer creates a private application Python environment with
scripts/icloud-requirements.txt. The Rust executable embeds the helper source.
No server or VPN is required. Only HTTPS iCloud domains are accepted, including
redirects and discovered resource URLs. Responses and helper runtime are bounded.

Verification:

```
cargo test --locked
~/.local/share/omarchy-mail/icloud-venv/bin/python scripts/test_icloud_worker.py
```

Live Apple-account authentication requires the user's app-specific password and
must be checked after connecting in the app; offline tests do not verify that.

Apple password instructions: https://support.apple.com/en-gb/102654
# Calendar subscriptions

Settings → Calendar includes visibility controls for your personal/iCloud
calendar and read-only subscriptions. Add a name and a `webcal://` or `https://`
ICS URL, then Save Changes. Enabled sources appear together in the planner:
personal events use an accent-coloured circle, subscriptions a yellow diamond.
Event details identify their source. New events still use the personal/iCloud
calendar, even when its visibility is switched off.

Feeds refresh every five minutes while the application runs, on opening the
planner, and through Refresh calendars. Failed downloads retain the last good
cache; successful downloads replace it to reflect rescheduling, removals and
cancellations without appending duplicates. Recurrences cover one year back
and two years ahead. HTTPS only, bounded downloads, no iCloud credentials sent
to subscription hosts. Feed events are cached separately and never uploaded to
iCloud. Add the same URL on other devices to see the subscription there.

Unchecking a source hides it; Remove removes the subscription from settings
when changes are saved. Neither operation deletes events from iCloud or the
provider. Previously downloaded feed caches are retained locally.
