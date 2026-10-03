//! D3a: the static half of the `--ws-bind` port. `--web-root DIR` makes the
//! listener answer plain HTTP `GET`/`HEAD` for the web build and the game
//! pack, plus a generated `/master.json`; `Upgrade: websocket` requests still
//! go to the game transport (`websocket.rs`). Plain HTTP/1.1, one request per
//! connection (`Connection: close`), bodies streamed so the ~400 MB pack is
//! never in memory, no Range. No TLS: same trust model as the ws listener.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::ServiceState;

/// Longest request head accepted (request line plus headers).
pub const MAX_HEAD_BYTES: usize = 8 * 1024;
/// Deadline for the whole head to arrive.
pub const HEAD_DEADLINE: Duration = Duration::from_secs(10);
/// Concurrent HTTP connections (head reading and file sends). Separate from
/// the player caps in `main.rs`: a page load opens several parallel
/// connections and the per-address cap of 32 is meant for players.
pub const MAX_HTTP_CONNECTIONS: usize = 256;

/// The WebTransport listener's port and certificate hash, for `/master.json`.
pub struct WtInfo {
    pub port: u16,
    pub hash_hex: String,
}

impl WtInfo {
    /// From the `webtransport.json` the wt listener writes (`"port": N`,
    /// `"hash_hex": "..."`, one per line).
    pub fn from_json(json: &str) -> Result<Self, String> {
        let field = |key: &str| {
            json.lines()
                .find_map(|line| {
                    line.trim()
                        .strip_prefix(&format!("\"{key}\":"))
                        .map(|rest| {
                            rest.trim()
                                .trim_end_matches(',')
                                .trim_matches('"')
                                .to_owned()
                        })
                })
                .ok_or_else(|| format!("webtransport.json has no {key}"))
        };
        Ok(Self {
            port: field("port")?
                .parse()
                .map_err(|_| "webtransport.json port is not a number".to_owned())?,
            hash_hex: field("hash_hex")?,
        })
    }
}

pub struct WebConfig {
    /// Canonical (symlinks resolved) root; every served file must stay in it.
    pub root: PathBuf,
    pub wt: Option<WtInfo>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Head {
    pub method: String,
    pub target: String,
    pub upgrade_websocket: bool,
    pub accept_gzip: bool,
    /// Bytes of the head, through the blank line.
    pub len: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Partial,
    Complete(Head),
    Bad,
}

/// Parses a request head with `httparse`. `Partial` until the blank line.
pub fn parse_head(buf: &[u8]) -> Parsed {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    let len = match request.parse(buf) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) => return Parsed::Partial,
        Err(_) => return Parsed::Bad,
    };
    let (Some(method), Some(target)) = (request.method, request.path) else {
        return Parsed::Bad;
    };
    let header = |name: &str| {
        request
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| String::from_utf8_lossy(h.value).to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(",")
    };
    Parsed::Complete(Head {
        method: method.to_owned(),
        target: target.to_owned(),
        upgrade_websocket: header("upgrade").contains("websocket"),
        accept_gzip: header("accept-encoding")
            .split(',')
            .any(|coding| coding.split(';').next().map(str::trim) == Some("gzip")),
        len,
    })
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = raw.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The path of a request target relative to the root: query and fragment cut,
/// percent-decoded, `.` and empty segments dropped, and `..`, backslashes,
/// colons (drive letters, streams) and NULs rejected. `None` is a bad request.
pub fn relative_path(target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next()?;
    if !path.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(path)?;
    let mut relative = PathBuf::new();
    for segment in decoded.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains(['\\', ':', '\0']) {
            return None;
        }
        relative.push(segment);
    }
    Some(relative)
}

/// The file for `relative` under the canonical `root`: a directory serves its
/// `index.html`, and the canonical path must stay under the root (symlink
/// escapes are refused).
pub fn locate(root: &Path, relative: &Path) -> Option<PathBuf> {
    let mut path = root.join(relative);
    if path.is_dir() {
        path.push("index.html");
    }
    canonical_file(root, path)
}

fn canonical_file(root: &Path, path: PathBuf) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    (canonical.starts_with(root) && canonical.is_file()).then_some(canonical)
}

fn mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("json") => "application/json",
        Some("css") => "text/css; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn no_cache(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("html" | "htm" | "json")
    )
}

fn json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `/master.json`: where the game socket is, and the rooms as `ListRooms`
/// reports them. Built per request from the live state.
async fn master_json(config: &WebConfig, state: &Mutex<ServiceState>) -> String {
    let mut adverts: Vec<_> = {
        let state = state.lock().await;
        state
            .rooms
            .values()
            .map(|room| room.view.advert())
            .collect()
    };
    adverts.sort_by_key(|advert| advert.id);
    let mut out = String::from("{\"ws\":\"/\",\"webtransport\":");
    match &config.wt {
        Some(wt) => out.push_str(&format!(
            "{{\"port\":{},\"hash_hex\":\"{}\"}}",
            wt.port, wt.hash_hex
        )),
        None => out.push_str("null"),
    }
    out.push_str(",\"rooms\":[");
    for (index, advert) in adverts.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"id\":\"{}\",\"name\":", advert.id));
        json_string(&mut out, &advert.name);
        out.push_str(",\"map\":");
        json_string(&mut out, &advert.map);
        out.push_str(",\"mode\":");
        json_string(&mut out, &advert.mode);
        out.push_str(&format!(
            ",\"players\":{},\"max_players\":{},\"in_match\":{},\"locked\":{}}}",
            advert.players, advert.max_players, advert.in_match, advert.locked
        ));
    }
    out.push_str("]}\n");
    out
}

async fn respond(
    tcp: &mut TcpStream,
    status: u16,
    reason: &str,
    extra: &str,
    body: &str,
    head_only: bool,
) -> std::io::Result<(u16, u64)> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    tcp.write_all(head.as_bytes()).await?;
    if !head_only {
        tcp.write_all(body.as_bytes()).await?;
    }
    Ok((status, body.len() as u64))
}

/// A 400 for a head that did not parse (the connection is not read further).
pub async fn bad_request(tcp: &mut TcpStream) -> std::io::Result<()> {
    respond(tcp, 400, "Bad Request", "", "bad request\n", false).await?;
    tcp.shutdown().await
}

/// Answers one parsed request and closes. Returns the status and body bytes.
pub async fn serve(
    tcp: &mut TcpStream,
    head: &Head,
    config: &WebConfig,
    state: &Mutex<ServiceState>,
) -> std::io::Result<(u16, u64)> {
    // Consume the head so closing does not reset the connection under the
    // response (unread bytes make close send RST).
    let mut sink = vec![0_u8; head.len];
    tcp.read_exact(&mut sink).await?;
    let result = serve_inner(tcp, head, config, state).await;
    let _ = tcp.shutdown().await;
    result
}

async fn serve_inner(
    tcp: &mut TcpStream,
    head: &Head,
    config: &WebConfig,
    state: &Mutex<ServiceState>,
) -> std::io::Result<(u16, u64)> {
    let head_only = match head.method.as_str() {
        "GET" => false,
        "HEAD" => true,
        _ => {
            return respond(
                tcp,
                405,
                "Method Not Allowed",
                "Allow: GET, HEAD\r\n",
                "method not allowed\n",
                false,
            )
            .await;
        }
    };
    let Some(relative) = relative_path(&head.target) else {
        return respond(tcp, 400, "Bad Request", "", "bad path\n", head_only).await;
    };
    if relative == Path::new("master.json") {
        let body = master_json(config, state).await;
        let head_text = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tcp.write_all(head_text.as_bytes()).await?;
        if !head_only {
            tcp.write_all(body.as_bytes()).await?;
        }
        return Ok((200, body.len() as u64));
    }
    let Some(path) = locate(&config.root, &relative) else {
        return respond(tcp, 404, "Not Found", "", "not found\n", head_only).await;
    };
    // `X` as `X.gz` with Content-Encoding when the client takes gzip and the
    // build step pre-compressed it (the wasm).
    let mut served = path.clone();
    let mut gzip = false;
    if head.accept_gzip && path.extension().and_then(|e| e.to_str()) != Some("gz") {
        let mut name = path.clone().into_os_string();
        name.push(".gz");
        if let Some(gz) = canonical_file(&config.root, PathBuf::from(name)) {
            served = gz;
            gzip = true;
        }
    }
    let mut file = match tokio::fs::File::open(&served).await {
        Ok(file) => file,
        Err(_) => return respond(tcp, 404, "Not Found", "", "not found\n", head_only).await,
    };
    let metadata = file.metadata().await?;
    let length = metadata.len();
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {length}\r\nConnection: close\r\n",
        mime(&path)
    );
    // Validator the page's pack cache compares (index.html fetchPack): size + mtime.
    if let Ok(age) = metadata.modified().map(|m| m.duration_since(std::time::UNIX_EPOCH)) {
        let secs = age.map(|d| d.as_secs()).unwrap_or(0);
        out.push_str(&format!("ETag: \"{length:x}-{secs:x}\"\r\n"));
    }
    if no_cache(&path) {
        out.push_str("Cache-Control: no-cache\r\n");
    }
    if gzip {
        // gzip ISIZE (last 4 bytes): the page's progress bar counts decoded bytes.
        if length >= 4 {
            let mut isize = [0_u8; 4];
            file.seek(SeekFrom::End(-4)).await?;
            file.read_exact(&mut isize).await?;
            file.seek(SeekFrom::Start(0)).await?;
            out.push_str(&format!(
                "X-Uncompressed-Length: {}\r\n",
                u32::from_le_bytes(isize)
            ));
        }
        out.push_str("Content-Encoding: gzip\r\nVary: Accept-Encoding\r\n");
    }
    out.push_str("\r\n");
    tcp.write_all(out.as_bytes()).await?;
    if head_only {
        return Ok((200, 0));
    }
    let sent = tokio::io::copy(&mut file, tcp).await?;
    Ok((200, sent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn parses_heads() {
        let raw = b"GET /a%20b?x=1 HTTP/1.1\r\nHost: h\r\nAccept-Encoding: br, gzip;q=0.8\r\nUpgrade: WebSocket\r\n\r\n";
        let Parsed::Complete(head) = parse_head(raw) else {
            panic!("not complete");
        };
        assert_eq!(head.method, "GET");
        assert_eq!(head.target, "/a%20b?x=1");
        assert!(head.accept_gzip && head.upgrade_websocket);
        assert_eq!(head.len, raw.len());
        assert_eq!(
            parse_head(b"GET / HTTP/1.1\r\nHost: h\r\n"),
            Parsed::Partial
        );
        assert_eq!(parse_head(b"\x01\x02 garbage\r\n\r\n"), Parsed::Bad);
        let Parsed::Complete(plain) =
            parse_head(b"GET / HTTP/1.1\r\nAccept-Encoding: identity\r\n\r\n")
        else {
            panic!("not complete");
        };
        assert!(!plain.accept_gzip && !plain.upgrade_websocket);
    }

    #[test]
    fn normalises_paths_and_rejects_traversal() {
        assert_eq!(relative_path("/"), Some(PathBuf::new()));
        assert_eq!(
            relative_path("/a/./b//c.js?v=2#x"),
            Some(PathBuf::from("a/b/c.js"))
        );
        assert_eq!(relative_path("/a%20b"), Some(PathBuf::from("a b")));
        for bad in [
            "/../x",
            "/a/../../x",
            "/%2e%2e/x",
            "/%2E%2E%2Fx",
            "/a/..%2fx",
            "/..%5cx",
            "/a\\b",
            "/C:/x",
            "/a%00b",
            "/%zz",
            "/%ff",
            "x",
        ] {
            assert_eq!(relative_path(bad), None, "{bad}");
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("iw4l-web-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("index.html"), "<html>hi</html>").unwrap();
        std::fs::write(dir.join("sub").join("index.html"), "sub").unwrap();
        std::fs::write(dir.join("a.wasm"), b"rawrawraw").unwrap();
        std::fs::write(dir.join("a.wasm.gz"), b"gz-body\x2a\x00\x00\x00").unwrap();
        std::fs::write(
            dir.parent().unwrap().join(format!("outside-{name}.txt")),
            "secret",
        )
        .unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn locates_only_under_the_root() {
        let root = temp_root("locate");
        assert_eq!(locate(&root, Path::new("")), Some(root.join("index.html")));
        assert_eq!(
            locate(&root, Path::new("sub")),
            Some(root.join("sub").join("index.html"))
        );
        assert_eq!(locate(&root, Path::new("missing")), None);
        // Not reachable through `relative_path`, but `locate` holds on its own.
        assert_eq!(locate(&root, Path::new("../outside-locate.txt")), None);
    }

    async fn get(root: &Path, request: &str) -> (String, Vec<u8>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = WebConfig {
            root: root.to_owned(),
            wt: None,
        };
        let state = Mutex::new(ServiceState::default());
        let server = async {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; MAX_HEAD_BYTES];
            let n = tcp.peek(&mut buf).await.unwrap();
            let Parsed::Complete(head) = parse_head(&buf[..n]) else {
                panic!("head");
            };
            serve(&mut tcp, &head, &config, &state).await.unwrap();
        };
        let client = async {
            let mut tcp = TcpStream::connect(addr).await.unwrap();
            tcp.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            tcp.read_to_end(&mut response).await.unwrap();
            response
        };
        let ((), response) = tokio::join!(server, client);
        let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        (
            String::from_utf8_lossy(&response[..split]).into_owned(),
            response[split..].to_vec(),
        )
    }

    #[tokio::test]
    async fn negotiates_gzip() {
        let root = temp_root("gzip");
        let (head, body) = get(
            &root,
            "GET /a.wasm HTTP/1.1\r\nHost: h\r\nAccept-Encoding: gzip, br\r\n\r\n",
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("Content-Encoding: gzip"), "{head}");
        assert!(head.contains("Content-Type: application/wasm"), "{head}");
        assert!(head.contains("X-Uncompressed-Length: 42"), "{head}");
        assert!(head.contains("Content-Length: 11"), "{head}");
        assert_eq!(body, b"gz-body\x2a\x00\x00\x00");
        let (head, body) = get(&root, "GET /a.wasm HTTP/1.1\r\nHost: h\r\n\r\n").await;
        assert!(!head.contains("Content-Encoding"), "{head}");
        assert!(head.contains("Content-Length: 9"), "{head}");
        assert_eq!(body, b"rawrawraw");
    }

    #[tokio::test]
    async fn serves_index_head_and_refuses_the_rest() {
        let root = temp_root("misc");
        let (head, body) = get(&root, "GET / HTTP/1.1\r\n\r\n").await;
        assert!(head.contains("Cache-Control: no-cache"), "{head}");
        assert!(head.contains("text/html"), "{head}");
        assert_eq!(body, b"<html>hi</html>");
        let (head, body) = get(&root, "HEAD /index.html HTTP/1.1\r\n\r\n").await;
        assert!(head.contains("Content-Length: 15"), "{head}");
        assert!(body.is_empty());
        let (head, _) = get(&root, "POST /index.html HTTP/1.1\r\n\r\n").await;
        assert!(head.starts_with("HTTP/1.1 405"), "{head}");
        let (head, _) = get(&root, "GET /%2e%2e/outside-misc.txt HTTP/1.1\r\n\r\n").await;
        assert!(head.starts_with("HTTP/1.1 400"), "{head}");
        let (head, _) = get(&root, "GET /nope HTTP/1.1\r\n\r\n").await;
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        let (head, body) = get(&root, "GET /master.json?x=1 HTTP/1.1\r\n\r\n").await;
        assert!(head.contains("application/json"), "{head}");
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "{\"ws\":\"/\",\"webtransport\":null,\"rooms\":[]}\n"
        );
    }
}
