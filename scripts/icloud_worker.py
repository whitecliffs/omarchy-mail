"""Private JSON stdin/stdout bridge for iCloud CalDAV and iCalendar parsing."""
import base64
from datetime import date, datetime, time, timedelta, timezone
import json
import signal
import sys
import traceback
import ipaddress
import socket
from urllib.parse import urljoin, urlsplit
from urllib.request import Request, build_opener, HTTPRedirectHandler
from urllib.error import HTTPError
from defusedxml import ElementTree as XML
from icalendar import Calendar, Event
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
    instances = (recurring_ical_events.of(calendar).between(start, end)
                 if any('RRULE' in item or 'RDATE' in item or 'RECURRENCE-ID' in item for item in components)
                 else components)
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
        }})
        if len(result) > 10000:
            raise SafeError('Too many recurring instances to display safely.')
    return result

def sync(client, request):
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
    return {'events': events, 'uploaded': uploaded}

def main():
    signal.alarm(120)
    try:
        request = json.load(sys.stdin)
        if request['action'] == 'subscription':
            result = subscription(request['url'])
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
