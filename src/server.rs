//! The local HTTP server that the bootstrap plugin inside Studio reports to.
//!
//! Protocol (all bodies are JSON, and every POST carries the run's `token`):
//!
//! - `POST /hello`: the plugin asks to run. Only the first plugin that is
//!   running in the place this run opened is accepted; the response carries
//!   the script's arguments. Everyone else gets a 4xx and backs off.
//! - `POST /output`: a batch of messages from Studio's output.
//! - `POST /finish`: the script finished, successfully or not.
//! - `GET /ping/<session id>`: lets other runs tell that this one is alive.

use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    sync::{Arc, mpsc::Sender},
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tiny_http::{Header, Method, Request, Response};

use crate::session::Event;

const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Print,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Deserialize)]
pub struct OutputMessage {
    pub level: Level,
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct Finish {
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct Envelope<T> {
    token: String,
    #[serde(flatten)]
    payload: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Hello {
    place_name: String,
    place_id: i64,
}

#[derive(Serialize)]
struct HelloResponse<'a> {
    args: &'a [String],
}

#[derive(Deserialize)]
struct OutputBatch {
    messages: Vec<OutputMessage>,
}

pub struct Config {
    pub session_id: String,
    pub token: String,
    /// The file name of the place copy that Studio was asked to open.
    pub place_file_name: String,
    pub script_args: Vec<String>,
}

pub struct Server {
    http: Arc<tiny_http::Server>,
    port: u16,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start(port: u16, config: Config, events: Sender<Event>) -> anyhow::Result<Self> {
        let http = tiny_http::Server::http((Ipv4Addr::LOCALHOST, port))
            .map_err(|err| anyhow!(err))
            .with_context(|| {
                if port == 0 {
                    "Could not start the local server Studio reports back to".to_owned()
                } else {
                    format!("Could not listen on port {port}. Is another program using it?")
                }
            })?;
        let http = Arc::new(http);

        let port = http
            .server_addr()
            .to_ip()
            .map(|addr| addr.port())
            .context("Local server is not listening on an IP address")?;

        let thread = thread::Builder::new()
            .name("studio-run-server".to_owned())
            .spawn({
                let http = Arc::clone(&http);
                move || serve(&http, &config, &events)
            })?;

        crate::debug!("listening on 127.0.0.1:{port}");
        Ok(Self {
            http,
            port,
            thread: Some(thread),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.http.unblock();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(http: &tiny_http::Server, config: &Config, events: &Sender<Event>) {
    let mut claimed = false;

    for mut request in http.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_owned();
        crate::debug!("{method} {url}");

        let (status, body) = match (method, url.as_str()) {
            (Method::Get, path) if path.strip_prefix("/ping/") == Some(&config.session_id) => {
                (200, String::new())
            }

            (Method::Post, "/hello") => match read_json::<Hello>(&mut request, config) {
                Err(status) => (status, String::new()),
                Ok(_) if claimed => (409, "This run is already in progress".to_owned()),
                Ok(hello) if !is_our_place(&hello.place_name, &config.place_file_name) => {
                    crate::debug!(
                        "turned away a Studio with place {:?} (place ID {})",
                        hello.place_name,
                        hello.place_id
                    );
                    (409, "This Studio was not opened by this run".to_owned())
                }
                Ok(_) => {
                    claimed = true;
                    let _ = events.send(Event::Connected);
                    let response = HelloResponse {
                        args: &config.script_args,
                    };
                    (
                        200,
                        serde_json::to_string(&response).expect("args serialize"),
                    )
                }
            },

            (Method::Post, "/output") if claimed => {
                match read_json::<OutputBatch>(&mut request, config) {
                    Ok(batch) => {
                        let _ = events.send(Event::Output(batch.messages));
                        (200, String::new())
                    }
                    Err(status) => (status, String::new()),
                }
            }

            (Method::Post, "/finish") if claimed => match read_json::<Finish>(&mut request, config)
            {
                Ok(finish) => {
                    let _ = events.send(Event::Finished(finish));
                    (200, String::new())
                }
                Err(status) => (status, String::new()),
            },

            _ => (404, String::new()),
        };

        let content_type =
            Header::from_bytes("Content-Type", "application/json").expect("valid header");
        let response = Response::from_string(body)
            .with_status_code(status)
            .with_header(content_type);
        if let Err(err) = request.respond(response) {
            crate::debug!("could not respond to Studio: {err}");
        }
    }
}

/// Studio names a place opened from disk after its file, but the extension
/// has not always been included, so accept either form.
fn is_our_place(place_name: &str, place_file_name: &str) -> bool {
    let stem = place_file_name
        .rsplit_once('.')
        .map_or(place_file_name, |(stem, _)| stem);
    place_name == place_file_name || place_name == stem
}

fn read_json<T: DeserializeOwned>(request: &mut Request, config: &Config) -> Result<T, u16> {
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES)
        .read_to_end(&mut body)
        .map_err(|_| 400u16)?;

    let envelope: Envelope<T> = serde_json::from_slice(&body).map_err(|err| {
        crate::debug!("malformed request from Studio: {err}");
        400u16
    })?;

    if envelope.token != config.token {
        return Err(403);
    }

    Ok(envelope.payload)
}

/// Checks whether a studio-run process owning `session_id` is still listening
/// on `port`.
pub fn is_session_alive(port: u16, session_id: &str) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else {
        return false;
    };

    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));

    let request = format!(
        "GET /ping/{session_id} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let mut status_line = [0u8; 12];
    stream.read_exact(&mut status_line).is_ok() && status_line.ends_with(b" 200")
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn start_test_server() -> (Server, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let config = Config {
            session_id: "abc123".to_owned(),
            token: "secret".to_owned(),
            place_file_name: "studio-run-abc123.rbxl".to_owned(),
            script_args: vec!["one".to_owned(), "two".to_owned()],
        };
        (Server::start(0, config, tx).unwrap(), rx)
    }

    fn post(port: u16, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response[9..12].parse().unwrap();
        let body = response.split_once("\r\n\r\n").unwrap().1.to_owned();
        (status, body)
    }

    #[test]
    fn matches_place_names_with_or_without_extension() {
        assert!(is_our_place("studio-run-abc.rbxl", "studio-run-abc.rbxl"));
        assert!(is_our_place("studio-run-abc", "studio-run-abc.rbxl"));
        assert!(!is_our_place("kaiju-main.rbxl", "studio-run-abc.rbxl"));
        assert!(!is_our_place("Place1", "studio-run-abc.rbxl"));
    }

    #[test]
    fn accepts_only_one_studio_with_the_right_token_and_place() {
        let (server, events) = start_test_server();
        let port = server.port();
        let hello = |token: &str, place: &str| {
            post(
                port,
                "/hello",
                &format!(r#"{{"token":"{token}","placeName":"{place}","placeId":0}}"#),
            )
        };

        assert_eq!(hello("wrong", "studio-run-abc123.rbxl").0, 403);
        assert_eq!(hello("secret", "SomeoneElsesPlace.rbxl").0, 409);
        assert_eq!(
            post(port, "/output", r#"{"token":"secret","messages":[]}"#).0,
            404
        );

        let (status, body) = hello("secret", "studio-run-abc123.rbxl");
        assert_eq!(status, 200);
        assert_eq!(body, r#"{"args":["one","two"]}"#);
        assert!(matches!(events.try_recv(), Ok(Event::Connected)));

        assert_eq!(hello("secret", "studio-run-abc123.rbxl").0, 409);
    }

    #[test]
    fn forwards_output_and_finish() {
        let (server, events) = start_test_server();
        let port = server.port();
        post(
            port,
            "/hello",
            r#"{"token":"secret","placeName":"studio-run-abc123","placeId":0}"#,
        );
        let _ = events.try_recv();

        let batch = r#"{"token":"secret","messages":[{"level":"warning","body":"careful"}]}"#;
        assert_eq!(post(port, "/output", batch).0, 200);
        match events.try_recv() {
            Ok(Event::Output(messages)) => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].level, Level::Warning);
                assert_eq!(messages[0].body, "careful");
            }
            _ => panic!("expected output"),
        }

        let finish = r#"{"token":"secret","success":false,"error":"boom"}"#;
        assert_eq!(post(port, "/finish", finish).0, 200);
        match events.try_recv() {
            Ok(Event::Finished(finish)) => {
                assert!(!finish.success);
                assert_eq!(finish.error.as_deref(), Some("boom"));
            }
            _ => panic!("expected finish"),
        }
    }

    #[test]
    fn answers_pings_for_its_own_session_only() {
        let (server, _events) = start_test_server();
        assert!(is_session_alive(server.port(), "abc123"));
        assert!(!is_session_alive(server.port(), "ffffff"));

        let port = server.port();
        drop(server);
        assert!(!is_session_alive(port, "abc123"));
    }
}
