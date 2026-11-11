# Google Calendar

Open Settings → Calendar → Google Calendar, enter the Google account email and
choose **Sign in with Google…**. The browser opens Google's consent page; after
approving, pick a calendar and Save Changes.

## One-time Google Cloud setup

Google Calendar uses the same desktop OAuth client as Gmail sign-in
(`google_client_id` / `google_client_secret` in
`~/.config/omarchy-mail/oauth.json`, or the `OMARCHY_MAIL_GOOGLE_CLIENT_ID`
environment variables). In Google Cloud Console for that client's project:

1. APIs & Services → Library → enable **Google Calendar API** and
   **Google Tasks API**.
2. OAuth consent screen → add the scopes
   `https://www.googleapis.com/auth/calendar` and
   `https://www.googleapis.com/auth/tasks`. While the app is in *Testing*,
   add your Google account as a test user.

## Behaviour

- Calendar access is requested separately from mail; it is stored in the
  Secret Service as `oauth2:google-calendar:<email>` and refreshed automatically.
  Only short-lived access tokens are passed (via stdin) to the calendar helper.
- Only `https://www.googleapis.com/calendar/v3/` is contacted.
- Events (including recurring instances, expanded by Google) are cached from one
  year ago to two years ahead, every five minutes while the app is open.
- New events upload with a stable client-chosen ID derived from the local event,
  so a retried upload cannot create a duplicate.
- iCloud and Google can be connected together. Each keeps its own events;
  **New events upload to** chooses the single destination for events created in
  Omarchy Mail (Automatic prefers iCloud).
- Single events can be edited or deleted in the planner; changes are sent as
  PATCH/DELETE on the next sync. Recurring series are edited in Google Calendar.

## Google Tasks

Settings → Calendar → Google Tasks → **Choose task list…** syncs the todo list
both ways with one Google Tasks list (title, notes, due date, completed). Each
todo keeps a revision number: local edits made since the last sync are pushed,
untouched todos take Google's values, and deletions on either side are applied.
If a todo was edited here but deleted in Google, it is uploaded again rather
than lost.
