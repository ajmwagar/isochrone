# isochrone

Audio over IP for converters that will never agree on a clock.

Two interfaces on two machines are nominally both 48kHz and actually differ by a
few parts per million. At 10ppm the sender produces an extra sample every couple
of seconds; the buffer between them drains or overflows on a schedule, and the
result is a click, then silence, then a click. `isochrone` is the transport and
the correction that make that not happen.

## Why not PTP

Disciplining a clock only helps if something downstream can *be* disciplined.
AES67 and Dante hardware slave their sample clocks to the network clock. Most USB
interfaces cannot: they run on their own oscillator, or they lock to the USB host
controller's frame timing, and either way there is no mechanism to tell them to
run twelve parts per million slower.

So `isochrone` changes the number of samples instead. Asynchronous sample-rate
conversion, governed by how full the playout buffer is, absorbs clock drift and
residual network timing error together. PTP becomes worth adding when several
receivers must agree with *each other* — until then it is precision about a
problem solved another way.

## Layout

- `isochrone-asrc` — the governor. Turns playout-buffer fill into a resampling
  ratio, slowly and within an audibility bound. No audio, no I/O, no network:
  the decisions live here and are testable without any of it.

- `isochrone-core` — RTP framing, L24/L16 conversion, wrap-safe clock-domain
  arithmetic, and the timestamp-addressed playout buffer.
- `isochrone` — UDP sender/receiver state, loss and latency telemetry, and the
  first blocking ALSA adapter and CLI.

## First I/O path

The first executable path is deliberately small: 48kHz stereo L24, one RTP
packet per millisecond, UDP unicast, and a configurable receiver target. It is
AES67-shaped on the wire but is not yet a conforming AES67 endpoint: there is
no SDP/SAP discovery or PTP clock identity, and the reported ASRC correction is
not applied until a production resampler lands.

On Linux, send an ALSA capture device:

```sh
isochrone send hw:Gen,0 192.168.10.74:50040
```

On macOS the same command selects an exact CoreAudio device name and sends
its first two input channels. The Studio's current path is:

```sh
isochrone send "Scarlett 18i20 4th Gen" 192.168.20.13:50040
```

Receive into an ALSA playback device with a 20ms network target:

```sh
isochrone receive hw:Gen,0 0.0.0.0:50040 20
```

The Pi's Scarlett 2i2 is shared by the existing ALSA dmix contract, whose
low-latency deployment uses a 96-frame (2ms) device period. Keep RTP at 48
frames and aggregate at the device edge:

```sh
isochrone receive hardware_dmixer 0.0.0.0:50040 20 96
```

Use the Pi's wired `192.168.20.13` address from Studio. Its Wi-Fi address
`192.168.10.74` has an asymmetric return path while Ethernet is preferred and
does not carry this UDP stream reliably.

For a repeatable Studio file test, use macOS's system decoder and send at an
explicit safe gain:

```sh
afconvert input.mp3 /tmp/isochrone-test.wav -f WAVE -d LEF32@48000 -c 2
isochrone send-wav /tmp/isochrone-test.wav 192.168.20.13:50040 -30
```

The installed Pi contract is versioned under `deploy/home-pi/`. It preserves
the existing `hardware_dmixer`, `visualizer_sink`, `jarvis_tts`, and
`visualizer_capture` interfaces while replacing the old 150ms relay and
250–500ms ALSA buffers with a 10ms relay target and 20ms buffers.

The receiver reports contiguous buffer fill, requested clock correction,
packet gaps/reordering/lateness, concealed frames, malformed packets, and ALSA
xrun recoveries once per second. These are operational signals, not debug-only
logs: reducing the target is successful only while concealment and recoveries
remain at zero.

For a same-host test without touching a physical interface, load ALSA's
`snd-aloop` module and use a paired Loopback subdevice:

```sh
isochrone receive hw:Loopback,1,0 127.0.0.1:50040 20
isochrone send hw:Loopback,0,0 127.0.0.1:50040
```

## Design notes

**Ratio is pitch.** A correction applied abruptly is a pitch step, and a
listener who would never notice 3ms of added latency will hear 50ppm of sudden
ratio change as flutter. The servo is deliberately sluggish and clamped well
below audibility.

**Slow is not slack.** With a 100ppm bound, correcting 10ms of buffer error takes
333 seconds however the gains are arranged. Clock drift is a slow problem and
admits only slow answers; the loop is tuned near critical damping so it arrives
without ringing, not so it arrives quickly.

**Saturation is reported.** A system quietly pinned at its correction limit for
an hour looks exactly like a healthy one. If the drift exceeds what resampling
should hide, something else is wrong — a wrong nominal rate, a stalled sender —
and pitch-shifting harder is the wrong response.

## License

MIT OR Apache-2.0, at your option.
