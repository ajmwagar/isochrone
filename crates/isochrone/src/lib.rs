//! The I/O boundary around isochrone's deterministic transport core.
//!
//! This crate owns sockets and stream state. Audio-device APIs remain thin
//! adapters: packets, buffering, loss accounting and clock policy must behave
//! identically whether the endpoint is ALSA, CoreAudio, or a test fixture.

use std::{
    io,
    net::{SocketAddr, UdpSocket},
    time::Duration,
};

use isochrone_asrc::{Correction, Servo, ServoConfig};
use isochrone_core::{
    Accepted, Header, Played, Playout, Sequence, StreamFormat, Timestamp,
    packet::{HEADER_BYTES, decode_samples, encode_samples, frames_in_payload},
};

pub mod audio;
pub mod control;

pub const AES67_DYNAMIC_PAYLOAD_TYPE: u8 = 97;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiverMetrics {
    pub packets_received: u64,
    pub malformed_packets: u64,
    pub wrong_payload_type: u64,
    pub wrong_ssrc: u64,
    pub stream_changes: u64,
    pub sequence_gaps: u64,
    pub reordered_packets: u64,
    pub duplicate_packets: u64,
    pub late_packets: u64,
    pub discontinuities: u64,
    pub concealed_frames: u64,
}

/// Turns exact audio blocks into one RTP packet each.
pub struct Packetizer {
    format: StreamFormat,
    payload_type: u8,
    sequence: Sequence,
    timestamp: Timestamp,
    ssrc: u32,
    first: bool,
    payload: Vec<u8>,
}

impl Packetizer {
    #[must_use]
    pub fn new(format: StreamFormat, ssrc: u32) -> Self {
        Self {
            format,
            payload_type: AES67_DYNAMIC_PAYLOAD_TYPE,
            sequence: Sequence(0),
            timestamp: Timestamp(0),
            ssrc,
            first: true,
            payload: Vec::with_capacity(format.payload_bytes()),
        }
    }

    pub fn packet(&mut self, samples: &[f32]) -> io::Result<Vec<u8>> {
        let wanted = self.format.frames_per_packet as usize * self.format.channels as usize;
        if samples.len() != wanted {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "packet needs {wanted} interleaved samples, got {}",
                    samples.len()
                ),
            ));
        }
        encode_samples(samples, self.format.encoding, &mut self.payload);
        let header = Header {
            payload_type: self.payload_type,
            sequence: self.sequence,
            timestamp: self.timestamp,
            ssrc: self.ssrc,
            marker: self.first,
        };
        let mut wire = Vec::with_capacity(HEADER_BYTES + self.payload.len());
        wire.extend_from_slice(&header.encode());
        wire.extend_from_slice(&self.payload);
        self.sequence = self.sequence.next();
        self.timestamp = self.timestamp.advance(self.format.frames_per_packet);
        self.first = false;
        Ok(wire)
    }

    /// Advance the media clock for packets deliberately dropped before send.
    /// Sequence and timestamp must move together or every later packet is late.
    pub fn skip(&mut self, packets: u32) {
        self.sequence = Sequence(self.sequence.0.wrapping_add(packets as u16));
        self.timestamp = self
            .timestamp
            .advance(self.format.frames_per_packet.wrapping_mul(packets));
        if packets != 0 {
            self.first = false;
        }
    }
}

pub struct UdpSender {
    socket: UdpSocket,
    destination: SocketAddr,
    packetizer: Packetizer,
}

impl UdpSender {
    pub fn connect(
        bind: SocketAddr,
        destination: SocketAddr,
        format: StreamFormat,
        ssrc: u32,
    ) -> io::Result<Self> {
        Ok(Self {
            socket: UdpSocket::bind(bind)?,
            destination,
            packetizer: Packetizer::new(format, ssrc),
        })
    }

    pub fn send(&mut self, samples: &[f32]) -> io::Result<usize> {
        let packet = self.packetizer.packet(samples)?;
        let sent = self.socket.send_to(&packet, self.destination)?;
        if sent != packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "partial UDP datagram",
            ));
        }
        Ok(sent)
    }

    pub fn skip(&mut self, packets: u32) {
        self.packetizer.skip(packets);
    }
}

/// Stateful RTP receiver and playout policy, independent of the socket and
/// audio callback that drive it.
pub struct Receiver {
    format: StreamFormat,
    target_frames: usize,
    playout: Playout,
    servo: Servo,
    started: bool,
    ssrc: Option<u32>,
    newest_sequence: Option<Sequence>,
    candidate_ssrc: Option<u32>,
    candidate_sequence: Option<Sequence>,
    candidate_packets: u8,
    active_idle: Duration,
    decoded: Vec<f32>,
    output: Vec<f32>,
    metrics: ReceiverMetrics,
    correction: Correction,
}

impl Receiver {
    #[must_use]
    pub fn new(format: StreamFormat, target_latency: Duration) -> Self {
        let target_frames = duration_frames(target_latency, format.sample_rate_hz).max(1);
        let capacity_frames = target_frames.saturating_mul(4);
        let servo = Servo::new(ServoConfig {
            target_fill: target_latency,
            ..ServoConfig::default()
        });
        Self {
            format,
            target_frames,
            playout: Playout::new(format, capacity_frames),
            servo,
            started: false,
            ssrc: None,
            newest_sequence: None,
            candidate_ssrc: None,
            candidate_sequence: None,
            candidate_packets: 0,
            active_idle: Duration::ZERO,
            decoded: Vec::new(),
            output: Vec::new(),
            metrics: ReceiverMetrics::default(),
            correction: Servo::new(ServoConfig::default())
                .observe(Duration::from_millis(20), Duration::ZERO),
        }
    }

    pub fn ingest(&mut self, wire: &[u8]) {
        let Ok((header, payload)) = Header::decode(wire) else {
            self.metrics.malformed_packets += 1;
            return;
        };
        if header.payload_type != AES67_DYNAMIC_PAYLOAD_TYPE {
            self.metrics.wrong_payload_type += 1;
            return;
        }
        if frames_in_payload(
            payload.len(),
            self.format.channels as usize,
            self.format.encoding,
        ) != Ok(self.format.frames_per_packet as usize)
            || decode_samples(payload, self.format.encoding, &mut self.decoded).is_err()
        {
            self.metrics.malformed_packets += 1;
            return;
        }

        match self.ssrc {
            None => self.ssrc = Some(header.ssrc),
            Some(ssrc) if ssrc != header.ssrc => {
                self.metrics.wrong_ssrc += 1;
                let consecutive_candidate = self.candidate_ssrc == Some(header.ssrc)
                    && self
                        .candidate_sequence
                        .is_some_and(|sequence| sequence.next() == header.sequence);
                self.candidate_ssrc = Some(header.ssrc);
                self.candidate_sequence = Some(header.sequence);
                self.candidate_packets = if consecutive_candidate {
                    self.candidate_packets.saturating_add(1)
                } else {
                    1
                };
                let takeover_delay = self.format.packet_duration().saturating_mul(3);
                if self.active_idle < takeover_delay
                    || (!header.marker && self.candidate_packets < 3)
                {
                    return;
                }
                self.ssrc = Some(header.ssrc);
                self.newest_sequence = None;
                self.playout.reset_to(header.timestamp);
                self.servo.reset();
                self.started = false;
                self.candidate_ssrc = None;
                self.candidate_sequence = None;
                self.candidate_packets = 0;
                self.active_idle = Duration::ZERO;
                self.metrics.stream_changes += 1;
            }
            Some(_) => {
                self.candidate_ssrc = None;
                self.candidate_sequence = None;
                self.candidate_packets = 0;
                self.active_idle = Duration::ZERO;
            }
        }

        if let Some(newest) = self.newest_sequence {
            if newest.precedes(header.sequence) {
                self.metrics.sequence_gaps +=
                    u64::from(newest.distance_to(header.sequence).saturating_sub(1));
                self.newest_sequence = Some(header.sequence);
            } else if newest == header.sequence {
                self.metrics.duplicate_packets += 1;
            } else {
                self.metrics.reordered_packets += 1;
            }
        } else {
            self.newest_sequence = Some(header.sequence);
        }

        match self.playout.insert(header.timestamp, &self.decoded) {
            Accepted::Written { .. } => self.metrics.packets_received += 1,
            Accepted::Duplicate => self.metrics.duplicate_packets += 1,
            Accepted::TooLate { .. } => self.metrics.late_packets += 1,
            Accepted::Discontinuous => {
                self.metrics.discontinuities += 1;
                self.playout.reset_to(header.timestamp);
                let _ = self.playout.insert(header.timestamp, &self.decoded);
                self.servo.reset();
                self.started = false;
            }
        }
    }

    #[must_use]
    pub fn ready(&self) -> bool {
        self.playout.fill_frames() >= self.target_frames
    }

    /// Render exactly `frames`; before the startup target is reached this is
    /// silence without advancing the media playhead.
    pub fn render(&mut self, frames: usize, elapsed: Duration) -> &[f32] {
        self.active_idle = self.active_idle.saturating_add(elapsed);
        if !self.started {
            if self.ready() {
                self.started = true;
            } else {
                self.output.clear();
                self.output
                    .resize(frames * self.format.channels as usize, 0.0);
                return &self.output;
            }
        }
        self.correction = self.servo.observe(self.playout.fill(), elapsed);
        if let Played::Concealed { frames } = self.playout.play(frames, &mut self.output) {
            self.metrics.concealed_frames += frames as u64;
        }
        &self.output
    }

    #[must_use]
    pub fn metrics(&self) -> ReceiverMetrics {
        self.metrics
    }

    #[must_use]
    pub fn fill(&self) -> Duration {
        self.playout.fill()
    }

    /// Desired resampling correction. A device adapter must not claim drift
    /// correction unless it actually applies this ratio to a resampler.
    #[must_use]
    pub fn correction(&self) -> Correction {
        self.correction
    }
}

#[must_use]
pub fn duration_frames(duration: Duration, rate: u32) -> usize {
    (duration.as_nanos() * u128::from(rate) / 1_000_000_000) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(packet: usize) -> Vec<f32> {
        (0..48)
            .flat_map(|frame| {
                let value = (packet * 48 + frame) as f32 / 1_000.0;
                [value, -value]
            })
            .collect()
    }

    #[test]
    fn startup_waits_for_the_target_without_consuming_the_first_packet() {
        let format = StreamFormat::aes67_48k_stereo();
        let mut sender = Packetizer::new(format, 7);
        let mut receiver = Receiver::new(format, Duration::from_millis(3));
        for packet in 0..2 {
            receiver.ingest(&sender.packet(&samples(packet)).unwrap());
        }
        assert!(!receiver.ready());
        assert!(
            receiver
                .render(48, Duration::from_millis(1))
                .iter()
                .all(|v| *v == 0.0)
        );
        receiver.ingest(&sender.packet(&samples(2)).unwrap());
        assert!(receiver.ready());
        let out = receiver.render(48, Duration::from_millis(1));
        assert!(
            (out[2] - 0.001).abs() < 1e-6,
            "L24 quantisation: {}",
            out[2]
        );
    }

    #[test]
    fn loss_and_reordering_are_visible_and_a_gap_is_concealed() {
        let format = StreamFormat::aes67_48k_stereo();
        let mut sender = Packetizer::new(format, 7);
        let packets: Vec<_> = (0..4)
            .map(|number| sender.packet(&samples(number)).unwrap())
            .collect();
        let mut receiver = Receiver::new(format, Duration::from_millis(1));
        receiver.ingest(&packets[0]);
        receiver.ingest(&packets[2]);
        receiver.ingest(&packets[1]);
        receiver.render(48 * 3, Duration::from_millis(3));
        let metrics = receiver.metrics();
        assert_eq!(metrics.sequence_gaps, 1);
        assert_eq!(metrics.reordered_packets, 1);
        assert_eq!(
            metrics.concealed_frames, 0,
            "the reordered packet arrived in time"
        );

        receiver.ingest(&packets[3]);
        receiver.render(48 * 2, Duration::from_millis(2));
        assert_eq!(receiver.metrics().concealed_frames, 48);
    }

    #[test]
    fn udp_sender_emits_a_real_rtp_datagram() {
        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let destination = listener.local_addr().unwrap();
        let mut sender = UdpSender::connect(
            "127.0.0.1:0".parse().unwrap(),
            destination,
            StreamFormat::aes67_48k_stereo(),
            42,
        )
        .unwrap();
        sender.send(&samples(0)).unwrap();
        let mut wire = [0_u8; 2_048];
        let (length, _) = listener.recv_from(&mut wire).unwrap();
        let (header, payload) = Header::decode(&wire[..length]).unwrap();
        assert_eq!(header.ssrc, 42);
        assert!(header.marker);
        assert_eq!(payload.len(), 288);
    }

    #[test]
    fn skipped_packets_advance_sequence_and_media_time_together() {
        let format = StreamFormat::aes67_48k_stereo();
        let mut sender = Packetizer::new(format, 42);
        let _ = sender.packet(&samples(0)).unwrap();
        sender.skip(10);
        let wire = sender.packet(&samples(1)).unwrap();
        let (header, _) = Header::decode(&wire).unwrap();
        assert_eq!(header.sequence, Sequence(11));
        assert_eq!(header.timestamp, Timestamp(11 * 48));
    }

    #[test]
    fn sender_restart_switches_streams_without_restarting_the_receiver() {
        let format = StreamFormat::aes67_48k_stereo();
        let mut first = Packetizer::new(format, 7);
        let mut receiver = Receiver::new(format, Duration::from_millis(2));
        receiver.ingest(&first.packet(&samples(0)).unwrap());
        receiver.ingest(&first.packet(&samples(1)).unwrap());
        assert!(receiver.ready());
        receiver.render(96, Duration::from_millis(3));

        let mut restarted = Packetizer::new(format, 8);
        receiver.ingest(&restarted.packet(&samples(10)).unwrap());
        assert_eq!(receiver.metrics().stream_changes, 1);
        assert!(!receiver.ready(), "old stream audio must be discarded");
        receiver.ingest(&restarted.packet(&samples(11)).unwrap());
        assert!(receiver.ready());
    }

    #[test]
    fn a_restart_recovers_when_the_marker_packet_was_lost() {
        let format = StreamFormat::aes67_48k_stereo();
        let mut first = Packetizer::new(format, 7);
        let mut receiver = Receiver::new(format, Duration::from_millis(1));
        receiver.ingest(&first.packet(&samples(0)).unwrap());
        receiver.render(48, Duration::from_millis(3));

        let mut restarted = Packetizer::new(format, 8);
        let _lost_marker = restarted.packet(&samples(10)).unwrap();
        receiver.ingest(&restarted.packet(&samples(11)).unwrap());
        receiver.ingest(&restarted.packet(&samples(12)).unwrap());
        assert_eq!(receiver.metrics().stream_changes, 0);
        receiver.ingest(&restarted.packet(&samples(13)).unwrap());
        assert_eq!(receiver.metrics().stream_changes, 1);
        assert!(receiver.ready());
    }

    #[test]
    fn a_concurrent_foreign_sender_cannot_steal_an_active_stream() {
        let format = StreamFormat::aes67_48k_stereo();
        let mut active = Packetizer::new(format, 7);
        let mut foreign = Packetizer::new(format, 8);
        let mut receiver = Receiver::new(format, Duration::from_millis(1));
        receiver.ingest(&active.packet(&samples(0)).unwrap());
        for packet in 1..20 {
            receiver.render(48, Duration::from_millis(1));
            receiver.ingest(&foreign.packet(&samples(packet)).unwrap());
            receiver.ingest(&active.packet(&samples(packet)).unwrap());
        }
        assert_eq!(receiver.metrics().stream_changes, 0);
    }
}
