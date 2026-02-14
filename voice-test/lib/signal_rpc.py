"""JSON-RPC client for signal-cli daemon over Unix socket."""

import json
import socket
import time


class SignalRPC:
    """Connects to signal-cli's JSON-RPC Unix socket and provides call control methods."""

    SAMPLE_RATE = 48000
    CHANNELS = 1
    PTIME_MS = 10
    SAMPLES_PER_FRAME = SAMPLE_RATE * PTIME_MS // 1000  # 480
    BYTES_PER_SAMPLE = 2  # 16-bit signed LE
    PCM_FRAME_SIZE = SAMPLES_PER_FRAME * BYTES_PER_SAMPLE  # 960

    def __init__(self, socket_path):
        self.socket_path = socket_path
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(socket_path)
        self._next_id = 0
        self._buf = b""

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass

    def _send(self, method, params=None):
        self._next_id += 1
        req = {"jsonrpc": "2.0", "method": method, "id": self._next_id}
        if params:
            req["params"] = params
        line = json.dumps(req) + "\n"
        self.sock.sendall(line.encode("utf-8"))
        return self._next_id

    def _read_lines(self):
        """Yield complete newline-delimited JSON strings from socket."""
        # Drain any complete lines already in the buffer from a previous recv
        while b"\n" in self._buf:
            line, self._buf = self._buf.split(b"\n", 1)
            line = line.strip()
            if line:
                yield line.decode("utf-8")
        while True:
            data = self.sock.recv(4096)
            if not data:
                return
            self._buf += data
            while b"\n" in self._buf:
                line, self._buf = self._buf.split(b"\n", 1)
                line = line.strip()
                if line:
                    yield line.decode("utf-8")

    def _wait_response(self, req_id, timeout=10):
        """Wait for a JSON-RPC response matching req_id, buffering notifications."""
        self.sock.settimeout(timeout)
        try:
            for line in self._read_lines():
                msg = json.loads(line)
                if "id" in msg and msg["id"] == req_id:
                    if "error" in msg:
                        raise RuntimeError(f"RPC error: {msg['error']}")
                    return msg.get("result")
                # Buffer notification for later consumption
                self._pending_events.append(msg)
        except socket.timeout:
            raise TimeoutError(f"Timed out waiting for response to request {req_id}")
        finally:
            self.sock.settimeout(None)

    def subscribe_receive(self):
        self._pending_events = getattr(self, "_pending_events", [])
        req_id = self._send("subscribeReceive")
        return self._wait_response(req_id)

    def start_call(self, recipient):
        self._pending_events = getattr(self, "_pending_events", [])
        req_id = self._send("startCall", {"recipient": [recipient]})
        return self._wait_response(req_id, timeout=30)

    def accept_call(self, call_id):
        self._pending_events = getattr(self, "_pending_events", [])
        req_id = self._send("acceptCall", {"call-id": call_id})
        return self._wait_response(req_id, timeout=30)

    def reject_call(self, call_id):
        self._pending_events = getattr(self, "_pending_events", [])
        req_id = self._send("rejectCall", {"call-id": call_id})
        return self._wait_response(req_id, timeout=10)

    def hangup_call(self, call_id):
        self._pending_events = getattr(self, "_pending_events", [])
        req_id = self._send("hangupCall", {"call-id": call_id})
        return self._wait_response(req_id, timeout=10)

    def list_calls(self):
        self._pending_events = getattr(self, "_pending_events", [])
        req_id = self._send("listCalls")
        return self._wait_response(req_id, timeout=10)

    def read_event(self, timeout=30):
        """Read the next JSON-RPC notification (callEvent, receive, etc.)."""
        self._pending_events = getattr(self, "_pending_events", [])
        # Return buffered events first
        if self._pending_events:
            return self._pending_events.pop(0)
        self.sock.settimeout(timeout)
        try:
            for line in self._read_lines():
                msg = json.loads(line)
                if "id" in msg and "method" not in msg:
                    # This is a response, buffer it (shouldn't normally happen here)
                    self._pending_events.append(msg)
                    continue
                return msg
        except socket.timeout:
            raise TimeoutError(f"No event received within {timeout}s")
        finally:
            self.sock.settimeout(None)
        return None

    def wait_for_state(self, target_state, timeout=60):
        """Block until a callEvent with the given state arrives. Returns the event params."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            try:
                msg = self.read_event(timeout=remaining)
            except TimeoutError:
                break
            if msg and msg.get("method") == "callEvent":
                params = msg.get("params", {})
                event = params.get("callEvent", {})
                state = event.get("state")
                print(f"    [rpc] callEvent: {state} (reason={event.get('reason')})")
                if state == target_state:
                    return params
        raise TimeoutError(f"Did not reach state {target_state} within {timeout}s")
