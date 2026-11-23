//! The o-charts chart store: sign in, see what the account owns, download it.
//!
//! Manx reads o-charts cells, so it should be able to fetch them too rather
//! than sending the user to another program. The account, the entitlements and
//! the machine assignment all live on o-charts' server; this is a client for
//! that server, written from the protocol as observed, sharing no code with the
//! GPL-2.0 plugin it was learned from.
//!
//! What this does **not** do: it does not decrypt anything, and it does not
//! forge entitlement. Cells arrive encrypted, and opening them still needs the
//! machine's own licence and the vendor's `oexserverd` — the same path Manx
//! already uses to read the charts on disk today.
//!
//! Sequence, once per machine:
//!
//! 1. `login` — email and password, returns a session key
//! 2. `identify_system` — send this machine's fingerprint, learn its name, or
//!    be told the machine is new
//! 3. `register_system` — give a new machine a name
//!
//! and then, per chart:
//!
//! 4. `list_charts` — everything the account owns, with the shop's edition
//! 5. `assign` — claim one of the chart's slots for this machine
//! 6. `request_download` — a signed link to the package and to its keys

pub mod eula;
pub mod install;
pub mod noaa;
pub mod installed;
pub mod protocol;
pub mod service;
pub mod types;

use std::path::Path;

/// This machine, as the shop knows it.
///
/// The fingerprint is produced by the vendor's `oexserverd`, which is also what
/// decrypts the cells — Manx already runs it, so the file usually exists
/// before the shop is ever opened.
pub struct Fingerprint {
    pub bytes: Vec<u8>,
    /// The file's own name, e.g. `oc01D_1481247791.fpr`. The shop stores it
    /// alongside the fingerprint, so it has to travel with it.
    pub name: String,
}

impl Fingerprint {
    /// Read the fingerprint the decryptor resolved to.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            bytes: std::fs::read(path)?,
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        })
    }
}

use std::io::Read;
use std::time::Duration;

use protocol::{ShopError, ENDPOINT};
use types::{Chart, DownloadGrant, DownloadTarget, Edition};

/// The client version the shop is told about. See [`protocol::client_version`].
const COMPATIBLE_PLUGIN_VERSION: &str = "3.4.4";

/// How long any single shop call may take.
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub enum Error {
    /// The shop answered, and said no.
    Shop(ShopError),
    /// The shop could not be reached, or did not answer in time.
    Transport(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Shop(e) => write!(f, "{e}"),
            Error::Transport(e) => write!(f, "could not reach o-charts: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ShopError> for Error {
    fn from(e: ShopError) -> Self {
        Error::Shop(e)
    }
}

/// A signed-in session.
pub struct Session {
    pub username: String,
    /// The session token. Treat as a credential.
    pub key: String,
    /// This machine's name at the shop, once it is known.
    pub system_name: Option<String>,
}

/// How long one request of a package download may run.
///
/// ureq has no idle timeout, and its "receive response" limit keeps running
/// through the body: the twenty seconds this used to allow for the reply to
/// *start* cut every package off twenty seconds in, so any chart set that
/// took longer than that to arrive could never be downloaded. So, as for the
/// NOAA packages (`noaa.rs`), each request gets a minute and the download
/// continues with a Range request: a slow link finishes in several pieces,
/// and a dead one is noticed within minutes rather than never.
const DOWNLOAD_CALL: Duration = Duration::from_secs(60);
/// Requests in a row that may bring nothing new before a download gives up.
const MAX_STALLS: u32 = 3;

fn download_agent(per_call: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(TIMEOUT))
        .timeout_per_call(Some(per_call))
        .build()
        .into()
}

pub struct ShopClient {
    agent: ureq::Agent,
    /// Chart packages: each request is bounded, and the download carries
    /// on from where a request stopped. See [`DOWNLOAD_CALL`].
    downloads: ureq::Agent,
}

impl Default for ShopClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ShopClient {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .build();
        let downloads = download_agent(DOWNLOAD_CALL);
        Self {
            agent: config.into(),
            downloads: downloads.into(),
        }
    }

    fn post(&self, body: String) -> Result<protocol::Reply, Error> {
        let mut response = self
            .agent
            .post(ENDPOINT)
            .content_type("application/x-www-form-urlencoded")
            .send(body.as_str())
            .map_err(|e| Error::Transport(e.to_string()))?;
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(protocol::parse(&text)?)
    }

    /// Sign in. The password is sent hex-encoded, which is what the server
    /// expects and is not encryption — see [`protocol::encode_password`].
    pub fn login(&self, username: &str, password: &str) -> Result<Session, Error> {
        let reply = self.post(protocol::form(&[
            ("taskId", "login2"),
            ("username", username),
            ("password", &protocol::encode_password(password)),
            ("version", &protocol::client_version()),
        ]))?;
        let key = reply.value("key").unwrap_or_default().to_string();
        if key.is_empty() {
            return Err(Error::Shop(ShopError {
                code: "1".into(),
                message: "the shop accepted the login but returned no session key".into(),
            }));
        }
        Ok(Session {
            username: username.to_string(),
            key,
            system_name: None,
        })
    }

    /// Ask the shop what it calls this machine.
    ///
    /// `Ok(None)` means the machine is new to the account and needs a name;
    /// that is the shop's `8l`, which is an answer rather than a failure.
    pub fn identify_system(
        &self,
        session: &Session,
        fingerprint: &[u8],
        fingerprint_name: &str,
    ) -> Result<Option<String>, Error> {
        let reply = self.post(protocol::form(&[
            ("taskId", "identifySystem"),
            ("username", &session.username),
            ("key", &session.key),
            ("xfpr", &protocol::encode_bytes(fingerprint)),
            ("xfprName", fingerprint_name),
            ("version", &protocol::client_version()),
        ]));
        match reply {
            Ok(r) => Ok(r.value("systemName").map(str::to_string)),
            Err(Error::Shop(e)) if e.code == "8l" => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Register this machine under a name the user chose.
    pub fn register_system(
        &self,
        session: &Session,
        system_name: &str,
        fingerprint: &[u8],
        fingerprint_name: &str,
    ) -> Result<(), Error> {
        self.post(protocol::form(&[
            ("taskId", "xfpr"),
            ("username", &session.username),
            ("key", &session.key),
            ("systemName", system_name),
            ("xfpr", &protocol::encode_bytes(fingerprint)),
            ("xfprName", fingerprint_name),
            ("version", &protocol::client_version()),
        ]))?;
        Ok(())
    }

    /// Everything the account owns.
    pub fn list_charts(&self, session: &Session) -> Result<(Vec<Chart>, Vec<String>), Error> {
        let reply = self.post(protocol::form(&[
            ("taskId", "getlist"),
            ("username", &session.username),
            ("key", &session.key),
            ("version", &protocol::client_version()),
        ]))?;
        Ok((reply.charts, reply.system_names))
    }

    /// Claim one of a chart's slots for this machine.
    ///
    /// Returns the slot's id. A chart already assigned here answers `20`, which
    /// the caller can treat as success.
    pub fn assign(
        &self,
        session: &Session,
        chart: &Chart,
        quantity_id: &str,
        system_name: &str,
    ) -> Result<String, Error> {
        let reply = self.post(protocol::form(&[
            ("taskId", "assign"),
            ("username", &session.username),
            ("key", &session.key),
            ("systemName", system_name),
            ("order", &chart.order),
            ("chartid", &chart.id),
            ("quantityId", quantity_id),
            ("version", &protocol::client_version()),
        ]))?;
        Ok(reply.value("slotUuid").unwrap_or_default().to_string())
    }

    /// Ask for a download. Returns the package and key links.
    /// `requested_version` is sent verbatim rather than as an [`Edition`],
    /// because the shop's own strings are the safest thing to echo back: the
    /// chart's `edition` arrives as `2026/1-29` but a slot's `lastRequested`
    /// arrives as `1-20`, and re-formatting either one risks asking for an
    /// edition that does not exist.
    pub fn request_download(
        &self,
        session: &Session,
        slot_uuid: &str,
        system_name: &str,
        target: DownloadTarget,
        requested_version: &str,
        installed: Option<Edition>,
    ) -> Result<DownloadGrant, Error> {
        let Some(requested_file) = target.requested_file() else {
            return Ok(DownloadGrant::default());
        };
        // The shop needs the installed edition to build a patch against it, and
        // ignores it for a base package.
        let current = installed.map(|e| e.to_string()).unwrap_or_default();
        let reply = self.post(protocol::form(&[
            ("taskId", "request"),
            ("username", &session.username),
            ("key", &session.key),
            ("assignedSystemName", system_name),
            ("slotUuid", slot_uuid),
            ("requestedFile", requested_file),
            ("requestedVersion", requested_version),
            ("currentVersion", &current),
            ("version", &protocol::client_version()),
        ]))?;
        Ok(reply.grant.unwrap_or_default())
    }

    /// Fetch a URL the shop handed out, verifying its digest.
    ///
    /// `progress` is called with bytes so far and the expected total, so a
    /// download of a few hundred megabytes over a marina's wifi can show
    /// something moving.
    pub fn download(
        &self,
        url: &str,
        expected_sha256: &str,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<Vec<u8>, Error> {
        use sha2::{Digest, Sha256};

        let header = |resp: &ureq::http::Response<ureq::Body>, name: &str| {
            resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
        };
        let mut out: Vec<u8> = Vec::new();
        let mut total = 0u64;
        // What names this version of the file, so a resumed request cannot
        // be answered with the rest of a different one.
        let mut validator = String::new();
        // The most that has ever arrived: only getting past it is progress,
        // so a server that keeps starting over cannot keep us for ever.
        let mut best = 0usize;
        let mut stalls = 0;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let mut req = self.downloads.get(url);
            if !out.is_empty() {
                req = req.header("Range", &format!("bytes={}-", out.len()));
                if !validator.is_empty() {
                    req = req.header("If-Range", &validator);
                }
            }
            let mut last_error = None;
            match req.call() {
                Err(e) => last_error = Some(e.to_string()),
                Ok(mut response) => {
                    if response.status() != 206 {
                        // The whole file, from the top.
                        out.clear();
                        total = header(&response, "content-length")
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(0);
                        validator = header(&response, "etag")
                            .or_else(|| header(&response, "last-modified"))
                            .unwrap_or_default();
                    }
                    let mut reader = response.body_mut().as_reader();
                    loop {
                        match reader.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => {
                                out.extend_from_slice(&chunk[..n]);
                                progress(out.len() as u64, total);
                            }
                            Err(e) => {
                                last_error = Some(e.to_string());
                                break;
                            }
                        }
                    }
                }
            }
            if total > 0 && out.len() as u64 >= total {
                break;
            }
            if total == 0 && last_error.is_none() {
                // No length given: the end of the body is the end.
                break;
            }
            if out.len() > best {
                best = out.len();
                stalls = 0;
            } else {
                stalls += 1;
                if stalls >= MAX_STALLS {
                    return Err(Error::Transport(
                        last_error.unwrap_or_else(|| "the download stopped short".into()),
                    ));
                }
            }
        }
        let mut hasher = Sha256::new();
        hasher.update(&out);

        // A truncated download that still unpacks is the worst outcome: a chart
        // set with holes in it, and nothing to say so.
        if !expected_sha256.is_empty() {
            let got = format!("{:x}", hasher.finalize());
            if got != expected_sha256.to_ascii_lowercase() {
                return Err(Error::Transport(format!(
                    "downloaded file does not match o-charts' checksum \
                     (expected {expected_sha256}, got {got})"
                )));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            username: "skipper@example.org".into(),
            key: "k".into(),
            system_name: Some("saloon-mac".into()),
        }
    }

    /// Up to date means nothing is asked of the shop at all — no request,
    /// so no slot is spent and no network is needed for the answer.
    #[test]
    fn an_up_to_date_chart_asks_the_shop_nothing() {
        let grant = ShopClient::new()
            .request_download(&session(), "UU-1", "saloon-mac", DownloadTarget::UpToDate, "2025/1-20", None)
            .expect("answered locally");
        assert!(grant.files.is_empty());
    }

    /// Serve `response` verbatim to each of `connections` requests on a
    /// loopback port, and return the URL. Enough HTTP to exercise the
    /// download path without the internet.
    fn serve(response: &'static [u8], connections: usize) -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/pkg.zip", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().take(connections) {
                let Ok(mut stream) = stream else { continue };
                // Read the request head before answering.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => break,
                    }
                }
                let _ = stream.write_all(response);
            }
        });
        url
    }

    /// SHA-256 of "abc", from FIPS 180-2's own test vectors.
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    /// A package whose digest does not match what the shop promised is
    /// refused — the truncated or tampered download that would otherwise
    /// unpack into a chart set with holes in it.
    #[test]
    fn a_download_is_checked_against_the_shops_digest() {
        let ok: &'static [u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc";
        let url = serve(ok, 3);
        let client = ShopClient::new();

        let mut seen = Vec::new();
        let bytes = client
            .download(&url, &ABC_SHA256.to_ascii_uppercase(), |n, total| seen.push((n, total)))
            .expect("the digest matches, whatever its case");
        assert_eq!(bytes, b"abc");
        assert_eq!(seen.last(), Some(&(3, 3)));

        let wrong = ABC_SHA256.replace('b', "c");
        let err = client.download(&url, &wrong, |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");

        // No digest offered: nothing to check against, and the bytes come
        // through as they are.
        assert_eq!(client.download(&url, "", |_, _| {}).unwrap(), b"abc");
    }

    /// A server whose connection drops after every `PIECE` bytes of body:
    /// it promises the whole length and delivers ten bytes of it. Asked to
    /// carry on from where it stopped (a Range request) it does — unless it
    /// is `stubborn`, which ignores the Range and starts from the top every
    /// time. Nothing here depends on timing.
    fn serve_in_pieces(body: &'static [u8], stubborn: bool) -> String {
        use std::io::Write;
        const PIECE: usize = 10;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/pkg.zip", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => break,
                    }
                }
                let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                let from = head
                    .lines()
                    .find_map(|l| l.strip_prefix("range: bytes="))
                    .and_then(|r| r.trim_end_matches('-').trim().parse::<usize>().ok())
                    .filter(|_| !stubborn)
                    .unwrap_or(0);
                let rest = &body[from.min(body.len())..];
                let status = if from > 0 {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {from}-{}/{}\r\n",
                        body.len() - 1,
                        body.len()
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\n".to_string()
                };
                let _ = write!(stream, "{status}Content-Length: {}\r\nConnection: close\r\n\r\n", rest.len());
                let _ = stream.write_all(&rest[..rest.len().min(PIECE)]);
                // Dropped here, short of what was promised.
            }
        });
        url
    }

    const FORTY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCD";

    /// A package whose connection keeps dropping still arrives, whole and
    /// in order: each request carries on from where the last one stopped.
    /// This is what lets a request be bounded in time at all — the limit
    /// that used to cut every download off after twenty seconds.
    #[test]
    fn a_download_that_arrives_in_pieces_is_resumed() {
        let url = serve_in_pieces(FORTY, false);
        let mut seen = Vec::new();
        let bytes = ShopClient::new()
            .download(&url, "", |n, total| seen.push((n, total)))
            .expect("resumed");
        assert_eq!(bytes, FORTY);
        assert_eq!(seen.last().map(|(n, _)| *n), Some(40));
        assert!(seen.windows(2).all(|w| w[1].0 > w[0].0), "progress never goes back: {seen:?}");
    }

    /// A server that answers every resume by starting over never gets past
    /// its first ten bytes: the download gives up instead of going round
    /// for ever.
    #[test]
    fn a_download_that_never_gets_further_gives_up() {
        let url = serve_in_pieces(FORTY, true);
        assert!(ShopClient::new().download(&url, "", |_, _| {}).is_err());
    }

    /// A body cut short of its declared length is an error even when there
    /// is no digest to catch it.
    #[test]
    fn a_download_cut_short_is_an_error() {
        let short: &'static [u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc";
        let url = serve(short, 1);
        assert!(ShopClient::new().download(&url, "", |_, _| {}).is_err());
    }

    #[test]
    fn errors_read_as_sentences() {
        let e = Error::Shop(ShopError {
            code: "6".into(),
            message: String::new(),
        });
        assert!(e.to_string().contains("wrong email or password"));
        let e = Error::Transport("dns failure".into());
        assert!(e.to_string().contains("could not reach o-charts"));
    }
}
