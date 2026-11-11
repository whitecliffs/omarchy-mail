"""Private JSON stdin/stdout bridge for iCloud CalDAV, Google Calendar and iCalendar parsing."""
import base64
from datetime import date, datetime, time, timedelta, timezone
import json
import signal
import sys
import traceback
import ipaddress
import socket
from urllib.parse import quote, urlencode, urljoin, urlsplit
from urllib.request import Request, build_opener, HTTPRedirectHandler
from urllib.error import HTTPError
from defusedxml import ElementTree as XML
from icalendar import Calendar, Event, vCalAddress, vText
import recurring_ical_events

D = '{DAV:}'
C = '{urn:ietf:params:xml:ns:caldav}'

class SafeError(Exception):
    pass

def trusted(url):
    parts = urlsplit(url)
    host = (parts.hostname or '').lower()
    if (parts.scheme != 'https' or not (host == 'icloud.com' or host.endswith('.icloud.com'))
            or parts.username or parts.password or parts.port not in (None, 443)):
        raise SafeError('iCloud returned an untrusted server address.')
    return url

class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None

class Client:
    def __init__(self, username, password):
        self.auth = 'Basic ' + base64.b64encode(f'{username}:{password}'.encode()).decode()
        self.opener = build_opener(NoRedirect())

    def request(self, method, url, body=None, headers=None, acceptable=(200, 201, 204, 207)):
        for _ in range(5):
            trusted(url)
            request = Request(url, data=body, method=method, headers={
                'Authorization': self.auth, 'Content-Type': 'application/xml; charset=utf-8',
                **(headers or {}),
            })
            try:
                response = self.opener.open(request, timeout=20)
            except HTTPError as error:
                response = error
            with response:
                self.headers = response.headers
                if response.code in (301, 302, 307, 308):
                    url = trusted(urljoin(url, response.headers.get('Location', '')))
                    continue
                if response.code not in acceptable:
                    if response.code in (401, 403):
                        raise SafeError('iCloud refused access. Check the Apple Account and app-specific password, and calendar permissions.')
                    raise SafeError(f'iCloud request failed (HTTP {response.code}). Local events are retained.')
                data = response.read(20_000_001)
                if len(data) > 20_000_000:
                    raise SafeError('Calendar response is too large; local events were retained.')
                return response.code, data, url
        raise SafeError('Too many iCloud redirects.')

    def propfind(self, url, props, depth='0'):
        body = f'<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop>{props}</d:prop></d:propfind>'.encode()
        _, data, resolved = self.request('PROPFIND', url, body, {'Depth': depth})
        return XML.fromstring(data), resolved

def good_props(response):
    for stat in response.findall(D + 'propstat'):
        if ' 200 ' in (stat.findtext(D + 'status') or ''):
            yield stat.find(D + 'prop')

def property_href(doc, tag):
    for response in doc.findall(D + 'response'):
        for prop in good_props(response):
            found = prop.findtext(tag + '/' + D + 'href')
            if found:
                return found
    raise SafeError('Could not discover the iCloud calendar account.')

def discover(client):
    doc, url = client.propfind('https://caldav.icloud.com/', '<d:current-user-principal/>')
    principal = trusted(urljoin(url, property_href(doc, D + 'current-user-principal')))
    doc, url = client.propfind(principal, '<c:calendar-home-set/>')
    home = trusted(urljoin(url, property_href(doc, C + 'calendar-home-set')))
    doc, url = client.propfind(home, '<d:resourcetype/><d:displayname/><c:supported-calendar-component-set/>', '1')
    calendars = []
    for response in doc.findall(D + 'response'):
        for prop in good_props(response):
            if prop.find(D + 'resourcetype/' + C + 'calendar') is None:
                continue
            components = prop.findall(C + 'supported-calendar-component-set/' + C + 'comp')
            if components and not any(item.get('name') == 'VEVENT' for item in components):
                continue
            href = response.findtext(D + 'href')
            if href:
                calendars.append({'url': trusted(urljoin(url, href)), 'name': prop.findtext(D + 'displayname') or 'Calendar'})
    if not calendars:
        raise SafeError('No event calendars found in this iCloud account.')
    return {'calendars': calendars}

def serialize(event, uid):
    item = Event()
    item.add('uid', uid)
    item.add('dtstamp', datetime.now(timezone.utc))
    for key, field in [('dtstart', 'starts_at'), ('dtend', 'ends_at')]:
        # astimezone uses this machine's timezone rules for the event date.
        value = datetime.strptime(event[field], '%Y-%m-%d %H:%M').astimezone(timezone.utc)
        item.add(key, value)
    if item.decoded('dtend') <= item.decoded('dtstart'):
        raise SafeError('An event ends before it starts. Correct its dates before syncing.')
    for key, field in [('summary', 'title'), ('description', 'notes'), ('location', 'location')]:
        item.add(key, event[field])
    calendar = Calendar()
    calendar.add('version', '2.0')
    calendar.add('prodid', '-//Omarchy Mail//Calendar//EN')
    calendar.add_component(item)
    return calendar.to_ical()

def local_date(value):
    if isinstance(value, datetime):
        return value.astimezone().strftime('%Y-%m-%d %H:%M') if value.tzinfo else value.strftime('%Y-%m-%d %H:%M')
    return datetime.combine(value, time()).strftime('%Y-%m-%d %H:%M')

def expand(raw, href, start, end):
    calendar = Calendar.from_ical(raw)
    result = []
    components = calendar.walk('VEVENT')
    recurring = any('RRULE' in item or 'RDATE' in item or 'RECURRENCE-ID' in item for item in components)
    instances = recurring_ical_events.of(calendar).between(start, end) if recurring else components
    for item in instances:
        if str(item.get('STATUS', '')).upper() == 'CANCELLED':
            continue
        begin = item.decoded('dtstart')
        finish = item.decoded('dtend', None)
        if finish is None:
            finish = begin + item.decoded('duration', timedelta(0) if isinstance(begin, datetime) else timedelta(days=1))
        identity = item.decoded('recurrence-id', begin)
        key = href + '#' + str(item.get('uid', '')) + '#' + identity.isoformat()
        result.append({'key': key, 'href': href, 'event': {
            'id': 0, 'title': str(item.get('summary', '(Untitled)')),
            'notes': str(item.get('description', '')), 'location': str(item.get('location', '')),
            'starts_at': local_date(begin), 'ends_at': local_date(finish), 'message_id': None,
        }, 'recurring': recurring})
        if len(result) > 10000:
            raise SafeError('Too many recurring instances to display safely.')
    return result

def all_day(event):
    """Midnight-to-midnight spans are all-day events in the planner model."""
    return (event['starts_at'].endswith(' 00:00') and event['ends_at'].endswith(' 00:00')
            and event['ends_at'] > event['starts_at'])

def event_times(event):
    if all_day(event):
        return (datetime.strptime(event['starts_at'], '%Y-%m-%d %H:%M').date(),
                datetime.strptime(event['ends_at'], '%Y-%m-%d %H:%M').date())
    starts, ends = (datetime.strptime(event[field], '%Y-%m-%d %H:%M').astimezone()
                    for field in ('starts_at', 'ends_at'))
    if ends <= starts:
        raise SafeError('An event ends before it starts. Correct its dates before syncing.')
    return starts, ends

def apply_icloud_changes(client, changes):
    """Edits only the fields the planner owns, keeping attendees, alarms and
    other properties intact. ETags stop us overwriting a newer remote edit."""
    applied, dropped = [], []
    for change in changes:
        href = trusted(change['href'])
        if change['action'] == 'delete':
            client.request('DELETE', href, None, {}, (200, 204, 404, 410))
            applied.append(change['id'])
            continue
        status, raw, _ = client.request('GET', href, None, {}, (200, 404, 410))
        if status != 200:
            dropped.append(change['id'])
            continue
        etag = client.headers.get('ETag')
        calendar = Calendar.from_ical(raw)
        items = calendar.walk('VEVENT')
        if len(items) != 1 or 'RRULE' in items[0] or 'RECURRENCE-ID' in items[0]:
            dropped.append(change['id'])  # recurring series are edited in Apple Calendar
            continue
        item, event = items[0], change['event']
        starts, ends = event_times(event)
        for key in ('summary', 'description', 'location', 'dtstart', 'dtend', 'duration', 'dtstamp', 'last-modified'):
            if key in item:
                del item[key]
        item.add('summary', event['title'])
        if event['notes']:
            item.add('description', event['notes'])
        if event['location']:
            item.add('location', event['location'])
        timed = isinstance(starts, datetime)
        item.add('dtstart', starts.astimezone(timezone.utc) if timed else starts)
        item.add('dtend', ends.astimezone(timezone.utc) if timed else ends)
        now = datetime.now(timezone.utc)
        item.add('dtstamp', now)
        item.add('last-modified', now)
        sequence = int(item.get('sequence', 0)) + 1
        if 'sequence' in item:
            del item['sequence']
        item.add('sequence', sequence)
        headers = {'Content-Type': 'text/calendar; charset=utf-8', **({'If-Match': etag} if etag else {})}
        status, _, _ = client.request('PUT', href, calendar.to_ical(), headers, (200, 201, 204, 412))
        (dropped if status == 412 else applied).append(change['id'])
    return applied, dropped

def sync(client, request):
    applied, dropped = apply_icloud_changes(client, request.get('changes', []))
    url = trusted(request['calendar_url'])
    if not url.endswith('/'):
        url += '/'
    today = date.today()
    start, end = today - timedelta(days=366), today + timedelta(days=731)
    # Complete resource enumeration, then bounded recurrence expansion locally.
    body = b'<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"/></c:comp-filter></c:filter></c:calendar-query>'
    _, data, resolved = client.request('REPORT', url, body, {'Depth': '1'})
    doc = XML.fromstring(data)
    if doc.tag != D + 'multistatus':
        raise SafeError('Invalid calendar response; cached events were retained.')
    events, uploaded = [], []
    for response in doc.findall(D + 'response'):
        resource = response.findtext(D + 'href')
        if not resource:
            raise SafeError('Incomplete calendar response; cached events were retained.')
        href = trusted(urljoin(resolved, resource))
        # iCloud includes the calendar collection itself in the report. It is
        # metadata, not an event resource, and has no calendar-data property.
        if href.rstrip('/') == resolved.rstrip('/'):
            continue
        raw = None
        for prop in good_props(response):
            raw = prop.findtext(C + 'calendar-data')
            if raw is not None:
                break
        if raw is None:
            # iCloud may return the ETag but a 404 for the inline calendar-data
            # property. The resource itself still exists and can be fetched.
            _, raw, _ = client.request('GET', href)
        events.extend(expand(raw, href, start, end))
        if len(events) > 20000:
            raise SafeError('Calendar has too many event instances to display.')
    for event in request.get('pending', []):
        uid = request['namespace'] + '-' + str(event['id'])
        href = urljoin(url, uid + '.ics')
        raw = serialize(event, uid)
        status, _, _ = client.request('PUT', href, raw, {'If-None-Match': '*', 'Content-Type': 'text/calendar; charset=utf-8'}, (201, 204, 412))
        if status == 412:
            _, raw, _ = client.request('GET', href)
            saved = Calendar.from_ical(raw).walk('VEVENT')
            if not saved or str(saved[0].get('uid')) != uid:
                raise SafeError('Calendar resource collision; no existing event was overwritten.')
        uploaded.append({'id': event['id'], 'href': href})
        # Include newly uploaded events in the same snapshot.
        events = [item for item in events if item['href'] != href]
        events.extend(expand(raw, href, start, end))
    return {'events': events, 'uploaded': uploaded, 'applied': applied, 'dropped': dropped}

GOOGLE_API = 'https://www.googleapis.com/calendar/v3'

GOOGLE_APIS = {'www.googleapis.com': '/calendar/v3/', 'tasks.googleapis.com': '/tasks/v1/'}

def google_trusted(url):
    parts = urlsplit(url)
    host = (parts.hostname or '').lower()
    if (parts.scheme != 'https' or host not in GOOGLE_APIS
            or parts.username or parts.password or parts.port not in (None, 443)
            or not parts.path.startswith(GOOGLE_APIS[host])):
        raise SafeError('Google returned an untrusted server address.')
    return url

class GoogleClient:
    def __init__(self, token):
        self.auth = 'Bearer ' + token
        self.opener = build_opener(NoRedirect())

    def request(self, method, url, body=None, acceptable=(200,)):
        google_trusted(url)
        data = json.dumps(body).encode() if body is not None else None
        request = Request(url, data=data, method=method, headers={
            'Authorization': self.auth, 'Accept': 'application/json',
            **({'Content-Type': 'application/json; charset=utf-8'} if data else {}),
        })
        try:
            response = self.opener.open(request, timeout=20)
        except HTTPError as error:
            response = error
        with response:
            if response.code not in acceptable:
                if response.code == 401:
                    raise SafeError('Google sign-in has expired. Sign in again in Settings → Calendar.')
                if response.code == 403:
                    raise SafeError('Google refused calendar access. Check that the Google Calendar API is enabled for your OAuth client and that calendar access was granted.')
                raise SafeError(f'Google Calendar request failed (HTTP {response.code}). Local events are retained.')
            raw = response.read(20_000_001)
            if len(raw) > 20_000_000:
                raise SafeError('Calendar response is too large; local events were retained.')
            return response.code, (json.loads(raw) if raw.strip() else {})

    def pages(self, url, params):
        token = None
        for _ in range(200):
            query = dict(params, **({'pageToken': token} if token else {}))
            _, page = self.request('GET', url + '?' + urlencode(query))
            yield page
            token = page.get('nextPageToken')
            if not token:
                return
        raise SafeError('Google Calendar returned too many pages.')

def google_calendar_url(calendar_id, suffix=''):
    return f"{GOOGLE_API}/calendars/{quote(calendar_id, safe='')}/events{suffix}"

def google_discover(client):
    calendars = []
    for page in client.pages(GOOGLE_API + '/users/me/calendarList', {'minAccessRole': 'writer', 'maxResults': 250}):
        for item in page.get('items', []):
            if item.get('deleted') or not item.get('id'):
                continue
            calendars.append({'url': item['id'], 'name': item.get('summaryOverride') or item.get('summary') or 'Calendar',
                              'primary': bool(item.get('primary'))})
    if not calendars:
        raise SafeError('No writable calendars found in this Google account.')
    calendars.sort(key=lambda item: (not item.pop('primary'), item['name'].lower()))
    return {'calendars': calendars}

def google_time(value):
    """Google start/end objects carry either an all-day date or an RFC 3339 instant."""
    if 'dateTime' in value:
        return local_date(datetime.fromisoformat(value['dateTime'].replace('Z', '+00:00')))
    if 'date' in value:
        return local_date(date.fromisoformat(value['date']))
    raise SafeError('Google returned an event without a time; cached events were retained.')

def google_event(calendar_id, item):
    if item.get('status') == 'cancelled':
        return None
    series = item.get('recurringEventId') or item['id']
    return {'key': f"google:{calendar_id}#{item['id']}", 'href': f"google:{calendar_id}#{series}", 'event': {
        'id': 0, 'title': item.get('summary') or '(Untitled)', 'notes': item.get('description', ''),
        'location': item.get('location', ''), 'starts_at': google_time(item.get('start', {})),
        'ends_at': google_time(item.get('end', {})), 'message_id': None,
    }, 'recurring': bool(item.get('recurringEventId') or item.get('recurrence'))}

def google_event_id(namespace, local_id):
    # Google accepts client IDs made of base32hex characters (0-9, a-v). A UUID's
    # hex digits fit, so the same local event always maps to the same remote ID.
    value = ''.join(ch for ch in namespace.lower() if ch in '0123456789abcdef') + 'e' + str(int(local_id))
    if len(value) < 5:
        raise SafeError('Invalid calendar namespace.')
    return value

def google_times(event):
    starts, ends = event_times(event)
    if isinstance(starts, datetime):
        return {'dateTime': starts.isoformat()}, {'dateTime': ends.isoformat()}
    return {'date': starts.isoformat()}, {'date': ends.isoformat()}

def google_body(event, remote_id):
    start, end = google_times(event)
    return {'id': remote_id, 'summary': event['title'], 'description': event['notes'],
            'location': event['location'], 'start': start, 'end': end}

def apply_google_changes(client, calendar_id, changes):
    applied, dropped = [], []
    for change in changes:
        event_id = change['remote_key'].split('#', 1)[-1]
        url = google_calendar_url(calendar_id, '/' + quote(event_id, safe=''))
        if change['action'] == 'delete':
            client.request('DELETE', url, None, (200, 204, 404, 410))
            applied.append(change['id'])
            continue
        event = change['event']
        start, end = google_times(event)
        body = {'summary': event['title'], 'description': event['notes'], 'location': event['location'],
                'start': start, 'end': end}
        status, _ = client.request('PATCH', url, body, (200, 404, 410))
        (applied if status == 200 else dropped).append(change['id'])
    return applied, dropped

def google_sync(client, request):
    calendar_id = request['calendar_url']
    applied, dropped = apply_google_changes(client, calendar_id, request.get('changes', []))
    today = date.today()
    start = datetime.combine(today - timedelta(days=366), time(), timezone.utc)
    end = datetime.combine(today + timedelta(days=731), time(), timezone.utc)
    events = []
    params = {'singleEvents': 'true', 'showDeleted': 'false', 'maxResults': 2500,
              'timeMin': start.isoformat().replace('+00:00', 'Z'), 'timeMax': end.isoformat().replace('+00:00', 'Z')}
    for page in client.pages(google_calendar_url(calendar_id), params):
        for item in page.get('items', []):
            mapped = google_event(calendar_id, item)
            if mapped:
                events.append(mapped)
        if len(events) > 20000:
            raise SafeError('Calendar has too many event instances to display.')
    uploaded = []
    for event in request.get('pending', []):
        remote_id = google_event_id(request['namespace'], event['id'])
        status, saved = client.request('POST', google_calendar_url(calendar_id), google_body(event, remote_id), (200, 409))
        if status == 409:
            # A previous attempt already created this exact ID; fetch it rather than duplicating.
            _, saved = client.request('GET', google_calendar_url(calendar_id, '/' + quote(remote_id, safe='')))
        mapped = google_event(calendar_id, saved)
        href = f'google:{calendar_id}#{remote_id}'
        uploaded.append({'id': event['id'], 'href': href})
        events = [item for item in events if item['href'] != href]
        if mapped:
            events.append(mapped)
    return {'events': events, 'uploaded': uploaded, 'applied': applied, 'dropped': dropped}

TASKS_API = 'https://tasks.googleapis.com/tasks/v1'

def task_lists(client):
    lists = []
    for page in client.pages(TASKS_API + '/users/@me/lists', {'maxResults': 100}):
        lists.extend({'url': item['id'], 'name': item.get('title') or 'Tasks'} for item in page.get('items', []))
    if not lists:
        raise SafeError('No Google task lists were found.')
    return {'calendars': lists}

def task_body(todo):
    body = {'title': todo['title'], 'notes': todo['notes'],
            'status': 'completed' if todo['completed'] else 'needsAction'}
    due = (todo.get('due_at') or '').strip()
    body['due'] = f'{date.fromisoformat(due[:10]).isoformat()}T00:00:00.000Z' if due else None
    return body

def remote_task(item):
    due = item.get('due')
    return {'remote_id': item['id'], 'title': item.get('title') or '(Untitled)', 'notes': item.get('notes', ''),
            'due_at': due[:10] if due else None, 'completed': item.get('status') == 'completed'}

def tasks_sync(client, request):
    base = f"{TASKS_API}/lists/{quote(request['list_id'], safe='')}/tasks"
    created, pushed, deleted = [], [], []
    for row in request.get('deletes', []):
        client.request('DELETE', base + '/' + quote(row['remote_id'], safe=''), None, (200, 204, 404, 410))
        deleted.append(row['id'])
    for update in request.get('updates', []):
        status, _ = client.request('PATCH', base + '/' + quote(update['remote_id'], safe=''),
                                   task_body(update['todo']), (200, 404, 410))
        if status == 200:
            pushed.append({'id': update['todo']['id'], 'revision': update['revision']})
    for create in request.get('creates', []):
        _, saved = client.request('POST', base, task_body(create['todo']))
        created.append({'id': create['todo']['id'], 'remote_id': saved['id'], 'revision': create['revision']})
    tasks = []
    for page in client.pages(base, {'showCompleted': 'true', 'showHidden': 'true', 'maxResults': 100}):
        tasks.extend(remote_task(item) for item in page.get('items', []) if not item.get('deleted'))
        if len(tasks) > 20000:
            raise SafeError('Too many tasks to sync safely.')
    return {'created': created, 'pushed': pushed, 'deletions_done': deleted, 'remote': tasks}

def address(value):
    text = str(value or '')
    return text[7:] if text.lower().startswith('mailto:') else text

def invite_event(calendar):
    items = calendar.walk('VEVENT')
    if not items:
        raise SafeError('The invitation has no event.')
    return next((i for i in items if 'RECURRENCE-ID' not in i), items[0])

def parse_invite(raw):
    calendar = Calendar.from_ical(raw.encode() if isinstance(raw, str) else raw)
    method = str(calendar.get('method', 'PUBLISH')).upper()
    item = invite_event(calendar)
    begin = item.decoded('dtstart')
    finish = item.decoded('dtend', None)
    if finish is None:
        finish = begin + item.decoded('duration', timedelta(0) if isinstance(begin, datetime) else timedelta(days=1))
    organizer = item.get('organizer')
    attendees = item.get('attendee', [])
    if not isinstance(attendees, list):
        attendees = [attendees]
    return {'method': method, 'uid': str(item.get('uid', '')), 'title': str(item.get('summary', '(Untitled)')),
            'notes': str(item.get('description', '')), 'location': str(item.get('location', '')),
            'starts_at': local_date(begin), 'ends_at': local_date(finish),
            'recurring': 'RRULE' in item, 'sequence': int(item.get('sequence', 0)),
            'organizer_email': address(organizer),
            'organizer_name': str(organizer.params.get('CN', '')) if organizer is not None else '',
            'attendees': [address(a).lower() for a in attendees],
            'cancelled': method == 'CANCEL' or str(item.get('status', '')).upper() == 'CANCELLED'}

def invite_reply(raw, attendee_email, attendee_name, partstat):
    if partstat not in ('ACCEPTED', 'TENTATIVE', 'DECLINED'):
        raise SafeError('Unsupported invitation response.')
    original = invite_event(Calendar.from_ical(raw.encode() if isinstance(raw, str) else raw))
    reply = Calendar()
    reply.add('prodid', '-//Omarchy Mail//Calendar//EN')
    reply.add('version', '2.0')
    reply.add('method', 'REPLY')
    item = Event()
    for key in ('uid', 'sequence', 'dtstart', 'dtend', 'summary', 'organizer', 'recurrence-id'):
        if key in original:
            item.add(key, original[key])
    item.add('dtstamp', datetime.now(timezone.utc))
    attendee = vCalAddress('mailto:' + attendee_email)
    attendee.params['PARTSTAT'] = vText(partstat)
    if attendee_name:
        attendee.params['CN'] = vText(attendee_name)
    item.add('attendee', attendee)
    reply.add_component(item)
    return {'ics': reply.to_ical().decode()}

def main():
    signal.alarm(120)
    try:
        request = json.load(sys.stdin)
        if request['action'] == 'subscription':
            result = subscription(request['url'])
        elif request['action'] in ('google_discover', 'google_sync', 'tasks_lists', 'tasks_sync'):
            client = GoogleClient(request['token'])
            result = {'google_discover': google_discover, 'tasks_lists': task_lists,
                      'google_sync': lambda c: google_sync(c, request),
                      'tasks_sync': lambda c: tasks_sync(c, request)}[request['action']](client)
        elif request['action'] == 'invite':
            result = parse_invite(request['ics'])
        elif request['action'] == 'invite_reply':
            result = invite_reply(request['ics'], request['email'], request.get('name', ''), request['partstat'])
        else:
            client = Client(request['username'], request['password'])
            result = discover(client) if request['action'] == 'discover' else sync(client, request)
        print(json.dumps(result))
    except SafeError as error:
        print(json.dumps({'error': str(error)}))
    except Exception as error:
        # No URLs, credentials or calendar contents in errors/logs.
        frames = traceback.extract_tb(error.__traceback__)
        where = frames[-1].name if frames else 'unknown'
        print(json.dumps({'error': f'Calendar sync failed ({type(error).__name__} in {where}). Local events are retained.'}))

def public_feed_url(url):
    parts = urlsplit(url)
    if (parts.scheme != 'https' or not parts.hostname or parts.username or parts.password
            or parts.port not in (None, 443)):
        raise SafeError('Subscriptions require a public HTTPS calendar URL.')
    addresses = socket.getaddrinfo(parts.hostname, 443, type=socket.SOCK_STREAM)
    if not addresses or any(not ipaddress.ip_address(item[4][0]).is_global for item in addresses):
        raise SafeError('Subscriptions cannot access private or local network addresses.')
    return url

def parse_subscription(raw):
    if not raw.strip().upper().endswith(b'END:VCALENDAR'):
        raise SafeError('The feed is not a complete iCalendar file. Cached fixtures are retained.')
    calendar = Calendar.from_ical(raw)
    if calendar.name != 'VCALENDAR':
        raise SafeError('The URL did not return an iCalendar feed.')
    today = date.today()
    # Replacing a complete snapshot handles rescheduled and cancelled fixtures,
    # without appending duplicates on each refresh.
    events = expand(raw, 'subscription', today - timedelta(days=366), today + timedelta(days=731))
    unique = {item['key']: item['event'] for item in events}
    return {'events': list(unique.values())}

def subscription(url):
    opener = build_opener(NoRedirect())
    for _ in range(5):
        public_feed_url(url)
        # Deliberately no iCloud client, Authorization header, cookies or credentials.
        request = Request(url, headers={'Accept': 'text/calendar', 'User-Agent': 'Omarchy-Mail/0.1'})
        try:
            response = opener.open(request, timeout=20)
        except HTTPError as error:
            response = error
        with response:
            if response.code in (301, 302, 303, 307, 308):
                url = urljoin(url, response.headers.get('Location', ''))
                continue
            if response.code != 200:
                raise SafeError(f'Subscription returned HTTP {response.code}. Cached fixtures are retained.')
            raw = response.read(5_000_001)
            if len(raw) > 5_000_000:
                raise SafeError('Subscription is too large. Cached fixtures are retained.')
            return parse_subscription(raw)
    raise SafeError('Too many subscription redirects.')

if __name__ == '__main__':
    main()
