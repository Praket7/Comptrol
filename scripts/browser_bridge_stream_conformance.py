"""Real streaming transport regression: deliver a small frame before EOF."""
import importlib.util
import http.server
import pathlib
import queue
import threading
import time
from unittest.mock import patch

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('host', ROOT / 'extensions/comptrol-browser-bridge/native_host.py')
host = importlib.util.module_from_spec(spec)
spec.loader.exec_module(host)
release = threading.Event()
class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers['Content-Length']))
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.end_headers()
        self.wfile.write(b'event: command\ndata: {"request_id":"stream-fixture","command_type":"bridge_ping"}\n\n')
        self.wfile.flush()
        release.wait(5)
    def log_message(self, *args):
        pass
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
result = queue.Queue()
def consume():
    stream = host.stream_events('/browser/command/stream', {}, {})
    try:
        result.put(next(stream))
    except Exception as exc:
        result.put(exc)
    finally:
        stream.close()
try:
    with patch.object(host, 'LOCAL_DAEMON_URL', f'http://127.0.0.1:{server.server_port}'), patch.object(host, 'load_bridge_token', return_value='a'*64), patch.object(host, 'verify_daemon_identity', return_value=True):
        started = time.monotonic()
        worker = threading.Thread(target=consume, daemon=True)
        worker.start()
        event = result.get(timeout=1)
        assert event == ('command', {'request_id': 'stream-fixture', 'command_type': 'bridge_ping'}), event
        assert not release.is_set(), 'must arrive while the HTTP stream is still open'
        print(f'PASS small SSE command delivered before EOF in {(time.monotonic()-started)*1000:.1f} ms')
        worker.join(timeout=1)
finally:
    release.set()
    server.shutdown()
    server.server_close()
