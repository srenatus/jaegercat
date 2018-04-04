extern crate clap;
extern crate jaegercat;
extern crate serdeconv;
#[macro_use]
extern crate slog;
extern crate sloggers;
#[macro_use]
extern crate trackable;
extern crate futures;
extern crate hyper;
extern crate url;

use std::io::{self, Write};
use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::collections::{HashMap, HashSet};
use clap::{App, Arg};
use jaegercat::thrift::{EmitBatchNotification, Protocol};
use sloggers::Build;
use sloggers::terminal::{Destination, TerminalLoggerBuilder};
use sloggers::types::SourceLocation;
use trackable::error::Failure;
use futures::future::Future;

use hyper::header::ContentLength;
use hyper::server::{Http, Request, Response, Service};
use hyper::{Method, StatusCode};
use url::form_urlencoded;

macro_rules! try_parse {
    ($expr:expr) => { track_try_unwrap!($expr.parse().map_err(Failure::from_error)) }
}

static SAMPLE_ALL_RESP: &'static str = r#"{"strategyType": "PROBABILISTIC", "probabilisticSampling": {"samplingRate": 1}}"#;

fn main() {
    let matches = App::new("jaegercat")
        .version(env!("CARGO_PKG_VERSION"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .arg(
            Arg::with_name("COMPACT_THRIFT_PORT")
                .long("compact-thrift-port")
                .takes_value(true)
                .default_value("6831"),
        )
        .arg(
            Arg::with_name("BINARY_THRIFT_PORT")
                .long("binary-thrift-port")
                .takes_value(true)
                .default_value("6832"),
        )
        .arg(
            Arg::with_name("FORMAT")
                .short("f")
                .long("format")
                .takes_value(true)
                .default_value("json")
                .possible_values(&["raw", "json", "json-pretty"]),
        )
        .arg(
            Arg::with_name("UDP_BUFFER_SIZE")
                .short("b")
                .long("udp-buffer-size")
                .takes_value(true)
                .default_value("65000"),
        )
        .arg(
            Arg::with_name("LOG_LEVEL")
                .long("log-level")
                .takes_value(true)
                .default_value("info")
                .possible_values(&["debug", "info", "error"]),
        )
        .arg(
            Arg::with_name("SAMPLE_SERVICES")
                .short("S")
                .long("sample-services")
                .use_delimiter(true)
                .takes_value(true),
        )
        .get_matches();

    let compact_thrift_port: u16 = try_parse!(matches.value_of("COMPACT_THRIFT_PORT").unwrap());
    let binary_thrift_port: u16 = try_parse!(matches.value_of("BINARY_THRIFT_PORT").unwrap());
    let udp_buffer_size: usize = try_parse!(matches.value_of("UDP_BUFFER_SIZE").unwrap());
    let format = match matches.value_of("FORMAT").unwrap() {
        "raw" => Format::Raw,
        "json" => Format::Json,
        "json-pretty" => Format::JsonPretty,
        _ => unreachable!(),
    };
    let log_level = try_parse!(matches.value_of("LOG_LEVEL").unwrap());
    let logger = track_try_unwrap!(
        TerminalLoggerBuilder::new()
            .source_location(SourceLocation::None)
            .destination(Destination::Stderr)
            .level(log_level)
            .build()
    );
    let svcs = matches.values_of("SAMPLE_SERVICES").unwrap_or_default().map(String::from).collect::<HashSet<String>>();

    let mut threads = Vec::new();
    for (port, protocol) in [
        (compact_thrift_port, Protocol::Compact),
        (binary_thrift_port, Protocol::Binary),
    ].iter()
        .cloned()
    {
        let addr: SocketAddr = try_parse!(format!("0.0.0.0:{}", port));
        let socket = track_try_unwrap!(UdpSocket::bind(addr).map_err(Failure::from_error));
        let logger = logger.new(o!("port" => port, "thrift_protocol" => format!("{:?}", protocol)));
        info!(logger, "UDP server started");

        let thread = thread::spawn(move || {
            let mut buf = vec![0; udp_buffer_size];
            loop {
                let (recv_size, peer) =
                    track_try_unwrap!(socket.recv_from(&mut buf).map_err(Failure::from_error));
                debug!(logger, "Received {} bytes from {}", recv_size, peer);
                let mut bytes = &buf[..recv_size];
                match track!(EmitBatchNotification::decode(bytes, protocol)) {
                    Err(e) => {
                        error!(logger, "Received malformed or unknown message: {}", e);
                        debug!(logger, "Bytes: {:?}", bytes);
                    }
                    Ok(message) => {
                        let stdout = io::stdout();
                        let mut stdout = stdout.lock();
                        match format {
                            Format::Raw => {
                                track_try_unwrap!(
                                    io::copy(&mut bytes, &mut stdout).map_err(Failure::from_error)
                                );
                            }
                            Format::Json => {
                                let json = track_try_unwrap!(serdeconv::to_json_string(&message));
                                track_try_unwrap!(
                                    writeln!(stdout, "{}", json).map_err(Failure::from_error)
                                );
                            }
                            Format::JsonPretty => {
                                let json =
                                    track_try_unwrap!(serdeconv::to_json_string_pretty(&message));
                                track_try_unwrap!(
                                    writeln!(stdout, "{}", json).map_err(Failure::from_error)
                                );
                            }
                        }
                    }
                }
            }
        });
        threads.push(thread);
    }

    // agent sampling handler
    let thread = thread::spawn(move || {

        let addr = "127.0.0.1:5778".parse().unwrap();
        let svc = SamplingService{
            logger: logger,
            enabled_services: svcs
        };

        let server = Http::new().bind(&addr, move || Ok(svc.clone())).unwrap();
        server.run().unwrap();
    });
    threads.push(thread);

    for t in threads {
        let _ = t.join();
    }
}

#[derive(Clone)]
struct SamplingService {
    logger: slog::Logger,
    enabled_services: HashSet<String>,
}

impl Service for SamplingService {
    // boilerplate hooking up hyper's server types
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    // The future representing the eventual Response your call will
    // resolve to. This can change to whatever Future you need.
    type Future = Box<Future<Item=Self::Response, Error=Self::Error>>;

    fn call(&self, req: Request) -> Self::Future {
        let response = match (req.method(), req.path()) {
            (&Method::Get, "/sampling") => {
                let query = match req.uri().query() {
                    Some(q) => q,
                    None => "",
                };
                let params = form_urlencoded::parse(query.as_ref()).into_owned().collect::<HashMap<String, String>>();
                let svc = match params.get("service") {
                    Some(n) => n,
                    None => "unknown",
                };
                if self.enabled_services.contains(svc) {
                    info!(self.logger, "enabling service {:?}", svc);
                    Response::new()
                        .with_header(ContentLength(SAMPLE_ALL_RESP.len() as u64))
                        .with_body(SAMPLE_ALL_RESP)
                } else {
                    Response::new().with_status(StatusCode::NotFound)
                }
            },
            _ => Response::new().with_status(StatusCode::NotFound)
        };
        Box::new(futures::future::ok(response))
    }
}

#[derive(Clone, Copy)]
enum Format {
    Raw,
    Json,
    JsonPretty,
}
