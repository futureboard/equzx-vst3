//! Serving the embedded UI to the webview.
//!
//! The obvious route is wry's custom protocol, and that is what this started
//! as. It does not survive contact with current WebView2: the scheme has to be
//! smuggled through as `http://<name>.localhost` and intercepted with
//! `AddWebResourceRequestedFilter`, and on recent runtimes the interception
//! simply never fires — the webview navigates, reports the right URL, and sits
//! on a blank document forever.
//!
//! So the assets are served over a real HTTP connection instead, from a
//! listener bound to loopback on a port the OS picks. There is no protocol
//! trickery left to break: it is an ordinary page load, on every platform, in
//! every runtime. The files still live inside the binary — nothing is written to
//! disk and there is nothing to install.
//!
//! The server only answers `GET`, only from `127.0.0.1`, and only with files
//! that were compiled in, so the exposure is the UI bundle that already ships
//! inside a downloadable plugin.

use std::io::{BufRead, BufReader, Read as _, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::thread;

use include_dir::{include_dir, Dir};

static DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/editor/dist");

/// Longest request line we will read. A URL past this is not one of ours.
const MAX_REQUEST_LINE: usize = 8 * 1024;

pub struct AssetServer {
    addr: SocketAddr,
}

impl AssetServer {
    /// Bind to loopback on an ephemeral port and start serving in the background.
    ///
    /// The thread runs for the life of the process. An editor can be opened and
    /// closed many times over a session and each open would otherwise pay for a
    /// fresh listener; one per plugin instance is cheap and keeps the URL stable.
    pub fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        let addr = listener.local_addr()?;

        thread::Builder::new()
            .name("equzx-assets".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            // Serving is a few memcpys out of a static; doing it
                            // on the accept thread avoids a thread per request.
                            let _ = serve(stream);
                        }
                        // A failed accept is a dropped connection, not a reason
                        // to stop listening.
                        Err(_) => continue,
                    }
                }
            })?;

        Ok(Self { addr })
    }

    /// URL of the editor's entry point.
    pub fn index_url(&self) -> String {
        format!("http://127.0.0.1:{}/index.html", self.addr.port())
    }
}

fn serve(mut stream: TcpStream) -> std::io::Result<()> {
    // Anything not on the loopback interface has no business here. The listener
    // is bound to 127.0.0.1 so this should be unreachable; it is the kind of
    // check that costs nothing and is embarrassing to be missing.
    match stream.peer_addr() {
        Ok(peer) if peer.ip().is_loopback() => {}
        _ => return Ok(()),
    }

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    {
        let mut limited = (&mut reader).take(MAX_REQUEST_LINE as u64);
        limited.read_line(&mut request_line)?;
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    if method != "GET" {
        return write_response(&mut stream, 405, "text/plain", b"method not allowed");
    }

    // Drop the query and fragment; a Vite build has neither, but a webview may.
    let path = target
        .split(['?', '#'])
        .next()
        .unwrap_or("/")
        .trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match lookup(path) {
        Some(file) => write_response(
            &mut stream,
            200,
            mime_for(file.path().extension().and_then(|e| e.to_str())),
            file.contents(),
        ),
        None => write_response(&mut stream, 404, "text/plain", b"not found"),
    }
}

/// Resolve a request path against the bundle.
///
/// `..` is rejected outright rather than normalised: `include_dir` only holds
/// what was compiled in, so an escape can't reach the filesystem, but a path
/// that tries has nothing legitimate behind it either.
fn lookup(path: &str) -> Option<&'static include_dir::File<'static>> {
    if path.split('/').any(|segment| segment == "..") {
        return None;
    }
    // A single-page app resolves its own routes, so anything that isn't a real
    // file is the UI asking for one.
    DIST.get_file(path).or_else(|| DIST.get_file("index.html"))
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn mime_for(ext: Option<&str>) -> &'static str {
    match ext {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("map") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    fn get(server: &AssetServer, target: &str) -> String {
        let mut stream =
            TcpStream::connect(format!("127.0.0.1:{}", server.addr.port())).expect("connect");
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        response
    }

    #[test]
    fn the_built_ui_is_actually_embedded() {
        // `build.rs` writes a placeholder when the Vite build hasn't been run, so
        // this can only fail if `include_dir!` picked up nothing at all.
        let index = DIST
            .get_file("index.html")
            .expect("no index.html in editor/dist");
        assert!(!index.contents().is_empty());
    }

    #[test]
    fn it_serves_the_index() {
        let server = AssetServer::start().expect("bind");
        let response = get(&server, "/index.html");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("text/html"));
        assert!(response.contains("<script") || response.contains("EQUZX"));
    }

    #[test]
    fn the_root_is_the_index() {
        let server = AssetServer::start().expect("bind");
        assert!(get(&server, "/").starts_with("HTTP/1.1 200 OK"));
    }

    #[test]
    fn a_query_string_does_not_confuse_the_lookup() {
        let server = AssetServer::start().expect("bind");
        assert!(get(&server, "/index.html?v=2#top").starts_with("HTTP/1.1 200 OK"));
    }

    #[test]
    fn only_get_is_answered() {
        let server = AssetServer::start().expect("bind");
        let mut stream =
            TcpStream::connect(format!("127.0.0.1:{}", server.addr.port())).expect("connect");
        stream
            .write_all(b"POST /index.html HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        assert!(response.starts_with("HTTP/1.1 405"), "{response}");
    }

    #[test]
    fn traversal_is_refused_rather_than_resolved() {
        assert!(lookup("../../Cargo.toml").is_none());
        assert!(lookup("assets/../../secret").is_none());
    }

    #[test]
    fn an_unknown_route_falls_back_to_the_app() {
        // Not a 404: the single-page app owns its own routing.
        let file = lookup("some/client/route").expect("fallback");
        assert_eq!(file.path().to_string_lossy(), "index.html");
    }

    #[test]
    fn the_url_points_at_loopback() {
        let server = AssetServer::start().expect("bind");
        let url = server.index_url();
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert!(url.ends_with("/index.html"));
    }

    #[test]
    fn known_extensions_get_real_mime_types() {
        assert!(mime_for(Some("js")).starts_with("text/javascript"));
        assert_eq!(mime_for(Some("woff2")), "font/woff2");
        assert_eq!(mime_for(None), "application/octet-stream");
    }
}
