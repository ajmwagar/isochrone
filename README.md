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

Planned: `isochrone-core` (RTP framing, L24/L16, clock-domain types) and
`isochrone` (sender/receiver, jitter buffer, CoreAudio and ALSA).

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
