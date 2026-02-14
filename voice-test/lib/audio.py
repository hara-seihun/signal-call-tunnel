"""Audio utilities: tone generation, frequency detection (Goertzel), WAV I/O.

Pure Python, no numpy dependency.
"""

import math
import struct
import wave


def generate_tone(freq_hz, duration_s, sample_rate=48000, amplitude=0.8):
    """Generate PCM bytes (S16LE mono) for a sine wave at freq_hz."""
    n_samples = int(sample_rate * duration_s)
    samples = []
    for i in range(n_samples):
        t = i / sample_rate
        value = amplitude * math.sin(2 * math.pi * freq_hz * t)
        sample = int(value * 32767)
        sample = max(-32768, min(32767, sample))
        samples.append(struct.pack("<h", sample))
    return b"".join(samples)


def goertzel_magnitude(pcm_bytes, target_freq, sample_rate=48000):
    """Goertzel algorithm: compute magnitude of a single frequency bin.

    More efficient than FFT when only one frequency is needed: O(n) time, O(1) space.
    """
    n_samples = len(pcm_bytes) // 2
    if n_samples == 0:
        return 0.0

    k = round(target_freq * n_samples / sample_rate)
    w = 2 * math.pi * k / n_samples
    coeff = 2 * math.cos(w)

    s0 = 0.0
    s1 = 0.0
    s2 = 0.0

    for i in range(n_samples):
        sample = struct.unpack_from("<h", pcm_bytes, i * 2)[0] / 32768.0
        s0 = sample + coeff * s1 - s2
        s2 = s1
        s1 = s0

    magnitude = math.sqrt(s1 * s1 + s2 * s2 - coeff * s1 * s2)
    return magnitude / n_samples


def detect_tone(pcm_bytes, expected_freq, sample_rate=48000, threshold=3.0):
    """Returns True if expected_freq is the dominant frequency in the signal.

    Opus encoding and WebRTC audio processing (AGC, NS, AEC) can shift the
    tone by up to ~50 Hz and spread energy across nearby bins. To handle this,
    we scan a ±100 Hz window around the expected frequency and take the peak
    magnitude as the signal level. Noise is measured from bins well outside
    this window.
    """
    if len(pcm_bytes) < 1920:  # Less than 1 frame
        return False

    # Scan a window around the expected frequency to find the peak.
    # Opus codec can shift the tone by ~30-50 Hz.
    scan_step = 10
    scan_range = 100  # Hz each side
    peak_mag = 0.0
    peak_freq = expected_freq
    for f in range(expected_freq - scan_range, expected_freq + scan_range + 1, scan_step):
        if f < 50 or f >= sample_rate // 2:
            continue
        mag = goertzel_magnitude(pcm_bytes, f, sample_rate)
        if mag > peak_mag:
            peak_mag = mag
            peak_freq = f

    # Measure noise at frequencies well outside the signal window.
    noise_freqs = []
    for offset in [-600, -400, 400, 600]:
        f = expected_freq + offset
        if 50 < f < sample_rate // 2:
            noise_freqs.append(f)

    if not noise_freqs:
        return peak_mag > 0.001

    avg_noise = sum(
        goertzel_magnitude(pcm_bytes, f, sample_rate) for f in noise_freqs
    ) / len(noise_freqs)

    if avg_noise < 1e-8:
        return peak_mag > 0.001

    ratio = peak_mag / avg_noise
    print(f"    [audio] detect_tone({expected_freq}Hz): peak={peak_mag:.6f}@{peak_freq}Hz, noise={avg_noise:.6f}, ratio={ratio:.1f} (threshold={threshold})")
    return ratio >= threshold


def pcm_to_wav(pcm_bytes, path, sample_rate=48000):
    """Write raw PCM (S16LE mono) to a WAV file."""
    wf = wave.open(str(path), "wb")
    wf.setnchannels(1)
    wf.setsampwidth(2)
    wf.setframerate(sample_rate)
    wf.writeframes(pcm_bytes)
    wf.close()


def wav_to_pcm(path):
    """Read a WAV file and return raw PCM bytes (S16LE mono)."""
    wf = wave.open(str(path), "rb")
    pcm = wf.readframes(wf.getnframes())
    wf.close()
    return pcm


def rms_level(pcm_bytes):
    """RMS amplitude of PCM data (0.0 = silence, 1.0 = full scale)."""
    n_samples = len(pcm_bytes) // 2
    if n_samples == 0:
        return 0.0
    sum_sq = 0.0
    for i in range(n_samples):
        sample = struct.unpack_from("<h", pcm_bytes, i * 2)[0] / 32768.0
        sum_sq += sample * sample
    return math.sqrt(sum_sq / n_samples)
