use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use isochrone::control::{
    AgentTarget, ControllerConfig, Direction, Request, Response, call, split_endpoint,
};

fn invalid_input(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn usage() -> io::Error {
    invalid_input(
        "usage:\n  isochronectl <control.json> inventory\n  isochronectl <control.json> routes\n  isochronectl <control.json> connect <route-id> <node/input> <node/output> [channels] [gain-db] [latency-ms] [period-frames] [port]\n  isochronectl <control.json> disconnect <route-id>",
    )
}

fn expect_inventory(target: &AgentTarget) -> io::Result<Vec<isochrone::control::Endpoint>> {
    match call(target, Request::Inventory)? {
        Response::Inventory { node_id, endpoints } if node_id == target.node_id => Ok(endpoints),
        Response::Error { message } => Err(io::Error::other(message)),
        response => Err(invalid_data(format!(
            "{} returned unexpected response {response:?}",
            target.node_id
        ))),
    }
}

fn parse_channels(value: Option<&String>, endpoint_channels: &[u16]) -> io::Result<[u16; 2]> {
    if let Some(value) = value {
        let (left, right) = value
            .split_once(',')
            .ok_or_else(|| invalid_input("channels must look like 5,6"))?;
        return Ok([
            left.parse().map_err(invalid_input)?,
            right.parse().map_err(invalid_input)?,
        ]);
    }
    match endpoint_channels {
        [left, right, ..] => Ok([*left, *right]),
        _ => Ok([1, 2]),
    }
}

fn print_response(response: &Response) -> io::Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(response).map_err(io::Error::other)?
    );
    Ok(())
}

fn stop_route(target: &AgentTarget, route_id: &str) {
    let _ = call(
        target,
        Request::Stop {
            route_id: route_id.into(),
        },
    );
}

fn wait_for_packets(target: &AgentTarget, route_id: &str) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match call(target, Request::Routes)? {
            Response::Routes { routes, .. } => {
                let route = routes
                    .iter()
                    .find(|route| route.route_id == route_id)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "receiver route vanished")
                    })?;
                match route.phase {
                    isochrone::control::RoutePhase::Running => return Ok(()),
                    isochrone::control::RoutePhase::Failed => {
                        return Err(io::Error::other(
                            route
                                .error
                                .clone()
                                .unwrap_or_else(|| "receiver failed".into()),
                        ));
                    }
                    isochrone::control::RoutePhase::Starting => {}
                }
            }
            Response::Error { message } => return Err(io::Error::other(message)),
            response => {
                return Err(invalid_data(format!(
                    "receiver returned unexpected status {response:?}"
                )));
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "receiver saw no RTP packets within 5 seconds",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn connect(config: &ControllerConfig, args: &[String]) -> io::Result<()> {
    if !(3..=8).contains(&args.len()) {
        return Err(usage());
    }
    let route_id = &args[0];
    let (source_node, source_id) = split_endpoint(&args[1])?;
    let (sink_node, sink_id) = split_endpoint(&args[2])?;
    let source = config.by_id(source_node)?;
    let sink = config.by_id(sink_node)?;
    let source_endpoints = expect_inventory(source)?;
    let sink_endpoints = expect_inventory(sink)?;
    let source_endpoint = source_endpoints
        .iter()
        .find(|endpoint| endpoint.id == source_id && endpoint.direction == Direction::Input)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("input endpoint {} not found on {source_node}", source_id),
            )
        })?;
    let _sink_endpoint = sink_endpoints
        .iter()
        .find(|endpoint| endpoint.id == sink_id && endpoint.direction == Direction::Output)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("output endpoint {} not found on {sink_node}", sink_id),
            )
        })?;
    let channels = parse_channels(args.get(3), &source_endpoint.channels)?;
    let gain_db = args
        .get(4)
        .map(|value| value.parse().map_err(invalid_input))
        .transpose()?
        .unwrap_or(0.0);
    let latency_ms = args
        .get(5)
        .map(|value| value.parse().map_err(invalid_input))
        .transpose()?
        .unwrap_or(20);
    let period_frames = args
        .get(6)
        .map(|value| value.parse().map_err(invalid_input))
        .transpose()?
        .unwrap_or(96);
    let port = args
        .get(7)
        .map(|value| value.parse().map_err(invalid_input))
        .transpose()?
        .unwrap_or(0);

    let receiver = call(
        sink,
        Request::PrepareReceiver {
            route_id: route_id.clone(),
            endpoint_id: sink_id.into(),
            port,
            latency_ms,
            period_frames,
        },
    )?;
    let destination = match receiver {
        Response::ReceiverReady { destination, .. } => destination,
        Response::Error { message } => return Err(io::Error::other(message)),
        response => {
            return Err(invalid_data(format!(
                "receiver returned unexpected response {response:?}"
            )));
        }
    };

    let sender = call(
        source,
        Request::StartSender {
            route_id: route_id.clone(),
            endpoint_id: source_id.into(),
            destination,
            channels,
            gain_db,
        },
    );
    match sender {
        Ok(response @ Response::SenderStarted { .. }) => {
            if let Err(error) = wait_for_packets(sink, route_id) {
                stop_route(source, route_id);
                stop_route(sink, route_id);
                return Err(io::Error::new(
                    error.kind(),
                    format!("route verification failed; both sides rolled back: {error}"),
                ));
            }
            println!(
                "route {route_id} active and receiving RTP: {source_node}/{source_id} -> {sink_node}/{sink_id}"
            );
            print_response(&response)
        }
        Ok(Response::Error { message }) => {
            stop_route(sink, route_id);
            Err(io::Error::other(format!(
                "sender rejected route; receiver rolled back: {message}"
            )))
        }
        Ok(response) => {
            stop_route(sink, route_id);
            Err(invalid_data(format!(
                "sender returned unexpected response; receiver rolled back: {response:?}"
            )))
        }
        Err(error) => {
            stop_route(sink, route_id);
            Err(io::Error::new(
                error.kind(),
                format!("sender failed; receiver rolled back: {error}"),
            ))
        }
    }
}

fn run() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [config_path, command, rest @ ..] = args.as_slice() else {
        return Err(usage());
    };
    let config = ControllerConfig::read(Path::new(config_path))?;
    match command.as_str() {
        "inventory" if rest.is_empty() => {
            for agent in &config.agents {
                print_response(&call(agent, Request::Inventory)?)?;
            }
            Ok(())
        }
        "routes" if rest.is_empty() => {
            for agent in &config.agents {
                print_response(&call(agent, Request::Routes)?)?;
            }
            Ok(())
        }
        "connect" => connect(&config, rest),
        "disconnect" if rest.len() == 1 => {
            let mut stopped = 0;
            for agent in &config.agents {
                match call(
                    agent,
                    Request::Stop {
                        route_id: rest[0].clone(),
                    },
                )? {
                    Response::Stopped { existed: true, .. } => stopped += 1,
                    Response::Stopped { existed: false, .. } => {}
                    Response::Error { message } => return Err(io::Error::other(message)),
                    response => {
                        return Err(invalid_data(format!(
                            "unexpected stop response {response:?}"
                        )));
                    }
                }
            }
            println!("route {} stopped on {stopped} agent(s)", rest[0]);
            Ok(())
        }
        _ => Err(usage()),
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("isochronectl: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_channels_win_and_defaults_are_safe() {
        assert_eq!(parse_channels(None, &[5, 6]).unwrap(), [5, 6]);
        assert_eq!(parse_channels(None, &[]).unwrap(), [1, 2]);
        assert_eq!(
            parse_channels(Some(&"7,8".into()), &[5, 6]).unwrap(),
            [7, 8]
        );
    }
}
