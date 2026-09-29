#![cfg(unix)]

use std::fs;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use isochrone::control::{
    AgentConfig, AgentTarget, ControllerConfig, Direction, Endpoint, Request, Response, RoutePhase,
    call,
};

struct RunningAgent {
    child: Child,
    target: AgentTarget,
}

impl Drop for RunningAgent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_tcp_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn fake_data_plane(directory: &Path) -> PathBuf {
    let path = directory.join("fake-isochrone");
    fs::write(
        &path,
        "#!/bin/sh\nif test \"$1\" = receive && test -n \"$ISOCHRONE_STATUS_FILE\"; then printf '%s\\n' '{\"schema_version\":1,\"observed_at_unix_ms\":1,\"packets_received\":10,\"concealed_frames\":0,\"playback_recoveries\":0}' > \"$ISOCHRONE_STATUS_FILE\"; fi\nsleep 30\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn failing_sender_data_plane(directory: &Path) -> PathBuf {
    let path = directory.join("fail-sender-isochrone");
    fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn controller_file(directory: &Path, agents: &[&RunningAgent]) -> PathBuf {
    let path = directory.join("control.json");
    let config = ControllerConfig {
        agents: agents.iter().map(|agent| agent.target.clone()).collect(),
    };
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    path
}

fn start_agent(
    directory: &Path,
    node_id: &str,
    direction: Direction,
    binary: &Path,
    token: &str,
) -> RunningAgent {
    let address = free_tcp_address();
    let token_file = directory.join(format!("{node_id}.token"));
    fs::write(&token_file, token).unwrap();
    let config = AgentConfig {
        node_id: node_id.into(),
        listen: address,
        advertise_ip: "127.0.0.1".parse().unwrap(),
        token_file: token_file.clone(),
        isochrone_binary: binary.into(),
        state_dir: directory.join(format!("{node_id}-state")),
        max_routes: 4,
        endpoints: vec![Endpoint {
            id: match direction {
                Direction::Input => "adc",
                Direction::Output => "dac",
            }
            .into(),
            label: format!("{node_id} audio"),
            direction,
            device: "test-device".into(),
            channels: vec![1, 2],
        }],
    };
    let config_path = directory.join(format!("{node_id}.json"));
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_isochrone-agent"))
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(address).is_err() {
        assert!(Instant::now() < deadline, "agent {node_id} did not listen");
        std::thread::sleep(Duration::from_millis(20));
    }
    RunningAgent {
        child,
        target: AgentTarget {
            node_id: node_id.into(),
            address,
            token_file,
        },
    }
}

#[test]
fn two_agents_start_inspect_and_stop_a_route() {
    let directory = tempfile::tempdir().unwrap();
    let data_plane = fake_data_plane(directory.path());
    let token = "0123456789abcdef0123456789abcdef";
    let source = start_agent(
        directory.path(),
        "source",
        Direction::Input,
        &data_plane,
        token,
    );
    let sink = start_agent(
        directory.path(),
        "sink",
        Direction::Output,
        &data_plane,
        token,
    );

    let destination = match call(
        &sink.target,
        Request::PrepareReceiver {
            route_id: "studio-to-pi".into(),
            endpoint_id: "dac".into(),
            port: 0,
            latency_ms: 20,
            period_frames: 96,
        },
    )
    .unwrap()
    {
        Response::ReceiverReady { destination, route } => {
            assert_eq!(route.phase, RoutePhase::Starting);
            destination
        }
        response => panic!("unexpected receiver response {response:?}"),
    };
    assert_ne!(destination.port(), 0);

    match call(
        &source.target,
        Request::StartSender {
            route_id: "studio-to-pi".into(),
            endpoint_id: "adc".into(),
            destination,
            channels: [1, 2],
            gain_db: -6.0,
        },
    )
    .unwrap()
    {
        Response::SenderStarted { route } => assert_eq!(route.phase, RoutePhase::Running),
        response => panic!("unexpected sender response {response:?}"),
    }

    for target in [&source.target, &sink.target] {
        match call(target, Request::Routes).unwrap() {
            Response::Routes { routes, .. } => {
                assert_eq!(routes.len(), 1);
                assert_eq!(routes[0].route_id, "studio-to-pi");
                assert_eq!(routes[0].phase, RoutePhase::Running);
            }
            response => panic!("unexpected routes response {response:?}"),
        }
    }

    for target in [&source.target, &sink.target] {
        match call(
            target,
            Request::Stop {
                route_id: "studio-to-pi".into(),
            },
        )
        .unwrap()
        {
            Response::Stopped { existed, .. } => assert!(existed),
            response => panic!("unexpected stop response {response:?}"),
        }
    }
}

#[test]
fn bad_token_cannot_inventory_or_start_audio() {
    let directory = tempfile::tempdir().unwrap();
    let data_plane = fake_data_plane(directory.path());
    let agent = start_agent(
        directory.path(),
        "sink",
        Direction::Output,
        &data_plane,
        "0123456789abcdef0123456789abcdef",
    );
    let wrong_token = directory.path().join("wrong.token");
    fs::write(&wrong_token, "ffffffffffffffffffffffffffffffff").unwrap();
    let target = AgentTarget {
        token_file: wrong_token,
        ..agent.target.clone()
    };
    assert!(matches!(
        call(&target, Request::Inventory).unwrap(),
        Response::Error { message } if message == "authentication failed"
    ));
}

#[test]
fn controller_rolls_back_receiver_when_sender_fails() {
    let directory = tempfile::tempdir().unwrap();
    let healthy = fake_data_plane(directory.path());
    let failing = failing_sender_data_plane(directory.path());
    let token = "0123456789abcdef0123456789abcdef";
    let source = start_agent(
        directory.path(),
        "source",
        Direction::Input,
        &failing,
        token,
    );
    let sink = start_agent(directory.path(), "sink", Direction::Output, &healthy, token);
    let config = controller_file(directory.path(), &[&source, &sink]);
    let status = Command::new(env!("CARGO_BIN_EXE_isochronectl"))
        .args([
            config.to_str().unwrap(),
            "connect",
            "will-fail",
            "source/adc",
            "sink/dac",
        ])
        .status()
        .unwrap();
    assert!(!status.success());
    match call(&sink.target, Request::Routes).unwrap() {
        Response::Routes { routes, .. } => assert!(routes.is_empty()),
        response => panic!("unexpected routes response {response:?}"),
    }
}
