import unittest
from datetime import date
from unittest.mock import patch
from icalendar import Calendar
import icloud_worker as worker

EVENT = {'id': 42, 'title': 'Review, planning; café', 'notes': 'First line\nSecond line',
         'location': 'Home', 'starts_at': '2026-09-10 12:00', 'ends_at': '2026-09-10 13:00'}
URL = 'https://p01-caldav.icloud.com/123/calendars/home/'

class CalendarTests(unittest.TestCase):
    def test_subscription_parses_deduplicates_and_ignores_cancellations(self):
        raw = worker.serialize(EVENT, 'fixture')
        result = worker.parse_subscription(raw)
        self.assertEqual(len(result['events']), 1)
        self.assertEqual(result['events'][0]['title'], EVENT['title'])
        calendar = Calendar.from_ical(raw)
        calendar.add_component(calendar.walk('VEVENT')[0].copy())
        self.assertEqual(len(worker.parse_subscription(calendar.to_ical())['events']), 1)
        cancelled = raw.replace(b'BEGIN:VEVENT', b'BEGIN:VEVENT\r\nSTATUS:CANCELLED')
        self.assertEqual(worker.parse_subscription(cancelled)['events'], [])

    def test_subscription_rejects_incomplete_feed(self):
        for raw in [b'<html>Login</html>', b'BEGIN:VCALENDAR\r\n']:
            with self.assertRaises(worker.SafeError): worker.parse_subscription(raw)

    def test_subscription_rejects_private_destinations(self):
        for address in ['127.0.0.1', '192.168.1.130', '169.254.169.254', '::1']:
            with patch.object(worker.socket, 'getaddrinfo', return_value=[(0,0,0,'',(address,443))]):
                with self.assertRaises(worker.SafeError): worker.public_feed_url('https://example.com/a.ics')
        for url in ['file:///etc/passwd', 'http://example.com', 'https://user:pass@example.com']:
            with self.assertRaises(worker.SafeError): worker.public_feed_url(url)

    def test_subscription_download_has_no_icloud_credentials(self):
        class Response:
            code = 200
            def __enter__(self): return self
            def __exit__(self, *args): pass
            def read(self, limit): return worker.serialize(EVENT, 'fixture')
        with patch.object(worker, 'public_feed_url'), patch.object(worker, 'build_opener') as opener:
            opener.return_value.open.return_value = Response()
            worker.subscription('https://example.com/feed.ics')
            request = opener.return_value.open.call_args.args[0]
            self.assertFalse(request.has_header('Authorization'))
            self.assertEqual(request.get_method(), 'GET')

    def test_rejects_untrusted_destinations(self):
        for url in ['http://icloud.com/', 'https://icloud.com.evil.test/',
                    'https://evilicloud.com/', 'https://user:pass@icloud.com/', 'https://icloud.com:8443/']:
            with self.assertRaises(worker.SafeError): worker.trusted(url)
        self.assertEqual(worker.trusted(URL), URL)

    def test_unicode_and_multiline_round_trip(self):
        raw = worker.serialize(EVENT, 'stable-42')
        event = Calendar.from_ical(raw).walk('VEVENT')[0]
        self.assertEqual(str(event['summary']), EVENT['title'])
        self.assertEqual(str(event['description']), EVENT['notes'])
        expanded = worker.expand(raw, URL + 'stable-42.ics', date(2026,1,1), date(2027,1,1))
        self.assertEqual(expanded[0]['event']['starts_at'], EVENT['starts_at'])

    def test_all_day_and_recurring_exception(self):
        raw = b'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:repeat\r\nDTSTART;VALUE=DATE:20260910\r\nDTEND;VALUE=DATE:20260911\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEXDATE;VALUE=DATE:20260911\r\nSUMMARY:Daily\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n'
        events = worker.expand(raw, URL+'repeat.ics', date(2026,1,1), date(2027,1,1))
        self.assertEqual([e['event']['starts_at'] for e in events], ['2026-09-10 00:00','2026-09-12 00:00'])
        self.assertNotEqual(events[0]['key'], events[1]['key'])

    def test_single_event_outside_recurrence_window_is_retained(self):
        events = worker.expand(worker.serialize(EVENT, 'stable'), URL+'stable.ics', date(2030,1,1), date(2031,1,1))
        self.assertEqual(len(events), 1)

    def test_upload_retry_checks_uid_without_overwrite(self):
        class FakeClient:
            def request(self, method, url, body=None, headers=None, acceptable=None):
                if method == 'REPORT': return 207, b'<d:multistatus xmlns:d="DAV:"/>', URL
                if method == 'PUT':
                    self.headers = headers
                    return 412, b'', url
                return 200, worker.serialize(EVENT, 'stable-42'), url
        client = FakeClient()
        result = worker.sync(client, {'calendar_url':URL, 'namespace':'stable','pending':[EVENT]})
        self.assertEqual(client.headers['If-None-Match'], '*')
        self.assertEqual(result['uploaded'][0]['id'], 42)
        self.assertEqual(len(result['events']), 1)

    def test_bad_snapshot_never_returns_empty_success(self):
        class FakeClient:
            def request(self, *args): return 207, b'<html/>', URL
        with self.assertRaises(worker.SafeError):
            worker.sync(FakeClient(), {'calendar_url':URL})

    def test_icloud_collection_metadata_is_not_an_event(self):
        from xml.sax.saxutils import escape
        raw = worker.serialize(EVENT, 'stable-42').decode()
        report = f'''<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
        <d:response><d:href>{URL}</d:href>
          <d:propstat><d:prop><d:getetag>collection</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
          <d:propstat><d:prop><c:calendar-data/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
        </d:response>
        <d:response><d:href>{URL}stable-42.ics</d:href>
          <d:propstat><d:prop><c:calendar-data>{escape(raw)}</c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
        </d:response></d:multistatus>'''.encode()
        class FakeClient:
            def request(self, method, *args):
                self_method = method
                if self_method != 'REPORT': raise AssertionError('Must not GET the collection')
                return 207, report, URL
        result = worker.sync(FakeClient(), {'calendar_url':URL, 'pending':[]})
        self.assertEqual(len(result['events']), 1)

    def test_missing_inline_event_is_downloaded_separately(self):
        report = f'''<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
        <d:response><d:href>{URL}stable-42.ics</d:href>
          <d:propstat><d:prop><d:getetag>event</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
          <d:propstat><d:prop><c:calendar-data/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
        </d:response></d:multistatus>'''.encode()
        class FakeClient:
            def request(self, method, *args):
                if method == 'REPORT': return 207, report, URL
                return 200, worker.serialize(EVENT, 'stable-42'), URL+'stable-42.ics'
        result = worker.sync(FakeClient(), {'calendar_url':URL, 'pending':[]})
        self.assertEqual(len(result['events']), 1)

    def test_invalid_end_time_not_uploaded(self):
        with self.assertRaises(worker.SafeError):
            worker.serialize({**EVENT, 'ends_at':'2026-09-10 11:00'}, 'invalid')

class GoogleCalendarTests(unittest.TestCase):
    CAL = 'family123@group.calendar.google.com'

    def test_rejects_untrusted_google_destinations(self):
        for url in ['http://www.googleapis.com/calendar/v3/x', 'https://googleapis.com.evil.test/calendar/v3/',
                    'https://www.googleapis.com/drive/v3/files', 'https://www.googleapis.com:8443/calendar/v3/x']:
            with self.assertRaises(worker.SafeError): worker.google_trusted(url)
        url = worker.google_calendar_url(self.CAL)
        self.assertEqual(worker.google_trusted(url), url)
        self.assertIn('family123%40group.calendar.google.com', url)

    def test_event_ids_are_stable_base32hex(self):
        first = worker.google_event_id('3f2a-9c11-ffee', 42)
        self.assertEqual(first, worker.google_event_id('3f2a-9c11-ffee', 42))
        self.assertNotEqual(first, worker.google_event_id('3f2a-9c11-ffee', 43))
        self.assertTrue(all(ch in '0123456789abcdefghijklmnopqrstuv' for ch in first))

    def test_maps_timed_all_day_recurring_and_cancelled_events(self):
        timed = worker.google_event(self.CAL, {'id': 'abc', 'summary': 'Dentist',
            'start': {'dateTime': '2026-09-10T12:00:00Z'}, 'end': {'dateTime': '2026-09-10T13:00:00Z'}})
        self.assertEqual(timed['event']['title'], 'Dentist')
        self.assertEqual(timed['href'], f'google:{self.CAL}#abc')
        allday = worker.google_event(self.CAL, {'id': 'r_20260910', 'recurringEventId': 'r',
            'start': {'date': '2026-09-10'}, 'end': {'date': '2026-09-11'}})
        self.assertEqual(allday['event']['starts_at'], '2026-09-10 00:00')
        self.assertEqual(allday['event']['title'], '(Untitled)')
        self.assertEqual(allday['href'], f'google:{self.CAL}#r')
        self.assertNotEqual(allday['key'], allday['href'])
        self.assertIsNone(worker.google_event(self.CAL, {'id': 'x', 'status': 'cancelled'}))

    def test_sync_pages_and_retried_upload_does_not_duplicate(self):
        remote_id = worker.google_event_id('stable', 42)
        saved = worker.google_body(EVENT, remote_id)
        class FakeClient:
            calls = []
            def pages(self, url, params):
                self.params = params
                yield {'items': [{'id': 'one', 'start': {'date': '2026-01-01'}, 'end': {'date': '2026-01-02'}}]}
                yield {'items': [{'id': 'two', 'status': 'cancelled'}]}
            def request(self, method, url, body=None, acceptable=(200,)):
                self.calls.append(method)
                return (409, {}) if method == 'POST' else (200, saved)
        client = FakeClient()
        result = worker.google_sync(client, {'calendar_url': self.CAL, 'namespace': 'stable', 'pending': [EVENT]})
        self.assertEqual(client.params['singleEvents'], 'true')
        self.assertEqual(client.calls, ['POST', 'GET'])
        self.assertEqual(result['uploaded'], [{'id': 42, 'href': f'google:{self.CAL}#{remote_id}'}])
        self.assertEqual(sorted(e['event']['title'] for e in result['events']), ['(Untitled)', EVENT['title']])

    def test_upload_body_keeps_local_times_and_rejects_bad_ranges(self):
        body = worker.google_body(EVENT, 'abcde')
        self.assertTrue(body['start']['dateTime'].startswith('2026-09-10T12:00:00'))
        with self.assertRaises(worker.SafeError):
            worker.google_body({**EVENT, 'ends_at': '2026-09-10 11:00'}, 'abcde')

    def test_discover_lists_primary_first_and_requires_writable(self):
        class FakeClient:
            def pages(self, url, params):
                self.params = params
                yield {'items': [{'id': 'b', 'summary': 'Work'}, {'id': 'me@gmail.com', 'summary': 'Me', 'primary': True}]}
        client = FakeClient()
        result = worker.google_discover(client)
        self.assertEqual(client.params['minAccessRole'], 'writer')
        self.assertEqual([c['url'] for c in result['calendars']], ['me@gmail.com', 'b'])

INVITE = ('BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:meet-1@example.com\r\n'
          'SEQUENCE:2\r\nDTSTART:20260910T120000Z\r\nDTEND:20260910T130000Z\r\nSUMMARY:Planning\r\n'
          'LOCATION:Room 4\r\nORGANIZER;CN=Ann Boss:mailto:ann@example.com\r\n'
          'ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:Jim@Example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n')

class ChangeAndInviteTests(unittest.TestCase):
    def test_recurring_flag_marks_series_instances(self):
        raw = b'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:r\r\nDTSTART:20260910T120000Z\r\nDTEND:20260910T130000Z\r\nRRULE:FREQ=DAILY;COUNT=2\r\nSUMMARY:R\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n'
        self.assertTrue(all(e['recurring'] for e in worker.expand(raw, URL+'r.ics', date(2026,1,1), date(2027,1,1))))
        self.assertFalse(worker.expand(worker.serialize(EVENT, 's'), URL+'s.ics', date(2026,1,1), date(2027,1,1))[0]['recurring'])

    def test_icloud_edit_keeps_other_properties_and_uses_etag(self):
        original = INVITE.replace('METHOD:REQUEST\r\n', '').encode()
        class FakeClient:
            headers = {}
            calls = []
            def request(self, method, url, body=None, headers=None, acceptable=None):
                self.calls.append((method, headers))
                if method == 'GET':
                    self.headers = {'ETag': '"v1"'}
                    return 200, original, url
                if method == 'PUT': self.body = body
                return 204, b'', url
        client = FakeClient()
        edited = {**EVENT, 'title': 'Planning (moved)', 'starts_at': '2026-09-11 09:00', 'ends_at': '2026-09-11 10:00'}
        applied, dropped = worker.apply_icloud_changes(client, [
            {'id': 1, 'action': 'update', 'href': URL+'meet.ics', 'event': edited},
            {'id': 2, 'action': 'delete', 'href': URL+'gone.ics', 'event': EVENT}])
        self.assertEqual((applied, dropped), ([1, 2], []))
        self.assertEqual(client.calls[1][1]['If-Match'], '"v1"')
        saved = Calendar.from_ical(client.body).walk('VEVENT')[0]
        self.assertEqual(str(saved['summary']), 'Planning (moved)')
        self.assertIn('ATTENDEE', saved)
        self.assertEqual(int(saved['sequence']), 3)
        self.assertEqual(client.calls[2][0], 'DELETE')

    def test_icloud_edit_conflict_or_series_is_dropped_not_overwritten(self):
        class FakeClient:
            headers = {}
            def __init__(self, raw, put_status): self.raw, self.put_status = raw, put_status
            def request(self, method, url, body=None, headers=None, acceptable=None):
                return (200, self.raw, url) if method == 'GET' else (self.put_status, b'', url)
        change = [{'id': 7, 'action': 'update', 'href': URL+'x.ics', 'event': EVENT}]
        self.assertEqual(worker.apply_icloud_changes(FakeClient(worker.serialize(EVENT, 'x'), 412), change), ([], [7]))
        series = b'BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:r\r\nDTSTART:20260910T120000Z\r\nRRULE:FREQ=DAILY\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n'
        self.assertEqual(worker.apply_icloud_changes(FakeClient(series, 204), change), ([], [7]))

    def test_google_edit_patches_all_day_dates(self):
        class FakeClient:
            def request(self, method, url, body=None, acceptable=(200,)):
                self.method, self.url, self.body = method, url, body
                return 200, {}
        client = FakeClient()
        allday = {**EVENT, 'starts_at': '2026-09-10 00:00', 'ends_at': '2026-09-11 00:00'}
        applied, _ = worker.apply_google_changes(client, 'cal', [
            {'id': 3, 'action': 'update', 'remote_key': 'google:cal#abc', 'event': allday}])
        self.assertEqual(applied, [3])
        self.assertEqual(client.method, 'PATCH')
        self.assertTrue(client.url.endswith('/events/abc'))
        self.assertEqual(client.body['start'], {'date': '2026-09-10'})

    def test_tasks_sync_pushes_then_lists(self):
        class FakeClient:
            calls = []
            def request(self, method, url, body=None, acceptable=(200,)):
                self.calls.append((method, body))
                return 200, {'id': 'new-remote'}
            def pages(self, url, params):
                self.params = params
                yield {'items': [{'id': 'r1', 'title': 'Buy milk', 'status': 'completed', 'due': '2026-12-01T00:00:00.000Z'},
                                 {'id': 'gone', 'deleted': True}]}
        client = FakeClient()
        todo = {'id': 5, 'title': 'Call Ann', 'notes': '', 'due_at': '2026-12-02', 'completed': False}
        result = worker.tasks_sync(client, {'list_id': '@default', 'creates': [{'todo': todo, 'revision': 1}],
                                            'updates': [{'remote_id': 'r1', 'todo': {**todo, 'id': 6}, 'revision': 4}],
                                            'deletes': [{'id': 9, 'remote_id': 'old'}]})
        self.assertEqual([c[0] for c in client.calls], ['DELETE', 'PATCH', 'POST'])
        self.assertEqual(client.calls[2][1]['due'], '2026-12-02T00:00:00.000Z')
        self.assertEqual(result['created'], [{'id': 5, 'remote_id': 'new-remote', 'revision': 1}])
        self.assertEqual(result['pushed'], [{'id': 6, 'revision': 4}])
        self.assertEqual(result['deletions_done'], [9])
        self.assertEqual(result['remote'], [{'remote_id': 'r1', 'title': 'Buy milk', 'notes': '', 'due_at': '2026-12-01', 'completed': True}])
        self.assertTrue(worker.google_trusted('https://tasks.googleapis.com/tasks/v1/lists/x/tasks'))
        with self.assertRaises(worker.SafeError): worker.google_trusted('https://tasks.googleapis.com/drive/v3/x')

    def test_parses_invitation_and_builds_reply(self):
        invite = worker.parse_invite(INVITE)
        self.assertEqual((invite['method'], invite['title'], invite['organizer_email']), ('REQUEST', 'Planning', 'ann@example.com'))
        self.assertEqual(invite['organizer_name'], 'Ann Boss')
        self.assertIn('jim@example.com', invite['attendees'])
        self.assertFalse(invite['cancelled'])
        reply = Calendar.from_ical(worker.invite_reply(INVITE, 'jim@example.com', 'Jim', 'ACCEPTED')['ics'])
        self.assertEqual(str(reply['method']), 'REPLY')
        event = reply.walk('VEVENT')[0]
        self.assertEqual(str(event['uid']), 'meet-1@example.com')
        self.assertEqual(event['attendee'].params['PARTSTAT'], 'ACCEPTED')
        with self.assertRaises(worker.SafeError): worker.invite_reply(INVITE, 'a@b', '', 'MAYBE')
        self.assertTrue(worker.parse_invite(INVITE.replace('METHOD:REQUEST', 'METHOD:CANCEL'))['cancelled'])

if __name__ == '__main__': unittest.main()
