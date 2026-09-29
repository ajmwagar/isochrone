use std::{
    collections::VecDeque,
    env, io,
    net::{SocketAddr, UdpSocket},
    process::ExitCode,
    time::Duration,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::{Instant, SystemTime, UNIX_EPOCH};
#[cfg(target_os = "linux")]
use std::{sync::mpsc, thread};

#[cfg(target_os = "linux")]
use isochrone::{Receiver, audio::Playback};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use isochrone::{UdpSender, audio::Capture};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use isochrone_core::StreamFormat;

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage:\n  isochrone send <audio-device> <destination-ip:port> [bind-ip:port] [channels] [gain-db]\n  isochrone send-wav <48k-stereo-f32.wav> <destination-ip:port> [gain-db]\n  isochrone gateway <pcm-bind-ip:port> <rtp-destination-ip:port> [rtp-bind-ip:port] [gain-db]\n  isochrone relay <bind-ip:port> <destination-ip:port>\n  isochrone receive <alsa-device> <bind-ip:port> [target-latency-ms] [device-period-frames]\n  isochrone receive-mix <alsa-device> <bind-ip:port/packet-frames,...> <target-latency-ms> <device-period-frames>",
    )
}

fn socket(value: &str) -> io::Result<SocketAddr> {
    value.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid socket address {value:?}: {error}"),
        )
    })
}

fn stream_binds(value: &str) -> io::Result<Vec<(SocketAddr, u32)>> {
    let streams = value
        .split(',')
        .map(|stream| {
            let (address, frames) = stream.rsplit_once('/').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("stream {stream:?} must be address/packet-frames"),
                )
            })?;
            let frames = frames.parse::<u32>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid packet frames {frames:?}: {error}"),
                )
            })?;
            if frames == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "packet frames must be nonzero",
                ));
            }
            Ok((socket(address)?, frames))
        })
        .collect::<io::Result<Vec<_>>>()?;
    if streams.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "at least one socket address is required",
        ));
    }
    Ok(streams)
}

fn relay(bind: SocketAddr, destination: SocketAddr) -> io::Result<()> {
    let incoming = UdpSocket::bind(bind)?;
    let outgoing = UdpSocket::bind("0.0.0.0:0")?;
    let mut packet = [0_u8; 65_535];
    eprintln!("isochrone: relaying UDP {bind} to {destination}");
    loop {
        let size = incoming.recv(&mut packet)?;
        outgoing.send_to(&packet[..size], destination)?;
    }
}

fn decode_f32le_stereo(bytes: &[u8], output: &mut VecDeque<f32>) -> io::Result<usize> {
    if !bytes.len().is_multiple_of(2 * size_of::<f32>()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PCM datagram is not whole stereo f32le frames",
        ));
    }
    for sample in bytes.chunks_exact(size_of::<f32>()) {
        output.push_back(f32::from_le_bytes(
            sample.try_into().expect("chunk is four bytes"),
        ));
    }
    Ok(bytes.len() / (2 * size_of::<f32>()))
}

fn gateway(
    pcm_bind: SocketAddr,
    rtp_destination: SocketAddr,
    rtp_bind: SocketAddr,
    gain_db: f32,
) -> io::Result<()> {
    const MAX_QUEUED_FRAMES: usize = 48_000 * 10;
    let format = StreamFormat {
        frames_per_packet: 240,
        ..StreamFormat::aes67_48k_stereo()
    };
    let pcm = UdpSocket::bind(pcm_bind)?;
    pcm.set_nonblocking(true)?;
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let ssrc = (epoch.as_nanos() as u32) ^ std::process::id();
    let mut sender = UdpSender::connect(rtp_bind, rtp_destination, format, ssrc)?;
    let mut incoming = [0_u8; 65_535];
    let mut queued = VecDeque::new();
    let mut packet = vec![0.0_f32; format.frames_per_packet as usize * 2];
    let gain = 10.0_f32.powf(gain_db / 20.0);
    let mut next = Instant::now();
    let mut packets = 0_u64;
    let mut malformed = 0_u64;
    let mut overruns = 0_u64;
    let mut network_drops = 0_u64;
    let mut last_report = next;
    eprintln!(
        "isochrone: PCM gateway {pcm_bind} to {rtp_destination} at {gain_db:.1}dB, ssrc={ssrc:#010x}"
    );
    loop {
        loop {
            match pcm.recv(&mut incoming) {
                Ok(size) => {
                    if decode_f32le_stereo(&incoming[..size], &mut queued).is_err() {
                        malformed += 1;
                    }
                    let queued_frames = queued.len() / 2;
                    if queued_frames > MAX_QUEUED_FRAMES {
                        queued.truncate(MAX_QUEUED_FRAMES * 2);
                        overruns += 1;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }

        for sample in &mut packet {
            *sample = queued.pop_front().unwrap_or(0.0) * gain;
        }
        if let Err(error) = sender.send(&packet) {
            if error.kind() == io::ErrorKind::WouldBlock || error.raw_os_error() == Some(55) {
                network_drops += 1;
            } else {
                return Err(error);
            }
        }
        packets += 1;
        if last_report.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "isochrone: gateway packets={packets} queued={:.1}ms malformed={malformed} overruns={overruns} network_drops={network_drops}",
                queued.len() as f64 * 500.0 / 48_000.0,
            );
            last_report = Instant::now();
        }
        next += format.packet_duration();
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now.duration_since(next) > Duration::from_millis(10) {
            let packet_ns = format.packet_duration().as_nanos();
            let missed = (now.duration_since(next).as_nanos() / packet_ns) as u32;
            sender.skip(missed);
            packets += u64::from(missed);
            network_drops += u64::from(missed);
            next += format.packet_duration().saturating_mul(missed);
        }
    }
}

fn channel_pair(value: &str) -> io::Result<[usize; 2]> {
    let (left, right) = value.split_once(',').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("channel pair {value:?} must look like 5,6"),
        )
    })?;
    let parse = |channel: &str| -> io::Result<usize> {
        let channel = channel.parse::<usize>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid channel {channel:?}: {error}"),
            )
        })?;
        channel.checked_sub(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "channel numbers are one-based")
        })
    };
    Ok([parse(left)?, parse(right)?])
}

#[cfg(target_os = "linux")]
fn send(
    device: &str,
    destination: SocketAddr,
    bind: SocketAddr,
    _channels: [usize; 2],
    gain_db: f32,
) -> io::Result<()> {
    use isochrone::audio::alsa::AlsaCapture;

    let format = StreamFormat::aes67_48k_stereo();
    let frames = format.frames_per_packet as usize;
    let mut capture = AlsaCapture::open(
        device,
        format.sample_rate_hz,
        format.channels as usize,
        frames,
    )?;
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let ssrc = (epoch.as_nanos() as u32) ^ std::process::id();
    let mut sender = UdpSender::connect(bind, destination, format, ssrc)?;
    let mut samples = vec![0.0; frames * format.channels as usize];
    let gain = 10.0_f32.powf(gain_db / 20.0);
    eprintln!("isochrone: sending {device} to {destination}, ssrc={ssrc:#010x}");
    loop {
        capture.read_period(&mut samples)?;
        samples.iter_mut().for_each(|sample| *sample *= gain);
        sender.send(&samples)?;
    }
}

#[cfg(target_os = "macos")]
fn send_wav(path: &str, destination: SocketAddr, gain_db: f32) -> io::Result<()> {
    let mut reader = hound::WavReader::open(path).map_err(io::Error::other)?;
    let spec = reader.spec();
    if spec.channels != 2
        || spec.sample_rate != 48_000
        || spec.sample_format != hound::SampleFormat::Float
        || spec.bits_per_sample != 32
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "file must be 48kHz stereo 32-bit float WAV, got {}Hz {}ch {:?}/{}bit",
                spec.sample_rate, spec.channels, spec.sample_format, spec.bits_per_sample
            ),
        ));
    }
    let format = StreamFormat::aes67_48k_stereo();
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let ssrc = (epoch.as_nanos() as u32) ^ std::process::id();
    let mut sender = UdpSender::connect(
        "0.0.0.0:0".parse().expect("literal socket address"),
        destination,
        format,
        ssrc,
    )?;
    let gain = 10.0_f32.powf(gain_db / 20.0);
    let samples_per_packet = format.frames_per_packet as usize * format.channels as usize;
    let mut samples = Vec::with_capacity(samples_per_packet);
    let mut source = reader.samples::<f32>();
    let mut packets = 0_u64;
    let mut next_batch = Instant::now();
    eprintln!("isochrone: sending {path:?} to {destination} at {gain_db:.1}dB, ssrc={ssrc:#010x}");
    loop {
        samples.clear();
        for _ in 0..samples_per_packet {
            match source.next() {
                Some(Ok(sample)) => samples.push(sample * gain),
                Some(Err(error)) => return Err(io::Error::other(error)),
                None => break,
            }
        }
        if samples.is_empty() {
            break;
        }
        samples.resize(samples_per_packet, 0.0);
        sender.send(&samples)?;
        packets += 1;
        if packets.is_multiple_of(10) {
            next_batch += Duration::from_millis(10);
            let now = Instant::now();
            if now < next_batch {
                std::thread::sleep(next_batch - now);
            }
        }
    }
    eprintln!("isochrone: sent {packets} packets");
    Ok(())
}

#[cfg(target_os = "macos")]
fn send(
    device: &str,
    destination: SocketAddr,
    bind: SocketAddr,
    channels: [usize; 2],
    gain_db: f32,
) -> io::Result<()> {
    use isochrone::audio::coreaudio::CoreAudioCapture;

    let format = StreamFormat::aes67_48k_stereo();
    let frames = format.frames_per_packet as usize;
    let mut capture = CoreAudioCapture::open(device, format.sample_rate_hz, channels)?;
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let ssrc = (epoch.as_nanos() as u32) ^ std::process::id();
    let mut sender = UdpSender::connect(bind, destination, format, ssrc)?;
    let mut samples = vec![0.0; frames * format.channels as usize];
    let gain = 10.0_f32.powf(gain_db / 20.0);
    let mut packets = 0_u64;
    let mut last_report = Instant::now();
    eprintln!(
        "isochrone: sending CoreAudio {device} inputs {}-{} to {destination} at {gain_db:.1}dB, ssrc={ssrc:#010x}",
        channels[0] + 1,
        channels[1] + 1,
    );
    loop {
        capture.read_period(&mut samples)?;
        samples.iter_mut().for_each(|sample| *sample *= gain);
        sender.send(&samples)?;
        packets += 1;
        if last_report.elapsed() >= Duration::from_secs(1) {
            let channel_peaks = capture.take_channel_peaks()?;
            let active = channel_peaks
                .iter()
                .enumerate()
                .filter(|(_, peak)| **peak > 1e-6)
                .map(|(channel, peak)| format!("{}:{:.1}dBFS", channel + 1, 20.0 * peak.log10()))
                .collect::<Vec<_>>();
            eprintln!(
                "isochrone: packets={packets} active_inputs=[{}] callback_drops={}",
                active.join(", "),
                capture.dropped_blocks(),
            );
            last_report = Instant::now();
        }
    }
}

#[cfg(target_os = "linux")]
fn receive(
    device: &str,
    bind: SocketAddr,
    target: Duration,
    device_period_frames: usize,
) -> io::Result<()> {
    receive_mix(device, &[(bind, 48)], target, device_period_frames)
}

#[cfg(target_os = "linux")]
fn receive_mix(
    device: &str,
    streams: &[(SocketAddr, u32)],
    target: Duration,
    device_period_frames: usize,
) -> io::Result<()> {
    use isochrone::audio::alsa::AlsaPlayback;

    let output_format = StreamFormat::aes67_48k_stereo();
    let mut incoming = Vec::with_capacity(streams.len());
    for (index, (bind, _)) in streams.iter().copied().enumerate() {
        let socket = UdpSocket::bind(bind)?;
        let (packets, packets_in) = mpsc::sync_channel::<Vec<u8>>(512);
        thread::Builder::new()
            .name(format!("isochrone-rtp-receiver-{index}"))
            .spawn(move || {
                loop {
                    let mut wire = vec![0_u8; 2_048];
                    match socket.recv(&mut wire) {
                        Ok(length) => {
                            wire.truncate(length);
                            if packets.send(wire).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            eprintln!("isochrone: UDP receive failed: {error}");
                            break;
                        }
                    }
                }
            })?;
        incoming.push(packets_in);
    }

    let mut receivers = streams
        .iter()
        .map(|(_, frames_per_packet)| {
            Receiver::new(
                StreamFormat {
                    frames_per_packet: *frames_per_packet,
                    ..output_format
                },
                target,
            )
        })
        .collect::<Vec<_>>();
    let mut playback = AlsaPlayback::open(
        device,
        output_format.sample_rate_hz,
        output_format.channels as usize,
        device_period_frames,
    )?;
    let period = Duration::from_secs_f64(
        device_period_frames as f64 / f64::from(output_format.sample_rate_hz),
    );
    let mut last_report = Instant::now();
    let health_path = std::env::var_os("ISOCHRONE_STATUS_FILE").map(std::path::PathBuf::from);
    let mut next_playback = Instant::now();
    let mut mixed = vec![0.0_f32; device_period_frames * output_format.channels as usize];
    eprintln!(
        "isochrone: receiving {streams:?} to {device}, target={}ms, device={} frames",
        target.as_millis(),
        device_period_frames,
    );
    loop {
        let now = Instant::now();
        if now < next_playback {
            thread::sleep(next_playback - now);
        } else if now.duration_since(next_playback) > period {
            // Never catch up by emitting a burst: that drains the jitter
            // buffer and converts one scheduler stall into persistent loss.
            next_playback = now;
        }
        for (receiver, packets) in receivers.iter_mut().zip(&incoming) {
            loop {
                match packets.try_recv() {
                    Ok(packet) => receiver.ingest(&packet),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "network receiver stopped",
                        ));
                    }
                }
            }
        }
        mixed.fill(0.0);
        for receiver in &mut receivers {
            for (mixed_sample, sample) in mixed
                .iter_mut()
                .zip(receiver.render(device_period_frames, period))
            {
                *mixed_sample += *sample;
            }
        }
        mixed
            .iter_mut()
            .for_each(|sample| *sample = sample.clamp(-1.0, 1.0));
        playback.write_period(&mixed)?;
        next_playback += period;

        if last_report.elapsed() >= Duration::from_secs(1) {
            let mut packets_received = 0;
            let mut concealed_frames = 0;
            for (index, receiver) in receivers.iter().enumerate() {
                let metrics = receiver.metrics();
                packets_received += metrics.packets_received;
                concealed_frames += metrics.concealed_frames;
                eprintln!(
                    "isochrone: input={index} fill={:.2}ms correction={:.2}ppm packets={} stream_changes={} gaps={} reordered={} late={} concealed_frames={} malformed={} playback_recoveries={}",
                    receiver.fill().as_secs_f64() * 1_000.0,
                    receiver.correction().ppm.0,
                    metrics.packets_received,
                    metrics.stream_changes,
                    metrics.sequence_gaps,
                    metrics.reordered_packets,
                    metrics.late_packets,
                    metrics.concealed_frames,
                    metrics.malformed_packets,
                    playback.recoveries(),
                );
            }
            if let Some(path) = &health_path {
                write_receiver_health(
                    path,
                    packets_received,
                    concealed_frames,
                    playback.recoveries(),
                )?;
            }
            last_report = Instant::now();
        }
    }
}

#[cfg(target_os = "linux")]
fn write_receiver_health(
    path: &std::path::Path,
    packets_received: u64,
    concealed_frames: u64,
    playback_recoveries: u64,
) -> io::Result<()> {
    let observed_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis() as u64;
    let health = isochrone::control::ReceiverHealth {
        schema_version: 1,
        observed_at_unix_ms,
        packets_received,
        concealed_frames,
        playback_recoveries,
    };
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(
        &temporary,
        serde_json::to_vec(&health).map_err(io::Error::other)?,
    )?;
    std::fs::rename(temporary, path)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn send(_: &str, _: SocketAddr, _: SocketAddr, _: [usize; 2], _: f32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the first device adapter is ALSA/Linux",
    ))
}

#[cfg(not(target_os = "macos"))]
fn send_wav(_: &str, _: SocketAddr, _: f32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "send-wav is currently a macOS test adapter",
    ))
}

#[cfg(not(target_os = "linux"))]
fn receive(_: &str, _: SocketAddr, _: Duration, _: usize) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the first device adapter is ALSA/Linux",
    ))
}

#[cfg(not(target_os = "linux"))]
fn receive_mix(_: &str, _: &[(SocketAddr, u32)], _: Duration, _: usize) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the first device adapter is ALSA/Linux",
    ))
}

fn run() -> io::Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, device, destination] if command == "send" => send(
            device,
            socket(destination)?,
            "0.0.0.0:0".parse().expect("literal socket address"),
            [0, 1],
            0.0,
        ),
        [command, device, destination, bind] if command == "send" => {
            send(device, socket(destination)?, socket(bind)?, [0, 1], 0.0)
        }
        [command, device, destination, bind, channels] if command == "send" => send(
            device,
            socket(destination)?,
            socket(bind)?,
            channel_pair(channels)?,
            0.0,
        ),
        [command, device, destination, bind, channels, gain_db] if command == "send" => {
            let gain_db = gain_db.parse::<f32>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid gain {gain_db:?}: {error}"),
                )
            })?;
            send(
                device,
                socket(destination)?,
                socket(bind)?,
                channel_pair(channels)?,
                gain_db,
            )
        }
        [command, path, destination] if command == "send-wav" => {
            send_wav(path, socket(destination)?, -30.0)
        }
        [command, path, destination, gain_db] if command == "send-wav" => {
            let gain_db = gain_db.parse::<f32>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid gain {gain_db:?}: {error}"),
                )
            })?;
            send_wav(path, socket(destination)?, gain_db)
        }
        [command, pcm_bind, destination] if command == "gateway" => gateway(
            socket(pcm_bind)?,
            socket(destination)?,
            "0.0.0.0:0".parse().expect("literal socket address"),
            0.0,
        ),
        [command, pcm_bind, destination, rtp_bind] if command == "gateway" => gateway(
            socket(pcm_bind)?,
            socket(destination)?,
            socket(rtp_bind)?,
            0.0,
        ),
        [command, pcm_bind, destination, rtp_bind, gain_db] if command == "gateway" => {
            let gain_db = gain_db.parse::<f32>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid gain {gain_db:?}: {error}"),
                )
            })?;
            gateway(
                socket(pcm_bind)?,
                socket(destination)?,
                socket(rtp_bind)?,
                gain_db,
            )
        }
        [command, bind, destination] if command == "relay" => {
            relay(socket(bind)?, socket(destination)?)
        }
        [command, device, bind] if command == "receive" => {
            receive(device, socket(bind)?, Duration::from_millis(20), 48)
        }
        [command, device, bind, latency] if command == "receive" => {
            let latency = latency.parse::<u64>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid target latency {latency:?}: {error}"),
                )
            })?;
            receive(device, socket(bind)?, Duration::from_millis(latency), 48)
        }
        [command, device, bind, latency, period_frames] if command == "receive" => {
            let latency = latency.parse::<u64>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid target latency {latency:?}: {error}"),
                )
            })?;
            let period_frames = period_frames.parse::<usize>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid device period {period_frames:?}: {error}"),
                )
            })?;
            receive(
                device,
                socket(bind)?,
                Duration::from_millis(latency),
                period_frames,
            )
        }
        [command, device, binds, latency, period_frames] if command == "receive-mix" => {
            let latency = latency.parse::<u64>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid target latency {latency:?}: {error}"),
                )
            })?;
            let period_frames = period_frames.parse::<usize>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid device period {period_frames:?}: {error}"),
                )
            })?;
            receive_mix(
                device,
                &stream_binds(binds)?,
                Duration::from_millis(latency),
                period_frames,
            )
        }
        _ => Err(usage()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("isochrone: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::{channel_pair, decode_f32le_stereo};

    #[test]
    fn channel_pairs_are_one_based_at_the_cli_boundary() {
        assert_eq!(channel_pair("5,6").unwrap(), [4, 5]);
        assert!(channel_pair("0,1").is_err());
        assert!(channel_pair("5").is_err());
    }

    #[test]
    fn pcm_gateway_accepts_only_whole_stereo_f32le_frames() {
        let mut decoded = VecDeque::new();
        let bytes = [0.25_f32.to_le_bytes(), (-0.5_f32).to_le_bytes()].concat();
        assert_eq!(decode_f32le_stereo(&bytes, &mut decoded).unwrap(), 1);
        assert_eq!(decoded.into_iter().collect::<Vec<_>>(), [0.25, -0.5]);
        assert!(decode_f32le_stereo(&bytes[..7], &mut VecDeque::new()).is_err());
    }
}
