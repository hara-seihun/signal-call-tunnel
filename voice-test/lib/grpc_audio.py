"""Emulator gRPC audio injection and capture client.

Uses compiled proto stubs from the Android emulator's emulator_controller.proto.
Supports 48kHz mono S16LE audio matching the media socket format.

Handles both unauthenticated (-grpc flag) and JWT-authenticated emulator gRPC.
"""

import json
import os
import time
from pathlib import Path


def _get_stubs():
    """Lazy-import the generated proto stubs."""
    from lib.proto import emulator_controller_pb2 as pb
    from lib.proto import emulator_controller_pb2_grpc as pb_grpc
    return pb, pb_grpc


class EmulatorAudio:
    """Client for the emulator's gRPC audio streaming API."""

    def __init__(self, host="localhost", port=8554, token=None):
        import grpc
        self._grpc = grpc
        self._token = token
        self._host = host
        self._port = port

        self.channel = grpc.insecure_channel(f"{host}:{port}")
        pb, pb_grpc = _get_stubs()
        self.stub = pb_grpc.EmulatorControllerStub(self.channel)
        self.pb = pb

        # If no token provided, probe connectivity and auto-discover if needed
        if self._token is None:
            self._try_connect_or_discover()

    def _metadata(self):
        """Return gRPC call metadata with auth header if token is set."""
        if self._token:
            return [('authorization', f'Bearer {self._token}')]
        return []

    def _try_connect_or_discover(self):
        """Test unauthenticated connectivity; fall back to token discovery."""
        import grpc
        try:
            grpc.channel_ready_future(self.channel).result(timeout=3)
            # Channel connected — try a trivial call to check auth
            self.stub.getStatus(self.pb.Empty(), timeout=3)
        except grpc.RpcError as e:
            if e.code() == grpc.StatusCode.UNAUTHENTICATED:
                print("    [grpc] Unauthenticated — attempting token discovery...")
                token = self.discover_token()
                if token:
                    self._token = token
                    print("    [grpc] Token discovered, retrying with auth...")
                else:
                    print("    [grpc] No token found. Restart emulator with: -grpc <port>")
            else:
                # Non-auth error (e.g. connection refused) — let caller handle
                pass
        except Exception:
            pass

    @classmethod
    def discover_token(cls):
        """Search emulator discovery directories for a gRPC auth token.

        The emulator writes discovery files to:
          - ~/.android/avd/running/  (Linux/macOS default)
          - $TMPDIR/avd/running/     (macOS alternate)

        Each running emulator creates a pid_NNNNN.ini with grpc.token or
        a corresponding .jwk file.
        """
        search_dirs = []

        # ~/.android/avd/running/
        android_home = Path.home() / ".android" / "avd" / "running"
        if android_home.is_dir():
            search_dirs.append(android_home)

        # $TMPDIR/avd/running/
        tmpdir = os.environ.get("TMPDIR", "/tmp")
        tmpdir_running = Path(tmpdir) / "avd" / "running"
        if tmpdir_running.is_dir():
            search_dirs.append(tmpdir_running)

        for d in search_dirs:
            # Look for pid_*.ini files that contain grpc.token
            for ini_file in sorted(d.glob("pid_*.ini"), reverse=True):
                try:
                    text = ini_file.read_text()
                    for line in text.splitlines():
                        if line.startswith("grpc.token="):
                            token = line.split("=", 1)[1].strip()
                            if token:
                                return token
                except OSError:
                    continue

            # Look for .jwk files (JSON Web Key — contains the token)
            for jwk_file in sorted(d.glob("*.jwk"), reverse=True):
                try:
                    data = json.loads(jwk_file.read_text())
                    # The emulator JWK file format varies; look for common keys
                    if isinstance(data, dict):
                        token = data.get("token") or data.get("grpc_token")
                        if token:
                            return token
                except (OSError, json.JSONDecodeError):
                    continue

        return None

    def close(self):
        self.channel.close()

    def inject_audio(self, pcm_bytes, sample_rate=48000):
        """Inject PCM audio into the emulator's virtual microphone.

        Client-streaming RPC: first packet includes AudioFormat, all include audio data.
        """
        pb = self.pb
        metadata = self._metadata()
        audio_format = pb.AudioFormat(
            samplingRate=sample_rate,
            channels=pb.AudioFormat.Mono,
            format=pb.AudioFormat.AUD_FMT_S16,
            mode=pb.AudioFormat.MODE_UNSPECIFIED,
        )

        def _packet_generator():
            # Send in chunks matching 10ms frames (960 bytes at 48kHz mono S16LE)
            frame_size = sample_rate * 2 // 100  # 10ms worth of bytes
            offset = 0
            first = True
            while offset < len(pcm_bytes):
                chunk = pcm_bytes[offset:offset + frame_size]
                if first:
                    yield pb.AudioPacket(format=audio_format, audio=chunk)
                    first = False
                else:
                    yield pb.AudioPacket(audio=chunk)
                offset += frame_size
                # Pace the injection to approximate real-time
                time.sleep(0.008)  # slightly less than 10ms to avoid underruns

        self.stub.injectAudio(_packet_generator(), metadata=metadata)

    def capture_audio(self, duration_s, sample_rate=48000):
        """Capture audio from the emulator's speaker output.

        Server-streaming RPC: returns concatenated PCM bytes.
        """
        pb = self.pb
        metadata = self._metadata()
        audio_format = pb.AudioFormat(
            samplingRate=sample_rate,
            channels=pb.AudioFormat.Mono,
            format=pb.AudioFormat.AUD_FMT_S16,
        )

        deadline = time.monotonic() + duration_s + 1.0
        chunks = []
        total_bytes = 0
        target_bytes = int(sample_rate * 2 * duration_s)  # S16 mono

        try:
            for packet in self.stub.streamAudio(
                audio_format,
                timeout=duration_s + 5,
                metadata=metadata,
            ):
                if packet.audio:
                    chunks.append(packet.audio)
                    total_bytes += len(packet.audio)
                if total_bytes >= target_bytes:
                    break
                if time.monotonic() > deadline:
                    break
        except Exception as e:
            print(f"    [grpc] streamAudio ended: {e}")

        return b"".join(chunks)
