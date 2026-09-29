use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use isochrone::control::{
    AgentConfig, AuthenticatedRequest, Direction, Endpoint, MAX_CONTROL_LINE, PROTOCOL_VERSION,
    ReceiverHealth, Request, Response, RoutePhase, RouteRole, RouteStatus,
};

fn invalid_input(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

struct ManagedRoute {
    status: RouteStatus,
    child: Child,
    health_file: Option<std::path::PathBuf>,
    last_packets: u64,
    last_packet_at: Instant,
}

impl ManagedRoute {
    fn refresh(&mut self) {
        if self.status.phase == RoutePhase::Failed {
            return;
        }
        match self.child.try_wait() {
            Ok(Some(exit)) => {
                self.status.phase = RoutePhase::Failed;
                self.status.error = Some(format!("data-plane process exited with {exit}"));
                self.status.process_id = None;
            }
            Ok(None) => {
                self.status.phase = match &self.health_file {
                    None => RoutePhase::Running,
                    Some(path) => {
                        if let Some(health) = std::fs::read(path)
                            .ok()
                            .and_then(|bytes| serde_json::from_slice::<ReceiverHealth>(&bytes).ok())
                        {
                            if health.schema_version == 1
                                && health.packets_received > self.last_packets
                            {
                                self.last_packets = health.packets_received;
                                self.last_packet_at = Instant::now();
                            }
                        }
                        if self.last_packets > 0
                            && self.last_packet_at.elapsed() <= Duration::from_secs(3)
                        {
                            RoutePhase::Running
                        } else {
                            RoutePhase::Starting
                        }
                    }
                };
            }
            Err(error) => {
                self.status.phase = RoutePhase::Failed;
                self.status.error = Some(format!("inspect data-plane process: {error}"));
                self.status.process_id = None;
            }
        }
    }

    fn stop(mut self) -> io::Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child.kill()?;
            let _ = self.child.wait();
        }
        if let Some(path) = self.health_file {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }
}

struct Agent {
    config: AgentConfig,
    token: String,
    routes: BTreeMap<String, ManagedRoute>,
}

impl Agent {
    fn endpoint(&self, id: &str, direction: Direction) -> io::Result<&Endpoint> {
        let endpoint = self
            .config
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("unknown endpoint {id}"))
            })?;
        if endpoint.direction != direction {
            return Err(invalid_input(format!(
                "endpoint {id} is {:?}, expected {direction:?}",
                endpoint.direction
            )));
        }
        Ok(endpoint)
    }

    fn handle(&mut self, authenticated: AuthenticatedRequest) -> Response {
        if authenticated.protocol_version != PROTOCOL_VERSION {
            return Response::Error {
                message: format!(
                    "unsupported protocol version {}",
                    authenticated.protocol_version
                ),
            };
        }
        if !constant_time_eq(authenticated.token.as_bytes(), self.token.as_bytes()) {
            return Response::Error {
                message: "authentication failed".into(),
            };
        }
        if let Err(message) = authenticated.request.validate() {
            return Response::Error { message };
        }
        match self.dispatch(authenticated.request) {
            Ok(response) => response,
            Err(error) => Response::Error {
                message: error.to_string(),
            },
        }
    }

    fn dispatch(&mut self, request: Request) -> io::Result<Response> {
        match request {
            Request::Inventory => Ok(Response::Inventory {
                node_id: self.config.node_id.clone(),
                endpoints: self.config.endpoints.clone(),
            }),
            Request::Routes => {
                for route in self.routes.values_mut() {
                    route.refresh();
                }
                Ok(Response::Routes {
                    node_id: self.config.node_id.clone(),
                    routes: self
                        .routes
                        .values()
                        .map(|route| route.status.clone())
                        .collect(),
                })
            }
            Request::PrepareReceiver {
                route_id,
                endpoint_id,
                port,
                latency_ms,
                period_frames,
            } => self.prepare_receiver(route_id, endpoint_id, port, latency_ms, period_frames),
            Request::StartSender {
                route_id,
                endpoint_id,
                destination,
                channels,
                gain_db,
            } => self.start_sender(route_id, endpoint_id, destination, channels, gain_db),
            Request::Stop { route_id } => {
                let route = self.routes.remove(&route_id);
                let existed = route.is_some();
                if let Some(route) = route {
                    route.stop()?;
                }
                Ok(Response::Stopped { route_id, existed })
            }
        }
    }

    fn reserve(&self, route_id: &str) -> io::Result<()> {
        if self.routes.contains_key(route_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("route {route_id} already exists"),
            ));
        }
        if self.routes.len() >= self.config.max_routes {
            return Err(io::Error::other("agent route capacity reached"));
        }
        Ok(())
    }

    fn prepare_receiver(
        &mut self,
        route_id: String,
        endpoint_id: String,
        requested_port: u16,
        latency_ms: u64,
        period_frames: usize,
    ) -> io::Result<Response> {
        self.reserve(&route_id)?;
        let endpoint = self.endpoint(&endpoint_id, Direction::Output)?;
        let device = endpoint.device.clone();
        let port = if requested_port == 0 {
            available_udp_port()?
        } else {
            requested_port
        };
        let bind = SocketAddr::new(self.config.listen.ip(), port);
        let destination = SocketAddr::new(self.config.advertise_ip, port);
        let health_file = self
            .config
            .state_dir
            .join(format!("{route_id}.receiver.json"));
        let _ = std::fs::remove_file(&health_file);
        let mut child = Command::new(&self.config.isochrone_binary)
            .args([
                "receive",
                &device,
                &bind.to_string(),
                &latency_ms.to_string(),
                &period_frames.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .env("ISOCHRONE_STATUS_FILE", &health_file)
            .spawn()?;
        ensure_started(&mut child, "receiver")?;
        let status = RouteStatus {
            route_id: route_id.clone(),
            endpoint_id,
            role: RouteRole::Receiver,
            phase: RoutePhase::Starting,
            peer: Some(destination),
            process_id: Some(child.id()),
            error: None,
        };
        self.routes.insert(
            route_id,
            ManagedRoute {
                status: status.clone(),
                child,
                health_file: Some(health_file),
                last_packets: 0,
                last_packet_at: Instant::now(),
            },
        );
        Ok(Response::ReceiverReady {
            route: status,
            destination,
        })
    }

    fn start_sender(
        &mut self,
        route_id: String,
        endpoint_id: String,
        destination: SocketAddr,
        channels: [u16; 2],
        gain_db: f32,
    ) -> io::Result<Response> {
        self.reserve(&route_id)?;
        let endpoint = self.endpoint(&endpoint_id, Direction::Input)?;
        if !endpoint.channels.is_empty()
            && channels
                .iter()
                .any(|channel| !endpoint.channels.contains(channel))
        {
            return Err(invalid_input(format!(
                "endpoint {endpoint_id} does not expose channels {},{}",
                channels[0], channels[1]
            )));
        }
        let device = endpoint.device.clone();
        let channel_pair = format!("{},{}", channels[0], channels[1]);
        let mut child = Command::new(&self.config.isochrone_binary)
            .args([
                "send",
                &device,
                &destination.to_string(),
                "0.0.0.0:0",
                &channel_pair,
                &gain_db.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        ensure_started(&mut child, "sender")?;
        let status = RouteStatus {
            route_id: route_id.clone(),
            endpoint_id,
            role: RouteRole::Sender,
            phase: RoutePhase::Running,
            peer: Some(destination),
            process_id: Some(child.id()),
            error: None,
        };
        self.routes.insert(
            route_id,
            ManagedRoute {
                status: status.clone(),
                child,
                health_file: None,
                last_packets: 0,
                last_packet_at: Instant::now(),
            },
        );
        Ok(Response::SenderStarted { route: status })
    }
}

fn available_udp_port() -> io::Result<u16> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    Ok(socket.local_addr()?.port())
}

fn ensure_started(child: &mut Child, role: &str) -> io::Result<()> {
    std::thread::sleep(Duration::from_millis(250));
    match child.try_wait()? {
        None => Ok(()),
        Some(exit) => Err(io::Error::other(format!(
            "{role} failed during startup with {exit}"
        ))),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn serve(mut stream: TcpStream, agent: &mut Agent) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?)
        .take(MAX_CONTROL_LINE)
        .read_line(&mut line)?;
    let response = match serde_json::from_str::<AuthenticatedRequest>(&line) {
        Ok(request) => agent.handle(request),
        Err(error) => Response::Error {
            message: format!("invalid request: {error}"),
        },
    };
    serde_json::to_writer(&mut stream, &response).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn run(path: &Path) -> io::Result<()> {
    let config = AgentConfig::read(path)?;
    let token = config.token()?;
    std::fs::create_dir_all(&config.state_dir)?;
    let listener = TcpListener::bind(config.listen)?;
    eprintln!(
        "isochrone-agent: node={} listening={} endpoints={}",
        config.node_id,
        listener.local_addr()?,
        config.endpoints.len()
    );
    let mut agent = Agent {
        config,
        token,
        routes: BTreeMap::new(),
    };
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                if let Err(error) = serve(stream, &mut agent) {
                    eprintln!("isochrone-agent: request failed: {error}");
                }
            }
            Err(error) => eprintln!("isochrone-agent: accept failed: {error}"),
        }
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage: isochrone-agent <agent.json>");
        return std::process::ExitCode::FAILURE;
    };
    match run(Path::new(&path)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("isochrone-agent: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_covers_length_and_content() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"different"));
        assert!(!constant_time_eq(b"same", b"sane"));
    }
}
