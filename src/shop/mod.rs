//! The o-charts chart store: sign in, see what the account owns, download it.
//!
//! navcore reads o-charts cells, so it should be able to fetch them too rather
//! than sending the user to another program. The account, the entitlements and
//! the machine assignment all live on o-charts' server; this is a client for
//! that server, written from the protocol as observed, sharing no code with the
//! GPL-2.0 plugin it was learned from.
//!
//! What this does **not** do: it does not decrypt anything, and it does not
//! forge entitlement. Cells arrive encrypted, and opening them still needs the
//! machine's own licence and the vendor's `oexserverd` — the same path navcore
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
/// decrypts the cells — navcore already runs it, so the file usually exists
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
            Error::Transport(e) => write!(f, "could not reach the chart shop: {e}"),
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

pub struct ShopClient {
    agent: ureq::Agent,
    /// Chart packages: no whole-call deadline, only one for the reply to
    /// start. See [`ShopClient::new`].
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
        // A second agent for the chart packages. `timeout_global` is a cap on
        // the *whole* call including the body, so the twenty seconds that
        // suit a small XML reply guarantee that a few hundred megabytes over
        // a marina's wifi fails every time. This one bounds the wait for the
        // response to start, and then lets the transfer take as long as the
        // link takes.
        let downloads = ureq::Agent::config_builder()
            .timeout_recv_response(Some(TIMEOUT))
            .build();
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

        let mut response = self
            .downloads
            .get(url)
            .call()
            .map_err(|e| Error::Transport(e.to_string()))?;
        let total = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);

        let mut reader = response.body_mut().as_reader();
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        let mut hasher = Sha256::new();
        loop {
            let n = reader
                .read(&mut chunk)
                .map_err(|e| Error::Transport(e.to_string()))?;
            if n == 0 {
                break;
            }
            hasher.update(&chunk[..n]);
            out.extend_from_slice(&chunk[..n]);
            progress(out.len() as u64, total);
        }

        // A truncated download that still unpacks is the worst outcome: a chart
        // set with holes in it, and nothing to say so.
        if !expected_sha256.is_empty() {
            let got = format!("{:x}", hasher.finalize());
            if got != expected_sha256.to_ascii_lowercase() {
                return Err(Error::Transport(format!(
                    "downloaded file does not match the shop's checksum \
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

    #[test]
    fn a_client_can_be_built_without_touching_the_network() {
        let _ = ShopClient::new();
    }

    #[test]
    fn errors_read_as_sentences() {
        let e = Error::Shop(ShopError {
            code: "6".into(),
            message: String::new(),
        });
        assert!(e.to_string().contains("wrong email or password"));
        let e = Error::Transport("dns failure".into());
        assert!(e.to_string().contains("could not reach the chart shop"));
    }
}
