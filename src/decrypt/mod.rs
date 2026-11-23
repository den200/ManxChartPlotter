//! Runtime glue between Rust and the proprietary `oexserverd` decoder.
//!
//! The decryption flow mirrors the blueprint in `doc/SENC_RENDER_BLUEPRINT.md`: charts
//! ship as encrypted `.oesu` files, `oexserverd` decrypts them into SENC TLV streams, and
//! the renderer consumes the SENC payload.  This module focuses purely on the first stage.

#[cfg(target_os = "android")]
mod android_id;
mod error;
mod keys;

pub use error::{DecryptError, DecryptResult};
pub use keys::{ChartKey, KeyStore};

use log::{info, warn};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::tempdir;

const PIPE_PATH: &str = "/tmp/OCPN_PIPEX";
const RETURN_PIPE_PREFIX: &str = "/tmp/manx_oex_";
/// Android's `oexserverd` has no FIFOs: it listens on this abstract Unix
/// socket, takes the same 1025-byte request, and answers on the connection.
#[cfg(target_os = "android")]
const ANDROID_SOCKET: &str = "com.opencpn.ocharts_pi";
const CMD_READ_ESENC: u8 = 0;
const CMD_EXIT: u8 = 2;
const CMD_READ_OESU: u8 = 8;
const RETURN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUESTS_BEFORE_RESTART: u32 = 25;

/// High-level controller responsible for keeping `oexserverd` alive and decrypting charts.
///
/// Typical usage:
///
/// ```no_run
/// use manx::{ChartDecryptor, KeyStore};
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mut keys = KeyStore::new();
/// keys.load_keylists_in_dir("charts/oeuSENC-DK-2025-1-20-base-macbook")?;
/// let install_key = keys
///     .lookup("OC-45-D54503")
///     .expect("missing install key");
///
/// let mut decryptor = ChartDecryptor::new("path/to/license.fpr")?;
/// let senc_bytes = decryptor.decrypt_chart(
///     "charts/oeuSENC-DK-2025-1-20-base-macbook/OC-45-D54503.oesu",
///     install_key,
/// )?;
/// # Ok(()) }
/// ```
pub struct ChartDecryptor {
    binary: PathBuf,
    fpr_path: PathBuf,
    process: Option<Child>,
    /// Kills the helper if Manx goes without doing it ([`watch`]).
    watchdog: Option<Child>,
    requests_since_restart: u32,
}

impl ChartDecryptor {
    /// Creates a decryptor, automatically resolving the matching `oexserverd` binary from
    /// the `oeserverd/` directory checked into this repository or from the `OEXSERVERD_BIN`
    /// environment variable.
    pub fn new<P: Into<PathBuf>>(fpr_path: P) -> DecryptResult<Self> {
        let binary = resolve_oeserverd_binary()?;
        Self::with_binary(fpr_path, binary)
    }

    /// Same as [`ChartDecryptor::new`] but allows overriding the binary path explicitly.
    pub fn with_binary<P: Into<PathBuf>, Q: Into<PathBuf>>(
        fpr_path: P,
        binary_path: Q,
    ) -> DecryptResult<Self> {
        let binary_path = binary_path.into();
        if !binary_path.exists() {
            return Err(DecryptError::BinaryNotFound(vec![binary_path]));
        }

        let requested_fpr = fpr_path.into();
        let fpr_path = ensure_fpr_path(&binary_path, requested_fpr)?;

        Ok(Self {
            binary: binary_path,
            fpr_path,
            process: None,
            watchdog: None,
            requests_since_restart: 0,
        })
    }

    /// Decrypts a single chart and returns the SENC TLV payload (starting with record type 1).
    /// The fingerprint file this decryptor resolved to.
    ///
    /// It identifies the machine to o-charts, so the chart shop sends its
    /// contents when asking which cells this computer may download.
    pub fn fpr_path(&self) -> &Path {
        &self.fpr_path
    }

    pub fn decrypt_chart<P: AsRef<Path>>(
        &mut self,
        chart_path: P,
        install_key: &str,
    ) -> DecryptResult<Vec<u8>> {
        self.ensure_running()?;
        let chart_path = chart_path.as_ref();

        if !chart_path.exists() {
            return Err(DecryptError::Protocol(format!(
                "Chart not found: {}",
                chart_path.display()
            )));
        }

        if install_key.trim().is_empty() {
            let label = chart_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| chart_path.to_string_lossy().into_owned());
            return Err(DecryptError::MissingKey(label));
        }

        let raw = self.request_raw(chart_path, install_key)?;
        check_server_status(&raw)?;
        let start = find_senc_start(&raw)?;
        let senc = raw[start..].to_vec();

        if senc.len() < 14 {
            return Err(DecryptError::Protocol(
                "Response only contained version TLV (no chart data)".into(),
            ));
        }

        self.requests_since_restart += 1;
        if self.requests_since_restart >= MAX_REQUESTS_BEFORE_RESTART {
            self.restart()?;
        }

        Ok(senc)
    }

    /// Forces a restart of the helper daemon. This is exposed because older versions of
    /// `oexserverd` become unstable after ~25 decryptions in one session.
    pub fn restart(&mut self) -> DecryptResult<()> {
        self.stop();

        if Path::new(PIPE_PATH).exists() {
            let _ = fs::remove_file(PIPE_PATH);
        }

        self.process = None;
        self.requests_since_restart = 0;
        self.ensure_running()
    }

    fn ensure_running(&mut self) -> DecryptResult<()> {
        if self.process.is_some() && daemon_ready() {
            return Ok(());
        }

        self.spawn_process()
    }

    fn spawn_process(&mut self) -> DecryptResult<()> {
        info!("Starting oexserverd from {}", self.binary.display());

        if Path::new(PIPE_PATH).exists() {
            let _ = fs::remove_file(PIPE_PATH);
        }

        let mut command = build_spawn_command(&self.binary, &self.fpr_path)?;

        // Nothing of the helper's is piped back. Its output used to be,
        // and was never read; when Manx then went away without stopping it
        // the pipe broke, and the helper's next signal sent it into a loop
        // — its exit handler flushes stdout, the write raises SIGPIPE, and
        // the handler runs again — at a full core until someone killed it
        // (seen 2026-11-23, hours at 95 %). /dev/null cannot break.
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());

        let mut child = command
            .spawn()
            .map_err(|err| DecryptError::Process(format!("Failed to start oexserverd: {}", err)))?;

        if let Err(e) = wait_for_pipe(&mut child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        // Drop stops the helper, but Drop does not run when Manx is killed,
        // crashes, or leaves through `process::exit`.
        self.watchdog = watch(std::process::id(), child.id())
            .map_err(|e| warn!("no watchdog for oexserverd: {e}"))
            .ok();
        self.process = Some(child);
        self.requests_since_restart = 0;
        Ok(())
    }

    fn request_raw(&mut self, chart_path: &Path, install_key: &str) -> DecryptResult<Vec<u8>> {
        let cmd = if chart_path
            .extension()
            .map(|ext| ext.eq_ignore_ascii_case("oesu"))
            .unwrap_or(false)
        {
            CMD_READ_OESU
        } else {
            CMD_READ_ESENC
        };

        #[cfg(target_os = "android")]
        {
            let message = OexMessage::new(cmd, Path::new(""), chart_path, install_key)?;
            return android_request(&message);
        }
        #[allow(unreachable_code)]
        let return_pipe = create_return_pipe()?;
        let message = OexMessage::new(cmd, &return_pipe, chart_path, install_key)?;
        write_message(&message)?;
        let data = read_return_pipe(&return_pipe)?;
        fs::remove_file(&return_pipe).ok();
        Ok(data)
    }
}

impl ChartDecryptor {
    /// Stop the helper and wait for it to be gone.
    fn stop(&mut self) {
        // The watchdog first, so it cannot outlive what it watches and
        // later kill a stranger that was given the helper's process number.
        if let Some(mut dog) = self.watchdog.take() {
            let _ = dog.kill();
            let _ = dog.wait();
        }
        if let Some(mut child) = self.process.take() {
            send_exit_command().ok();
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChartDecryptor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A watchdog for a helper process: when `parent` is gone, `child` is
/// killed. It ends by itself once either is gone.
///
/// The helper does not notice Manx leaving, and an orphan holds the
/// decryption pipe and — worse — could spin (see `spawn_process`). There is
/// no portable "die with my parent", so a shell looks once a second.
fn watch(parent: u32, child: u32) -> std::io::Result<Child> {
    Command::new("/bin/sh")
        .args([
            "-c",
            "while kill -0 \"$0\" 2>/dev/null && kill -0 \"$1\" 2>/dev/null; do sleep 1; done; \
             kill -9 \"$1\" 2>/dev/null",
        ])
        .arg(parent.to_string())
        .arg(child.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn ensure_fpr_path(binary: &Path, requested: PathBuf) -> DecryptResult<PathBuf> {
    if requested.exists() {
        if requested.is_file() {
            return Ok(requested);
        } else if requested.is_dir() {
            if let Some(existing) = find_latest_fpr(&requested)? {
                return Ok(existing);
            }
            return generate_fpr_into(binary, &requested);
        }
    }

    if looks_like_directory(&requested) {
        fs::create_dir_all(&requested)?;
        return generate_fpr_into(binary, &requested);
    }

    let parent = requested.parent().map(|p| {
        if p.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            p.to_path_buf()
        }
    });

    let parent = parent.ok_or_else(|| DecryptError::MissingFpr(requested.clone()))?;

    fs::create_dir_all(&parent)?;
    let generated = generate_fpr_into(binary, &parent)?;
    if generated == requested {
        return Ok(generated);
    }
    move_or_copy(&generated, &requested)?;
    Ok(requested)
}

fn looks_like_directory(path: &Path) -> bool {
    path.extension().is_none()
}

fn generate_fpr_into(binary: &Path, destination: &Path) -> DecryptResult<PathBuf> {
    fs::create_dir_all(destination)?;
    if cfg!(target_os = "android") {
        return generate_android_fpr_into(binary, destination);
    }
    let temp_dir = tempdir()?;
    let temp_path = temp_dir.path();

    // The trailing separator is load-bearing. `oexserverd -g` joins its target
    // to the filename it invents by plain string concatenation, so a target of
    // `/tmp/.tmpAbC` yields `/tmp/.tmpAbCoc03D_1785145257.fpr` — the directory's
    // own name glued onto the front, and the file written *beside* the
    // directory rather than inside it. The name then travels to the shop as
    // `xfprName`. The reference client forces the separator for the same
    // reason.
    let mut target = temp_path.to_string_lossy().into_owned();
    if !target.ends_with(std::path::MAIN_SEPARATOR) {
        target.push(std::path::MAIN_SEPARATOR);
    }

    let args = vec!["-g".to_string(), target];
    let output = run_oex_cli(binary, &args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DecryptError::Process(format!(
            "Fingerprint generation failed: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut source = parse_generated_fpr_path(&stdout);
    if source.is_none() {
        source = find_latest_fpr(temp_path)?;
    }

    let source = source.ok_or_else(|| {
        DecryptError::Process("oexserverd did not report generated FPR path".into())
    })?;

    let file_name = source
        .file_name()
        .ok_or_else(|| DecryptError::Process("Generated FPR path missing filename".into()))?;

    let destination_path = destination.join(file_name);
    move_or_copy(&source, &destination_path)?;
    Ok(destination_path)
}

/// Android's `oexserverd` writes no file: it prints `timestamp;HEX`, the hex
/// being the same bytes a desktop `.fpr` holds. Given a Widevine ID (`-y`) it
/// is asked with `-k` and the file is named `oc04R_<timestamp>.fpr`; given
/// another ID (`-z`), with `-g` and `oc03R_` — as o-charts' own Android
/// clients name the two kinds.
///
/// Without an ID there is no fingerprint: the helper would make one with an
/// empty identity, and registering that would spend a licence slot on
/// nothing that identifies this device.
fn generate_android_fpr_into(binary: &Path, destination: &Path) -> DecryptResult<PathBuf> {
    let (flag, id) = android_device_id()?;
    let (action, prefix) = if flag == "-y" { ("-k", "oc04R_") } else { ("-g", "oc03R_") };
    let output = run_oex_cli(binary, &[flag.to_string(), id.to_string(), action.to_string()])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (stamp, bytes) = parse_android_fpr(&stdout).ok_or_else(|| {
        DecryptError::Process(format!(
            "Fingerprint generation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    })?;
    let path = destination.join(format!("{prefix}{stamp}.fpr"));
    fs::write(&path, bytes)?;
    Ok(path)
}

/// The flag and value that identify this device to the Android helper.
fn android_device_id() -> DecryptResult<(&'static str, &'static str)> {
    #[cfg(target_os = "android")]
    return android_id::device_id()
        .map(|id| (id.flag(), id.value()))
        .ok_or_else(|| DecryptError::Process("no device ID to give o-charts".into()));
    #[allow(unreachable_code)]
    Err(DecryptError::Process("no Android device ID off Android".into()))
}

fn parse_android_fpr(stdout: &str) -> Option<(&str, Vec<u8>)> {
    let line = stdout.lines().map(str::trim).find(|l| l.contains(';'))?;
    let (stamp, hex) = line.split_once(';')?;
    if stamp.is_empty() || !stamp.bytes().all(|b| b.is_ascii_digit()) || hex.len() % 2 != 0 {
        return None;
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    (!bytes.is_empty()).then_some((stamp, bytes))
}

fn parse_generated_fpr_path(stdout: &str) -> Option<PathBuf> {
    for line in stdout.lines() {
        if line.contains("fpr file") {
            if let Some(idx) = line.find(':') {
                let path = line[idx + 1..].trim();
                if !path.is_empty() {
                    return Some(PathBuf::from(path));
                }
            }
        }
    }
    None
}

fn find_latest_fpr(dir: &Path) -> DecryptResult<Option<PathBuf>> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        if !matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some(ext) if ext.eq_ignore_ascii_case("fpr")
        ) {
            continue;
        }

        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::now());
        match &newest {
            Some((existing_time, _)) if existing_time >= &modified => {}
            _ => newest = Some((modified, path)),
        }
    }
    Ok(newest.map(|(_, path)| path))
}

fn move_or_copy(src: &Path, dst: &Path) -> DecryptResult<()> {
    match fs::rename(src, dst) {
        Ok(_) => Ok(()),
        Err(_) => {
            fs::copy(src, dst)?;
            fs::remove_file(src).ok();
            Ok(())
        }
    }
}

fn run_oex_cli(binary: &Path, args: &[String]) -> DecryptResult<Output> {
    let mut command = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        let mut cmd = Command::new("arch");
        cmd.arg("-x86_64");
        cmd.arg(binary);
        cmd
    } else {
        Command::new(binary)
    };

    for arg in args {
        command.arg(arg);
    }

    if let Some(dir) = binary.parent() {
        #[cfg(target_os = "macos")]
        {
            command.env("DYLD_LIBRARY_PATH", dir);
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            command.env("LD_LIBRARY_PATH", dir);
        }
    }

    command.output().map_err(|err| {
        DecryptError::Process(format!("Failed to execute {}: {}", binary.display(), err))
    })
}

struct OexMessage {
    data: [u8; 1025],
}

impl OexMessage {
    fn new(
        cmd: u8,
        return_pipe: &Path,
        chart_path: &Path,
        install_key: &str,
    ) -> DecryptResult<Self> {
        let mut data = [0u8; 1025];
        data[0] = cmd;
        copy_str_into(&mut data[1..257], return_pipe)?;
        copy_str_into(&mut data[257..513], chart_path)?;
        copy_install_key(&mut data[513..1025], install_key)?;
        Ok(Self { data })
    }
}

fn copy_str_into(target: &mut [u8], value: &Path) -> DecryptResult<()> {
    let text = value.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() >= target.len() {
        return Err(DecryptError::Protocol(format!(
            "String too long for oexserverd message: {}",
            text
        )));
    }
    target[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

fn copy_install_key(target: &mut [u8], install_key: &str) -> DecryptResult<()> {
    let trimmed = install_key.trim();
    if trimmed.len() >= target.len() {
        return Err(DecryptError::Protocol(
            "Install key exceeds 512 bytes".into(),
        ));
    }
    target[..trimmed.len()].copy_from_slice(trimmed.as_bytes());
    Ok(())
}

fn write_message(message: &OexMessage) -> DecryptResult<()> {
    let mut pipe = OpenOptions::new().write(true).open(PIPE_PATH)?;
    pipe.write_all(&message.data)?;
    pipe.flush()?;
    Ok(())
}

fn read_return_pipe(path: &Path) -> DecryptResult<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let path = path.to_path_buf();
    thread::spawn(move || {
        let result = OpenOptions::new()
            .read(true)
            .open(&path)
            .and_then(|mut file| {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)?;
                Ok(buf)
            });
        let _ = tx.send(result);
    });

    match rx.recv_timeout(RETURN_TIMEOUT) {
        Ok(res) => res.map_err(DecryptError::from),
        Err(_) => Err(DecryptError::Timeout {
            operation: "oexserverd response",
        }),
    }
}

fn create_return_pipe() -> DecryptResult<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| DecryptError::Protocol(e.to_string()))?
        .as_micros();
    let path = format!("{RETURN_PIPE_PREFIX}{ts}");

    let status = Command::new("mkfifo").arg(&path).status()?;

    if !status.success() {
        return Err(DecryptError::Process(format!(
            "mkfifo failed with status {:?}",
            status.code()
        )));
    }

    Ok(PathBuf::from(path))
}

fn wait_for_pipe(child: &mut Child) -> DecryptResult<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(5);

    while start.elapsed() < timeout {
        if daemon_ready() {
            return Ok(());
        }

        if let Some(status) = child.try_wait()? {
            return Err(DecryptError::Process(format!(
                "oexserverd exited early: {}",
                format_exit(status)
            )));
        }

        thread::sleep(Duration::from_millis(100));
    }

    Err(DecryptError::Timeout {
        operation: "oexserverd pipe",
    })
}

/// Whether the daemon is taking requests: its FIFO exists, or on Android its
/// socket answers.
fn daemon_ready() -> bool {
    #[cfg(target_os = "android")]
    return android_connect().is_ok();
    #[allow(unreachable_code)]
    Path::new(PIPE_PATH).exists()
}

#[cfg(target_os = "android")]
fn android_connect() -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::android::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(ANDROID_SOCKET)?;
    std::os::unix::net::UnixStream::connect_addr(&addr)
}

/// One request over the socket: the message out, and the reply read until
/// the daemon closes the connection.
#[cfg(target_os = "android")]
fn android_request(message: &OexMessage) -> DecryptResult<Vec<u8>> {
    let mut stream = android_connect()?;
    stream.set_read_timeout(Some(RETURN_TIMEOUT))?;
    stream.write_all(&message.data)?;
    stream.flush()?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).map_err(|e| match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => DecryptError::Timeout {
            operation: "oexserverd response",
        },
        _ => DecryptError::from(e),
    })?;
    Ok(reply)
}

fn send_exit_command() -> DecryptResult<()> {
    #[cfg(target_os = "android")]
    {
        let mut msg = [0u8; 1025];
        msg[0] = CMD_EXIT;
        if let Ok(mut stream) = android_connect() {
            stream.write_all(&msg)?;
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    if !Path::new(PIPE_PATH).exists() {
        return Ok(());
    }

    let mut msg = [0u8; 1025];
    msg[0] = CMD_EXIT;
    let mut pipe = OpenOptions::new().write(true).open(PIPE_PATH)?;
    pipe.write_all(&msg)?;
    Ok(())
}

fn check_server_status(buf: &[u8]) -> DecryptResult<()> {
    if buf.len() < 6 {
        return Err(DecryptError::Protocol(
            "oexserverd returned an empty response".into(),
        ));
    }

    let ty = u16::from_le_bytes([buf[0], buf[1]]);
    let len = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]) as usize;
    if ty != 200 || len < 18 || len > buf.len() {
        return Ok(());
    }

    let payload = &buf[6..len];
    if payload.len() < 6 {
        return Ok(());
    }

    let decrypt_status = u16::from_le_bytes([payload[2], payload[3]]);
    let expire_status = u16::from_le_bytes([payload[4], payload[5]]);

    if decrypt_status != 0 && decrypt_status != 64 {
        return Err(DecryptError::Protocol(format!(
            "oexserverd reported decrypt_status={} expire_status={}",
            decrypt_status, expire_status
        )));
    }

    if decrypt_status == 64 {
        warn!("oexserverd reported status=64 (non-fatal warning)");
    }

    Ok(())
}

fn find_senc_start(buf: &[u8]) -> DecryptResult<usize> {
    let mut offset = 0usize;
    while offset + 6 <= buf.len() {
        let ty = u16::from_le_bytes([buf[offset], buf[offset + 1]]);
        let len = u32::from_le_bytes([
            buf[offset + 2],
            buf[offset + 3],
            buf[offset + 4],
            buf[offset + 5],
        ]) as usize;

        if len < 6 || offset + len > buf.len() {
            return Err(DecryptError::Protocol(format!(
                "Invalid TLV length {} at offset {}",
                len, offset
            )));
        }

        if ty == 1 && len == 8 {
            return Ok(offset);
        }

        offset += len;
    }

    Err(DecryptError::Protocol(
        "SENC header (type=1,len=8) not found in response".into(),
    ))
}

fn resolve_oeserverd_binary() -> DecryptResult<PathBuf> {
    if let Ok(path) = std::env::var("OEXSERVERD_BIN") {
        let expanded = PathBuf::from(path);
        if expanded.exists() {
            return Ok(expanded);
        }
    }

    let mut candidates = Vec::new();
    let exe_name = if cfg!(target_os = "windows") {
        "oexserverd.exe"
    } else {
        "oexserverd"
    };

    if let Some(manifest_dir) = option_env!("CARGO_MANIFEST_DIR").map(PathBuf::from) {
        let base = manifest_dir.join("oeserverd");
        let specific = base.join(platform_subdir()).join(exe_name);
        candidates.push(specific);
        candidates.push(base.join(exe_name));
    }

    if let Ok(cwd) = std::env::current_dir() {
        let base = cwd.join("oeserverd");
        candidates.push(base.join(platform_subdir()).join(exe_name));
        candidates.push(base.join(exe_name));
    }

    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home).join(".manx/decoder").join(exe_name));
    }

    candidates.push(PathBuf::from("/usr/local/bin").join(exe_name));
    candidates.push(PathBuf::from("/usr/bin").join(exe_name));

    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    Err(DecryptError::BinaryNotFound(candidates))
}

fn platform_subdir() -> &'static str {
    if cfg!(target_os = "macos") {
        "osx"
    } else if cfg!(target_os = "windows") {
        "win"
    } else if cfg!(target_arch = "aarch64") {
        "linuxarm64"
    } else if cfg!(target_arch = "arm") {
        "linuxarm"
    } else {
        "linux64"
    }
}

fn build_spawn_command(binary: &Path, fpr_path: &Path) -> DecryptResult<Command> {
    let mut command = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        let mut cmd = Command::new("arch");
        cmd.args(["-x86_64", binary.to_str().unwrap_or_default()]);
        cmd
    } else {
        Command::new(binary)
    };

    // Android's daemon takes no fingerprint file: it is given the same
    // device ID the fingerprint was made from. The desktop ones are told
    // which fingerprint to use.
    if cfg!(target_os = "android") {
        let (flag, id) = android_device_id()?;
        command.args([flag, id]);
    } else {
        command.args(["-d", "-f"]);
        command.arg(fpr_path);
    }

    if let Some(dir) = binary.parent() {
        #[cfg(target_os = "macos")]
        {
            command.env("DYLD_LIBRARY_PATH", dir);
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            command.env("LD_LIBRARY_PATH", dir);
        }
    }

    command.env("ENV_AVNAV_PID", std::process::id().to_string());
    Ok(command)
}

fn format_exit(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {}", code),
        None => "terminated by signal".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(pid: u32) -> bool {
        Command::new("/bin/sh")
            .args(["-c", "kill -0 \"$0\" 2>/dev/null"])
            .arg(pid.to_string())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// The helper must not outlive Manx: when the watched parent is gone,
    /// the watchdog kills the child and ends. Stand-ins: two `sleep`s.
    #[cfg(unix)]
    #[test]
    fn the_watchdog_kills_the_helper_when_its_parent_is_gone() {
        let mut parent = Command::new("sleep").arg("60").spawn().unwrap();
        let mut helper = Command::new("sleep").arg("60").spawn().unwrap();
        let mut dog = watch(parent.id(), helper.id()).unwrap();
        // With both alive it leaves the helper alone.
        std::thread::sleep(Duration::from_millis(1500));
        assert!(helper.try_wait().unwrap().is_none(), "killed while its parent lived");

        parent.kill().unwrap();
        parent.wait().unwrap();
        let gone = helper.wait().unwrap();
        assert!(!gone.success(), "the helper was killed, it did not finish");
        assert!(dog.wait().unwrap().code().is_some(), "the watchdog ends by itself");
    }

    /// And a watchdog does not outlive the helper it watches: it would sit
    /// there holding a process number that may later belong to someone else.
    #[cfg(unix)]
    #[test]
    fn the_watchdog_ends_when_the_helper_is_gone() {
        let mut parent = Command::new("sleep").arg("60").spawn().unwrap();
        let mut helper = Command::new("sleep").arg("60").spawn().unwrap();
        let mut dog = watch(parent.id(), helper.id()).unwrap();
        helper.kill().unwrap();
        helper.wait().unwrap();
        dog.wait().unwrap();
        assert!(alive(parent.id()), "the parent is not touched");
        parent.kill().unwrap();
        parent.wait().unwrap();
    }


    #[test]
    fn reads_the_android_fingerprint_line() {
        let (stamp, bytes) = parse_android_fpr("1790334623;0D626C79\n").unwrap();
        assert_eq!(stamp, "1790334623");
        assert_eq!(bytes, vec![0x0D, 0x62, 0x6C, 0x79]);
        assert!(parse_android_fpr("no fingerprint here").is_none());
        assert!(parse_android_fpr("1790334623;0D6").is_none());
        assert!(parse_android_fpr("abc;0D62").is_none());
    }

    #[test]
    fn detects_missing_senc_header() {
        let data = vec![0u8; 10];
        let err = find_senc_start(&data).unwrap_err();
        assert!(matches!(err, DecryptError::Protocol(_)));
    }

    fn tlv(out: &mut Vec<u8>, kind: u16, payload: &[u8]) {
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&((payload.len() + 6) as u32).to_le_bytes());
        out.extend_from_slice(payload);
    }

    /// The helper's reply can carry records before the SENC proper; the
    /// SENC starts at the version record (type 1, length 8), found by
    /// walking the records — not by searching for the bytes, which can turn
    /// up inside a payload.
    #[test]
    fn finds_the_senc_after_leading_records() {
        let mut reply = Vec::new();
        tlv(&mut reply, 1, &[0, 0, 0]); // type 1 but not 8 long
        tlv(&mut reply, 9, &[1, 0, 8, 0, 0, 0, 201, 0]); // the bytes, inside a payload
        let at = reply.len();
        tlv(&mut reply, 1, &201u16.to_le_bytes());
        tlv(&mut reply, 2, b"CELL\0");
        assert_eq!(find_senc_start(&reply).unwrap(), at);

        let mut header_first = Vec::new();
        tlv(&mut header_first, 1, &200u16.to_le_bytes());
        assert_eq!(find_senc_start(&header_first).unwrap(), 0);

        // A record running past the end of the reply is a protocol error.
        let cut = &reply[..at + 4];
        assert!(matches!(find_senc_start(cut), Err(DecryptError::Protocol(_))));
    }

    #[test]
    fn reads_the_generated_fingerprint_path() {
        let out = "Starting\nCreated fpr file: /home/pi/.manx/oc03R_1790334623.fpr\nDone";
        assert_eq!(
            parse_generated_fpr_path(out),
            Some(PathBuf::from("/home/pi/.manx/oc03R_1790334623.fpr"))
        );
        assert_eq!(parse_generated_fpr_path("fpr file:   \n"), None);
        assert_eq!(parse_generated_fpr_path("nothing to see"), None);
    }
}
