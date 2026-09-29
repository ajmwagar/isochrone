//! Control-plane contract for configuring Isochrone RTP routes.
//!
//! Agents supervise the existing `isochrone send` and `isochrone receive`
//! data-plane processes. Audio remains direct RTP/UDP between hosts.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_CONTROL_LINE: u64 = 64 * 1024;

fn invalid_input(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub id: String,
    pub label: String,
    pub direction: Direction,
    /// Exact CoreAudio or ALSA device name passed to the data-plane binary.
    pub device: String,
    #[serde(default)]
    pub channels: Vec<u16>,
}

impl Endpoint {
    pub fn validate(&self) -> Result<(), String> {
        validate_id("endpoint id", &self.id)?;
        validate_text("endpoint label", &self.label, 160)?;
        validate_text("audio device", &self.device, 512)?;
        if self.channels.len() > 64
            || self.channels.contains(&0)
            || self.channels.iter().copied().collect::<BTreeSet<_>>().len() != self.channels.len()
        {
            return Err(format!("endpoint {} has invalid channels", self.id));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub node_id: String,
    pub listen: SocketAddr,
    pub advertise_ip: IpAddr,
    pub token_file: PathBuf,
    pub isochrone_binary: PathBuf,
    pub state_dir: PathBuf,
    #[serde(default = "default_max_routes")]
    pub max_routes: usize,
    pub endpoints: Vec<Endpoint>,
}

fn default_max_routes() -> usize {
    16
}

impl AgentConfig {
    pub fn read(path: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let mut config: Self = serde_json::from_slice(&bytes).map_err(invalid_data)?;
        let absolute = std::fs::canonicalize(path)?;
        let base = absolute.parent().unwrap_or_else(|| Path::new("/"));
        resolve(&mut config.token_file, base);
        resolve(&mut config.isochrone_binary, base);
        resolve(&mut config.state_dir, base);
        config.validate().map_err(invalid_input)?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_id("node id", &self.node_id)?;
        if self.listen.ip().is_unspecified() && self.advertise_ip.is_unspecified() {
            return Err("advertise_ip must be routable when listen is unspecified".into());
        }
        if self.max_routes == 0 || self.max_routes > 256 {
            return Err("max_routes must be 1..=256".into());
        }
        if !self.state_dir.is_absolute() {
            return Err("state_dir must be absolute".into());
        }
        if self.endpoints.is_empty() || self.endpoints.len() > 128 {
            return Err("endpoints must contain 1..=128 entries".into());
        }
        let mut ids = BTreeSet::new();
        for endpoint in &self.endpoints {
            endpoint.validate()?;
            if !ids.insert(&endpoint.id) {
                return Err(format!("duplicate endpoint {}", endpoint.id));
            }
        }
        Ok(())
    }

    pub fn token(&self) -> io::Result<String> {
        let token = std::fs::read_to_string(&self.token_file)?;
        let token = token.trim().to_owned();
        if token.len() < 32 || token.len() > 512 || token.chars().any(char::is_whitespace) {
            return Err(invalid_data(
                "agent token must be 32..=512 non-whitespace characters",
            ));
        }
        Ok(token)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerConfig {
    pub agents: Vec<AgentTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTarget {
    pub node_id: String,
    pub address: SocketAddr,
    pub token_file: PathBuf,
}

impl ControllerConfig {
    pub fn read(path: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let mut config: Self = serde_json::from_slice(&bytes).map_err(invalid_data)?;
        let absolute = std::fs::canonicalize(path)?;
        let base = absolute.parent().unwrap_or_else(|| Path::new("/"));
        for agent in &mut config.agents {
            resolve(&mut agent.token_file, base);
        }
        if config.agents.is_empty() {
            return Err(invalid_input("controller has no agents"));
        }
        let mut nodes = BTreeSet::new();
        for agent in &config.agents {
            validate_id("node id", &agent.node_id).map_err(invalid_input)?;
            if !nodes.insert(&agent.node_id) {
                return Err(invalid_input(format!("duplicate agent {}", agent.node_id)));
            }
        }
        Ok(config)
    }

    pub fn by_id(&self, id: &str) -> io::Result<&AgentTarget> {
        self.agents
            .iter()
            .find(|agent| agent.node_id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown node {id}")))
    }
}

fn resolve(path: &mut PathBuf, base: &Path) {
    if path.is_relative() {
        *path = base.join(&*path);
    }
}

impl AgentTarget {
    pub fn token(&self) -> io::Result<String> {
        let token = std::fs::read_to_string(&self.token_file)?;
        let token = token.trim().to_owned();
        if token.len() < 32 || token.len() > 512 || token.chars().any(char::is_whitespace) {
            return Err(invalid_data(
                "agent token must be 32..=512 non-whitespace characters",
            ));
        }
        Ok(token)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthenticatedRequest {
    pub protocol_version: u32,
    pub token: String,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "command")]
pub enum Request {
    Inventory,
    Routes,
    PrepareReceiver {
        route_id: String,
        endpoint_id: String,
        #[serde(default)]
        port: u16,
        latency_ms: u64,
        period_frames: usize,
    },
    StartSender {
        route_id: String,
        endpoint_id: String,
        destination: SocketAddr,
        channels: [u16; 2],
        gain_db: f32,
    },
    Stop {
        route_id: String,
    },
}

impl Request {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Inventory | Self::Routes => Ok(()),
            Self::PrepareReceiver {
                route_id,
                latency_ms,
                period_frames,
                ..
            } => {
                validate_id("route id", route_id)?;
                if !(1..=2_000).contains(latency_ms) {
                    return Err("latency_ms must be 1..=2000".into());
                }
                if *period_frames == 0 || *period_frames > 48_000 {
                    return Err("period_frames must be 1..=48000".into());
                }
                Ok(())
            }
            Self::StartSender {
                route_id,
                channels,
                gain_db,
                ..
            } => {
                validate_id("route id", route_id)?;
                if channels.contains(&0) || channels[0] == channels[1] {
                    return Err("source channels must be distinct and one-based".into());
                }
                if !gain_db.is_finite() || !(-120.0..=24.0).contains(gain_db) {
                    return Err("gain_db must be finite and -120..=24".into());
                }
                Ok(())
            }
            Self::Stop { route_id } => validate_id("route id", route_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum Response {
    Inventory {
        node_id: String,
        endpoints: Vec<Endpoint>,
    },
    Routes {
        node_id: String,
        routes: Vec<RouteStatus>,
    },
    ReceiverReady {
        route: RouteStatus,
        destination: SocketAddr,
    },
    SenderStarted {
        route: RouteStatus,
    },
    Stopped {
        route_id: String,
        existed: bool,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteRole {
    Sender,
    Receiver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutePhase {
    Starting,
    Running,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteStatus {
    pub route_id: String,
    pub endpoint_id: String,
    pub role: RouteRole,
    pub phase: RoutePhase,
    pub peer: Option<SocketAddr>,
    pub process_id: Option<u32>,
    pub error: Option<String>,
}

/// Health written atomically by a receiving data-plane process. The agent
/// uses this to distinguish "process started" from "RTP is arriving".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiverHealth {
    pub schema_version: u32,
    pub observed_at_unix_ms: u64,
    pub packets_received: u64,
    pub concealed_frames: u64,
    pub playback_recoveries: u64,
}

pub fn call(target: &AgentTarget, request: Request) -> io::Result<Response> {
    request.validate().map_err(invalid_input)?;
    let mut stream = TcpStream::connect_timeout(&target.address, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let request = AuthenticatedRequest {
        protocol_version: PROTOCOL_VERSION,
        token: target.token()?,
        request,
    };
    serde_json::to_writer(&mut stream, &request).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(MAX_CONTROL_LINE)
        .read_line(&mut line)?;
    if line.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "agent closed without a response",
        ));
    }
    serde_json::from_str(&line).map_err(invalid_data)
}

pub fn split_endpoint(value: &str) -> io::Result<(&str, &str)> {
    let (node, endpoint) = value
        .split_once('/')
        .ok_or_else(|| invalid_input(format!("endpoint {value:?} must be NODE/ENDPOINT")))?;
    validate_id("node id", node).map_err(invalid_input)?;
    validate_id("endpoint id", endpoint).map_err(invalid_input)?;
    Ok((node, endpoint))
}

pub fn index_inventory(
    responses: impl IntoIterator<Item = Response>,
) -> io::Result<BTreeMap<String, Vec<Endpoint>>> {
    responses
        .into_iter()
        .map(|response| match response {
            Response::Inventory { node_id, endpoints } => Ok((node_id, endpoints)),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(invalid_data(format!(
                "agent returned unexpected response {other:?}"
            ))),
        })
        .collect()
}

fn validate_id(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!("invalid {label} {value:?}"));
    }
    Ok(())
}

fn validate_text(label: &str, value: &str, max: usize) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(format!("invalid {label}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(id: &str, direction: Direction) -> Endpoint {
        Endpoint {
            id: id.into(),
            label: id.into(),
            direction,
            device: format!("hw:{id}"),
            channels: vec![1, 2],
        }
    }

    #[test]
    fn request_wire_is_explicit_and_versioned() {
        let request = AuthenticatedRequest {
            protocol_version: PROTOCOL_VERSION,
            token: "x".repeat(32),
            request: Request::PrepareReceiver {
                route_id: "studio-to-pi".into(),
                endpoint_id: "scarlett-out".into(),
                port: 50_040,
                latency_ms: 20,
                period_frames: 96,
            },
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["command"], "prepare_receiver");
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["port"], 50_040);
    }

    #[test]
    fn config_rejects_duplicate_endpoints() {
        let config = AgentConfig {
            node_id: "pi".into(),
            listen: "127.0.0.1:50100".parse().unwrap(),
            advertise_ip: "127.0.0.1".parse().unwrap(),
            token_file: "token".into(),
            isochrone_binary: "isochrone".into(),
            state_dir: "/tmp/isochrone-test".into(),
            max_routes: 4,
            endpoints: vec![
                endpoint("scarlett", Direction::Input),
                endpoint("scarlett", Direction::Output),
            ],
        };
        assert!(config.validate().unwrap_err().contains("duplicate"));
    }

    #[test]
    fn endpoint_reference_is_unambiguous() {
        assert_eq!(
            split_endpoint("mac-studio/scarlett-in").unwrap(),
            ("mac-studio", "scarlett-in")
        );
        assert!(split_endpoint("scarlett-in").is_err());
        assert!(split_endpoint("node/bad/endpoint").is_err());
    }

    #[test]
    fn unsafe_routes_fail_before_reaching_an_agent() {
        let request = Request::StartSender {
            route_id: "route".into(),
            endpoint_id: "input".into(),
            destination: "127.0.0.1:50040".parse().unwrap(),
            channels: [0, 2],
            gain_db: 0.0,
        };
        assert!(request.validate().is_err());
    }
}
