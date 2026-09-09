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

if __name__ == '__main__': unittest.main()
