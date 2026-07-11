//! A throwaway local HTTP server used as the OAuth redirect target. Both the
//! Twitch (implicit) and Kick (authorization-code) flows redirect the browser
//! to `http://localhost:<port>`; this captures the single redirect request and
//! returns its query string, then serves a "you can close this tab" page.

use std::time::Duration;

use anyhow::{anyhow, Context};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Wait at most this long for the user to approve in the browser.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Binds `127.0.0.1:port` for the OAuth redirect. Returned before opening the
/// browser so the server is ready for the redirect.
pub async fn bind(port: u16) -> anyhow::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("binding redirect server on port {port}"))
}

/// Waits (up to the login timeout) for the redirect and returns its parameters
/// as `(key, value)` pairs. If `forward_fragment` is true, the first request is
/// served a page that POSTs the URL `#fragment` back to us (needed for the
/// implicit flow, where the token is in the fragment and never reaches the
/// server — a POST body also keeps it out of browser history); otherwise the
/// query string is read directly.
pub async fn wait_for_redirect(
    listener: &TcpListener,
    forward_fragment: bool,
) -> anyhow::Result<Vec<(String, String)>> {
    tokio::time::timeout(LOGIN_TIMEOUT, accept(listener, forward_fragment))
        .await
        .map_err(|_| anyhow!("login timed out after {}s", LOGIN_TIMEOUT.as_secs()))?
}

async fn accept(
    listener: &TcpListener,
    forward_fragment: bool,
) -> anyhow::Result<Vec<(String, String)>> {
    loop {
        let (mut stream, _) = listener.accept().await.context("accepting redirect")?;
        let request = read_request(&mut stream).await?;

        // Implicit flow: the bootstrap page POSTs the fragment's parameters as
        // the request body (a fetch, not a reload — so the token never becomes a
        // GET URL that lands in browser history).
        if request.method == "POST" {
            let params = parse_pairs(&request.body);
            respond(&mut stream, done_page()).await?;
            if !params.is_empty() {
                return Ok(params);
            }
            continue;
        }

        let params = parse_query(&request.target);
        if !params.is_empty() || !forward_fragment {
            respond(&mut stream, done_page()).await?;
            return Ok(params);
        }

        // Implicit flow: nothing in the query yet — serve the page that forwards
        // the fragment back to us as a POST.
        respond(&mut stream, bootstrap_page()).await?;
    }
}

/// A minimally-parsed HTTP request: method + target from the request line, plus
/// the body (for the fragment-forwarding POST).
struct Request {
    method: String,
    target: String,
    body: String,
}

/// Reads one HTTP request off `stream`: until the blank line ending the headers
/// (a request can arrive split across TCP segments — a single `read` is not
/// enough), then `Content-Length` more bytes of body. Sizes are capped; this
/// only ever serves the one OAuth redirect.
async fn read_request(stream: &mut TcpStream) -> anyhow::Result<Request> {
    const MAX: usize = 64 * 1024;
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        anyhow::ensure!(buf.len() < MAX, "request headers too large");
        let n = stream.read(&mut tmp).await.context("reading request")?;
        anyhow::ensure!(n > 0, "connection closed mid-request");
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split_whitespace();
    let method = request_line.next().unwrap_or("").to_string();
    let target = request_line.next().unwrap_or("/").to_string();
    let content_length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    anyhow::ensure!(content_length <= MAX, "request body too large");

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream
            .read(&mut tmp)
            .await
            .context("reading request body")?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);
    Ok(Request {
        method,
        target,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn parse_query(target: &str) -> Vec<(String, String)> {
    match target.split_once('?') {
        Some((_, query)) => parse_pairs(query),
        None => Vec::new(),
    }
}

/// Parses `k=v&k2=v2` pairs (a query string or a forwarded fragment body).
fn parse_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

async fn respond(stream: &mut TcpStream, body: &str) -> anyhow::Result<()> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn bootstrap_page() -> &'static str {
    // The token arrives in the URL fragment. It's forwarded as a POST body (not
    // a `location.replace('/?…')` reload) so it never becomes a GET URL that
    // sticks around in the browser's history.
    "<!doctype html><meta charset=utf-8><title>Backseater login</title>\
     <body style=\"background:#111;color:#eee;font-family:sans-serif\">\
     <p>Finishing login…</p>\
     <script>\
     if (location.hash) {\
       fetch('/', {method:'POST', body: location.hash.slice(1)})\
         .then(function(){ document.body.textContent = 'Logged in. You can close this tab and return to Backseater.'; })\
         .catch(function(){ document.body.textContent = 'Login failed. Return to Backseater and try again.'; });\
     } else {\
       document.body.textContent = 'Nothing to finish. You can close this tab.';\
     }\
     </script></body>"
}

fn done_page() -> &'static str {
    "<!doctype html><meta charset=utf-8><title>Backseater login</title>\
     <body style=\"background:#111;color:#eee;font-family:sans-serif\">\
     <p>Logged in. You can close this tab and return to Backseater.</p></body>"
}

/// A random URL-safe token of `len` chars, for PKCE verifiers and OAuth `state`
/// values (both flows). `thread_rng` is a CSPRNG.
pub(crate) fn random_token(len: usize) -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

/// Minimal percent-encoding for query/scope values (spaces, `:`, `/`).
pub fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            ':' => "%3A".to_string(),
            '/' => "%2F".to_string(),
            c => c.to_string(),
        })
        .collect()
}
