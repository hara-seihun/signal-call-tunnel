"""ADB-based Signal UI automation for the Android emulator.

Uses a combination of:
- adb shell uiautomator dump for dynamic UI element discovery
- adb shell commands for taps, key events, intents
- logcat polling for state verification
- telecom shell commands for call answer/reject (no coordinates needed)
"""

import os
import re
import subprocess
import time
import xml.etree.ElementTree as ET


class EmulatorControl:
    """Drives Signal on the Android emulator via adb shell commands.

    Uses uiautomator dump for dynamic element lookup (with timeout/retry)
    and telecom commands for call answer/reject. Falls back to keyevents
    when telecom commands fail.
    """

    def __init__(self, adb_path, output_dir=None):
        self.adb = adb_path
        self._output_dir = output_dir

    def _run(self, *args, timeout=15):
        cmd = [self.adb] + list(args)
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return result.stdout.strip()

    def _shell(self, cmd, timeout=15):
        return self._run("shell", cmd, timeout=timeout)

    # ------------------------------------------------------------------
    # UI element discovery via uiautomator dump
    # ------------------------------------------------------------------

    _UI_DUMP_PATH = "/sdcard/window_dump.xml"

    def _dump_ui(self, timeout=5):
        """Dump the current UI hierarchy as an ElementTree.

        Dumps to a file on the device then reads it back (more reliable
        than piping to /dev/stdout which drops XML on many emulators).
        Retries once on failure. Returns the parsed XML root or None.
        """
        for attempt in range(2):
            try:
                # Dump to file on device
                result = subprocess.run(
                    [self.adb, "shell", "uiautomator", "dump",
                     self._UI_DUMP_PATH],
                    capture_output=True, text=True, timeout=timeout,
                )
                if "dumped to" not in result.stdout.lower():
                    if attempt == 0:
                        time.sleep(1)
                        continue
                    print(f"    [emu] uiautomator dump failed: {result.stdout.strip()}")
                    return None

                # Read the file back
                xml_text = self._shell(f"cat {self._UI_DUMP_PATH}", timeout=5)
                if not xml_text or "<hierarchy" not in xml_text:
                    if attempt == 0:
                        time.sleep(1)
                        continue
                    print("    [emu] uiautomator dump returned no hierarchy XML")
                    return None
                start = xml_text.index("<hierarchy")
                end = xml_text.rindex(">") + 1
                xml_text = xml_text[start:end]
                if self._output_dir:
                    try:
                        dump_path = os.path.join(self._output_dir, "window_dump.xml")
                        with open(dump_path, "w") as f:
                            f.write(xml_text)
                    except OSError:
                        pass
                return ET.fromstring(xml_text)
            except subprocess.TimeoutExpired:
                if attempt == 0:
                    print("    [emu] uiautomator dump timed out, retrying...")
                    continue
                print("    [emu] uiautomator dump timed out twice")
                return None
            except (ET.ParseError, ValueError) as e:
                if attempt == 0:
                    time.sleep(1)
                    continue
                print(f"    [emu] uiautomator dump parse error: {e}")
                return None
        return None

    def _parse_bounds(self, bounds_str):
        """Parse a bounds attribute like '[0,0][1080,1920]' into (cx, cy)."""
        m = re.match(r"\[(\d+),(\d+)\]\[(\d+),(\d+)\]", bounds_str)
        if not m:
            return None
        x1, y1, x2, y2 = int(m.group(1)), int(m.group(2)), int(m.group(3)), int(m.group(4))
        return ((x1 + x2) // 2, (y1 + y2) // 2)

    def _find_element(self, root=None, text=None, content_desc=None,
                      resource_id_contains=None, class_name=None):
        """Find a UI element in the hierarchy and return its center (x, y).

        If root is None, calls _dump_ui() to get the current hierarchy.
        Searches for a node matching ALL provided criteria.
        Returns (center_x, center_y) or None if not found.
        """
        if root is None:
            root = self._dump_ui()
        if root is None:
            return None

        for node in root.iter("node"):
            if text is not None and node.get("text", "") != text:
                continue
            if content_desc is not None and content_desc not in node.get("content-desc", ""):
                continue
            if resource_id_contains is not None and resource_id_contains not in node.get("resource-id", ""):
                continue
            if class_name is not None and node.get("class", "") != class_name:
                continue

            bounds = node.get("bounds", "")
            center = self._parse_bounds(bounds)
            if center:
                return center
        return None

    def _tap_element(self, **kwargs):
        """Find an element via _find_element() and tap its center.

        Returns True if the element was found and tapped, False otherwise.
        Passes all kwargs through to _find_element().
        """
        center = self._find_element(**kwargs)
        if center is None:
            criteria = {k: v for k, v in kwargs.items() if v is not None and k != "root"}
            print(f"    [emu] Element not found: {criteria}")
            return False
        self.tap(*center)
        return True

    # ------------------------------------------------------------------
    # Signal lifecycle
    # ------------------------------------------------------------------

    def kill_signal(self):
        """Force-stop Signal. Clears any stuck dialogs, notifications, etc."""
        self._shell("am force-stop org.thoughtcrime.securesms")
        time.sleep(1)

    def launch_signal(self):
        """Launch Signal to its main chat list screen and wait for it."""
        self._shell(
            "monkey -p org.thoughtcrime.securesms "
            "-c android.intent.category.LAUNCHER 1"
        )
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            focus = self._shell("dumpsys window | grep mCurrentFocus")
            if "org.thoughtcrime.securesms" in focus:
                return True
            time.sleep(0.5)
        print("    [emu] Warning: Signal may not be in foreground")
        return False

    def restart_signal(self):
        """Kill and relaunch Signal to a clean state."""
        print("    [emu] Restarting Signal...")
        self.kill_signal()
        self.launch_signal()

    # ------------------------------------------------------------------
    # Navigation
    # ------------------------------------------------------------------

    def open_conversation(self, phone_number):
        """Open conversation with the given phone number.

        Kills and relaunches Signal to start from a clean chat list,
        then taps the first conversation row using uiautomator lookup.
        """
        self.restart_signal()
        time.sleep(2)  # let chat list fully render

        # Try to find and tap the first conversation row
        root = self._dump_ui()
        tapped = False
        if root is not None:
            # Try by content-desc containing the contact name/number
            tapped = self._tap_element(root=root, content_desc=phone_number)
            if not tapped:
                # Try finding a conversation item by resource-id
                tapped = self._tap_element(root=root, resource_id_contains="conversation")
            if not tapped:
                # Find the first clickable row below the toolbar area:
                # look for clickable FrameLayout/LinearLayout nodes
                for node in root.iter("node"):
                    if node.get("clickable") != "true":
                        continue
                    cls = node.get("class", "")
                    if cls not in ("android.widget.FrameLayout",
                                   "android.widget.LinearLayout",
                                   "android.view.ViewGroup"):
                        continue
                    bounds = node.get("bounds", "")
                    center = self._parse_bounds(bounds)
                    if center and center[1] > 200:  # below toolbar area
                        self.tap(*center)
                        tapped = True
                        break

        if not tapped:
            print("    [emu] Warning: could not find conversation row via uiautomator")

        time.sleep(2)  # let conversation load

        # Verify we're in a conversation by checking window focus
        focus = self._shell("dumpsys window | grep mCurrentFocus")
        if "org.thoughtcrime.securesms" in focus:
            print("    [emu] Conversation opened")
        else:
            print("    [emu] Warning: may not be in conversation")

    def ensure_signal_foreground(self):
        """Ensure Signal is in the foreground (for Scenario A where
        we just need Signal running, not in a specific conversation)."""
        self.restart_signal()

    # ------------------------------------------------------------------
    # Placing calls (from emulator)
    # ------------------------------------------------------------------

    def _dismiss_permission_dialogs(self):
        """Dismiss any permission dialogs that appear on the call screen.

        Signal may show camera/microphone permission prompts before the
        pre-join call screen. Tap "Not now" or "Deny" to dismiss them.
        """
        for _ in range(3):  # handle up to 3 stacked dialogs
            root = self._dump_ui()
            if root is None:
                return
            dismissed = False
            for criteria in [
                {"root": root, "text": "Not now"},
                {"root": root, "text": "Deny"},
                {"root": root, "text": "Don\u2019t allow"},
                {"root": root, "text": "Don't allow"},
            ]:
                if self._tap_element(**criteria):
                    print(f"    [emu] Dismissed permission dialog")
                    dismissed = True
                    time.sleep(0.5)
                    break
            if not dismissed:
                return

    def tap_call_button(self):
        """Tap the voice call button in the conversation header,
        then confirm the 'Start voice call?' dialog."""
        # Tap the call icon in the header (try content-desc patterns)
        tapped = self._tap_element(content_desc="Signal call")
        if not tapped:
            tapped = self._tap_element(content_desc="Voice call")
        if not tapped:
            tapped = self._tap_element(content_desc="call")
        if not tapped:
            print("    [emu] Warning: could not find call button via uiautomator")

        time.sleep(1.5)  # wait for call screen / dialog

        # Dismiss any permission dialogs (camera, microphone) that may
        # appear before the pre-join screen.
        self._dismiss_permission_dialogs()
        time.sleep(0.5)

        # On newer Signal versions, tapping the call icon opens a pre-join
        # call activity instead of a confirmation dialog. Try both flows.
        print("    [emu] Confirming call (dialog or pre-join screen)...")
        root = self._dump_ui()
        tapped = False
        if root is not None:
            # Try pre-join screen "Start Call" button first, then dialog "Call"
            for criteria in [
                {"root": root, "text": "Start Call"},
                {"root": root, "text": "Start call"},
                {"root": root, "content_desc": "Start call"},
                {"root": root, "content_desc": "Start Call"},
                {"root": root, "text": "Call"},
                {"root": root, "text": "Voice call"},
                {"root": root, "content_desc": "Voice call"},
            ]:
                if self._tap_element(**criteria):
                    tapped = True
                    break

            if not tapped:
                print("    [emu] Warning: could not find call start button")
                print("    [emu] Elements on screen:")
                for node in root.iter("node"):
                    text = node.get("text", "")
                    desc = node.get("content-desc", "")
                    click = node.get("clickable", "")
                    if text or desc:
                        bounds = node.get("bounds", "")
                        print(f"    [emu]   text={text!r} desc={desc!r} "
                              f"click={click} bounds={bounds}")

    # ------------------------------------------------------------------
    # Answering / rejecting calls (on emulator)
    # ------------------------------------------------------------------

    def _wait_for_incoming_call(self, timeout=30):
        """Poll logcat until the emulator is actually ringing.

        Waits for LocalRinging (the point where the heads-up notification
        with answer/decline buttons appears). handleReceivedOffer fires
        much earlier during ICE negotiation.
        """
        start_ts = self._shell("date '+%m-%d %H:%M:%S.000'")

        deadline = time.monotonic() + timeout
        offer_seen = False
        while time.monotonic() < deadline:
            logcat = self._shell(
                f"logcat -d -T '{start_ts}' 2>/dev/null", timeout=5
            )

            if not offer_seen and "handleReceivedOffer" in logcat:
                print("    [emu] Offer received, waiting for ringing...")
                offer_seen = True

            if "event: LOCAL_RINGING" in logcat or "handleLocalRinging" in logcat:
                print("    [emu] Phone is ringing (LocalRinging)")
                return True

            time.sleep(0.5)
        print("    [emu] Warning: incoming call did not start ringing within timeout")
        return False

    def _check_call_accepted(self, logcat_since):
        """Check logcat for handleAcceptCall (call was answered)."""
        logcat = self._shell(
            f"logcat -d -T '{logcat_since}' 2>/dev/null", timeout=5
        )
        return "handleAcceptCall" in logcat

    def _wait_for_call_accepted(self, logcat_since, timeout=3):
        """Poll logcat until handleAcceptCall appears or timeout."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self._check_call_accepted(logcat_since):
                return True
            time.sleep(0.3)
        return False

    def answer_incoming_call(self):
        """Answer an incoming call on the emulator.

        Waits for the phone to ring, then tries strategies in order:
        1. Expand notification shade, find and tap "Answer" via uiautomator
           (the heads-up notification is invisible to uiautomator, but the
           shade's notification actions ARE in the SystemUI hierarchy)
        2. KEYCODE_HEADSETHOOK (hardware button simulation)
        3. cmd telecom accept-ringing-call (only works if app uses Telecom)

        Each strategy is verified via logcat (handleAcceptCall).
        """
        self._wait_for_incoming_call(timeout=30)

        logcat_since = self._shell("date '+%m-%d %H:%M:%S.000'")
        time.sleep(1)  # let notification fully render

        # Strategy 1: expand notification shade, find Answer button
        print("    [emu] Expanding notification shade...")
        self._shell("cmd statusbar expand-notifications")
        time.sleep(1)  # let shade animate open

        root = self._dump_ui()
        tapped = False
        if root is not None:
            for criteria in [
                {"root": root, "text": "Answer"},
                {"root": root, "text": "Accept"},
                {"root": root, "content_desc": "Answer"},
                {"root": root, "content_desc": "Accept"},
            ]:
                if self._tap_element(**criteria):
                    tapped = True
                    break

        # Collapse the shade regardless of whether we found the button
        self._shell("cmd statusbar collapse")

        if tapped:
            if self._wait_for_call_accepted(logcat_since, timeout=3):
                print("    [emu] Call answered via notification tap")
                return

        # Strategy 2: HEADSETHOOK keyevent
        print("    [emu] Fallback: KEYCODE_HEADSETHOOK")
        self._shell("input keyevent 79")
        if self._wait_for_call_accepted(logcat_since, timeout=3):
            print("    [emu] Call answered via HEADSETHOOK")
            return

        # Strategy 3: telecom command (works if app registers with Telecom)
        print("    [emu] Fallback: cmd telecom accept-ringing-call")
        self._shell("cmd telecom accept-ringing-call")
        if self._wait_for_call_accepted(logcat_since, timeout=3):
            print("    [emu] Call answered via telecom command")
            return

        print("    [emu] Warning: could not confirm call was answered")

    def reject_incoming_call(self):
        """Reject an incoming call on the emulator.

        Waits for ringing, then uses the ENDCALL keyevent or telecom command.
        Verifies via logcat that the call ended.
        """
        self._wait_for_incoming_call(timeout=30)

        logcat_since = self._shell("date '+%m-%d %H:%M:%S.000'")
        time.sleep(1)

        # Use ENDCALL keyevent (keycode 6) — works without coordinates
        print("    [emu] Rejecting call via ENDCALL keyevent...")
        self._shell("input keyevent ENDCALL")

        # Verify call ended
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            logcat = self._shell(
                f"logcat -d -T '{logcat_since}' 2>/dev/null", timeout=5
            )
            if "call_concluded" in logcat or "onCallConcluded" in logcat:
                print("    [emu] Call declined")
                return
            time.sleep(0.3)

        # Fallback: try telecom end-call
        print("    [emu] Fallback: cmd telecom end-call")
        self._shell("cmd telecom end-call")
        print("    [emu] Warning: could not confirm call was declined")

    # ------------------------------------------------------------------
    # Utilities
    # ------------------------------------------------------------------

    def set_call_volume_max(self):
        """Set in-call volume to maximum by simulating volume-up key presses.

        The voice call volume can only be changed during an active call.
        We press KEYCODE_VOLUME_UP enough times to reach max (15 steps).
        """
        for _ in range(15):
            self._shell("input keyevent 24")  # KEYCODE_VOLUME_UP

    def tap(self, x, y):
        """Tap at screen coordinates."""
        self._shell(f"input tap {x} {y}")

    def is_device_online(self):
        """Check if the emulator is reachable via adb."""
        output = self._run("devices")
        return "emulator" in output and "device" in output
