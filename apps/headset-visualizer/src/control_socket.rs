use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_MESSAGE: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(3);
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(5);
const MAX_SUN_PATH: usize = 103;

#[derive(Debug)]
pub struct ClientError {
    message: String,
    exit_code: i32,
}

impl ClientError {
    fn known(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 1,
        }
    }
    fn unknown(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 3,
        }
    }
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }
}
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ClientError {}

pub struct Server {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // The server uses nonblocking I/O; don't make application shutdown wait on a client.
            let (done_tx, done_rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = thread.join();
                let _ = done_tx.send(());
            });
            let _ = done_rx.recv_timeout(Duration::from_millis(250));
        }
    }
}

pub fn start_server(
    dispatch: impl Fn(Value, SyncSender<Value>) -> Result<(), String> + Send + 'static,
) -> anyhow::Result<Server> {
    start_server_at(socket_path()?, dispatch)
}

fn start_server_at(
    path: PathBuf,
    dispatch: impl Fn(Value, SyncSender<Value>) -> Result<(), String> + Send + 'static,
) -> anyhow::Result<Server> {
    check_path_length(&path)?;
    let dir = path.parent().unwrap();
    if !dir.exists() {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o777 != 0o700
    {
        anyhow::bail!(
            "unsafe control runtime directory {} (must be an owned 0700 directory)",
            dir.display()
        );
    }

    let lock_path = dir.join("control.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(0x100) // macOS O_NOFOLLOW
        .open(&lock_path)?;
    let lock_meta = lock.metadata()?;
    if !lock_meta.is_file() || lock_meta.uid() != effective_uid() {
        anyhow::bail!("unsafe control lock file {}", lock_path.display());
    }
    lock.set_permissions(fs::Permissions::from_mode(0o600))?;
    if unsafe { flock(lock.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
        anyhow::bail!("another vr-space instance already owns the CLI control socket");
    }

    clean_stale_socket(&path)?;
    let listener = UnixListener::bind(&path)?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    let owned_meta = fs::symlink_metadata(&path)?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = thread::Builder::new()
        .name("control-socket".into())
        .spawn(move || {
            let _lock = lock;
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if stream.set_nonblocking(true).is_ok() {
                            serve_one(&mut stream, &thread_stop, &dispatch)
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
                    Err(_) => thread::sleep(POLL),
                }
            }
            if same_inode(&path, &owned_meta) {
                let _ = fs::remove_file(&path);
            }
        })?;
    Ok(Server {
        stop,
        thread: Some(thread),
    })
}

pub fn client(request: Value) -> Result<Value, ClientError> {
    let path = socket_path().map_err(|e| ClientError::known(e.to_string()))?;
    check_path_length(&path).map_err(|e| ClientError::known(e.to_string()))?;
    let mut stream = UnixStream::connect(path).map_err(|e| {
        ClientError::known(format!("cannot connect to vr-space control server: {e}"))
    })?;
    stream
        .set_nonblocking(true)
        .map_err(|e| ClientError::known(e.to_string()))?;
    let mut bytes = serde_json::to_vec(&request).map_err(|e| ClientError::known(e.to_string()))?;
    if bytes.len() >= MAX_MESSAGE {
        return Err(ClientError::known("control request exceeds 64 KiB"));
    }
    bytes.push(b'\n');
    if let Err(e) = write_all_until(&mut stream, &bytes, Instant::now() + IO_TIMEOUT, None) {
        return Err(ClientError::known(format!(
            "control request was not dispatched: {e}"
        )));
    }
    match read_line_until(&mut stream, Instant::now() + IO_TIMEOUT, None) {
        Ok(value) => {
            if value.get("outcome").and_then(Value::as_str) == Some("unknown") {
                Err(ClientError::unknown(
                    value
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("command outcome unknown; check status before retrying")
                        .to_owned(),
                ))
            } else {
                Ok(value)
            }
        }
        Err(e) => Err(ClientError::unknown(format!(
            "command outcome unknown (reply lost or timed out); check status before retrying: {e}"
        ))),
    }
}

fn serve_one(
    stream: &mut UnixStream,
    stop: &AtomicBool,
    dispatch: &dyn Fn(Value, SyncSender<Value>) -> Result<(), String>,
) {
    let deadline = Instant::now() + IO_TIMEOUT;
    let request = match read_line_until(stream, deadline, Some(stop)) {
        Ok(v) => v,
        Err(e) => {
            let _ = write_json(stream, &error_reply(e.to_string(), "not_dispatched"), stop);
            return;
        }
    };
    let (tx, rx) = mpsc::sync_channel(1);
    if let Err(e) = dispatch(request, tx) {
        let _ = write_json(stream, &error_reply(e, "not_dispatched"), stop);
        return;
    }
    match rx.recv_timeout(DISPATCH_TIMEOUT) {
        Ok(reply) => {
            let _ = write_json(stream, &reply, stop);
        }
        Err(_) => {
            let _ = write_json(
                stream,
                &error_reply(
                    "command outcome unknown; check status before retrying".into(),
                    "unknown",
                ),
                stop,
            );
        }
    }
}

fn error_reply(error: String, outcome: &str) -> Value {
    json!({"version": 1, "ok": false, "error": error, "outcome": outcome})
}

fn write_json(stream: &mut UnixStream, value: &Value, stop: &AtomicBool) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() >= MAX_MESSAGE {
        bytes = serde_json::to_vec(&error_reply("reply exceeds 64 KiB".into(), "unknown")).unwrap();
    }
    bytes.push(b'\n');
    write_all_until(stream, &bytes, Instant::now() + IO_TIMEOUT, Some(stop))
}

fn write_all_until(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    deadline: Instant,
    stop: Option<&AtomicBool>,
) -> io::Result<()> {
    while !bytes.is_empty() {
        if stop.is_some_and(|s| s.load(Ordering::Acquire)) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "server stopping",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "socket write timed out",
            ));
        }
        match stream.write(bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "socket closed")),
            Ok(n) => bytes = &bytes[n..],
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                thread::sleep(POLL)
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_line_until(
    stream: &mut UnixStream,
    deadline: Instant,
    stop: Option<&AtomicBool>,
) -> io::Result<Value> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0; 1024];
    loop {
        if stop.is_some_and(|s| s.load(Ordering::Acquire)) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "server stopping",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "socket read timed out",
            ));
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "client disconnected before newline",
                ))
            }
            Ok(n) => {
                let end = chunk[..n]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map(|p| p + 1)
                    .unwrap_or(n);
                if bytes.len() + end > MAX_MESSAGE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "control message exceeds 64 KiB",
                    ));
                }
                bytes.extend_from_slice(&chunk[..end]);
                if chunk[end.saturating_sub(1)] == b'\n' {
                    bytes.pop();
                    return serde_json::from_slice(&bytes)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
                }
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                thread::sleep(POLL)
            }
            Err(e) => return Err(e),
        }
    }
}

fn socket_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join("Library/Caches/vr-space/control.sock"))
}
fn check_path_length(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if path.as_os_str().as_bytes().len() > MAX_SUN_PATH {
        anyhow::bail!(
            "control socket path exceeds macOS Unix-socket limit ({} bytes): {}",
            MAX_SUN_PATH,
            path.display()
        );
    }
    Ok(())
}
fn effective_uid() -> u32 {
    unsafe { geteuid() }
}
fn same_inode(path: &Path, original: &fs::Metadata) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| {
        m.dev() == original.dev() && m.ino() == original.ino() && m.file_type().is_socket()
    })
}
fn clean_stale_socket(path: &Path) -> anyhow::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if !meta.file_type().is_socket() || meta.uid() != effective_uid() {
        anyhow::bail!(
            "refusing to remove unexpected control socket path {}",
            path.display()
        );
    }
    match UnixStream::connect(path) {
        Ok(_) => anyhow::bail!(
            "control socket is active; refusing to unlink {}",
            path.display()
        ),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) => {}
        Err(e) => anyhow::bail!("cannot verify stale control socket {}: {e}", path.display()),
    }
    if same_inode(path, &meta) {
        fs::remove_file(path)?;
    }
    Ok(())
}

use std::os::fd::AsRawFd;
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
    fn geteuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn framing_and_reply_round_trip_and_unknown_mapping() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer.write_all(b"{\"version\":1}\n").unwrap();
        assert_eq!(
            read_line_until(&mut reader, Instant::now() + IO_TIMEOUT, None).unwrap()["version"],
            1
        );
        let unknown = error_reply("lost".into(), "unknown");
        assert_eq!(unknown["outcome"], "unknown");
        let e = ClientError::unknown("lost");
        assert_eq!(e.exit_code(), 3);
        let e = ClientError::known("no server");
        assert_eq!(e.exit_code(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&serde_json::to_vec(&unknown).unwrap()).unwrap(),
            unknown
        );
    }

    #[test]
    fn server_dispatch_and_client_reply() {
        let root = std::env::temp_dir().join(format!(
            "vr-control-test-{}-{}",
            effective_uid(),
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let thread = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let v = read_line_until(&mut s, Instant::now() + IO_TIMEOUT, None).unwrap();
            c.fetch_add(1, Ordering::SeqCst);
            write_json(
                &mut s,
                &json!({"version":1,"ok":true,"got":v}),
                &AtomicBool::new(false),
            )
            .unwrap();
        });
        let mut s = UnixStream::connect(&path).unwrap();
        s.set_nonblocking(true).unwrap();
        write_all_until(&mut s, b"{\"x\":1}\n", Instant::now() + IO_TIMEOUT, None).unwrap();
        assert_eq!(
            read_line_until(&mut s, Instant::now() + IO_TIMEOUT, None).unwrap()["got"]["x"],
            1
        );
        thread.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_and_stalled_input_are_bounded() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let write_thread =
            thread::spawn(move || writer.write_all(&vec![b'x'; MAX_MESSAGE + 1]).unwrap());
        assert_eq!(
            read_line_until(&mut reader, Instant::now() + IO_TIMEOUT, None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        write_thread.join().unwrap();
        let (_writer, mut reader) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let start = Instant::now();
        assert_eq!(
            read_line_until(&mut reader, start + Duration::from_millis(30), None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn exclusive_startup_and_stale_socket_recovery() {
        let root = std::env::temp_dir().join(format!(
            "vr-control-lock-{}-{}",
            effective_uid(),
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("control.sock");
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);
        let handler = |_: Value, tx: SyncSender<Value>| {
            let _ = tx.send(json!({"version":1,"ok":true}));
            Ok(())
        };
        let server = start_server_at(path.clone(), handler).unwrap();
        assert!(path.exists());
        let rejected = start_server_at(path.clone(), |_, _| Ok(()));
        assert!(rejected.is_err());
        assert!(path.exists());
        let stalled = UnixStream::connect(&path).unwrap();
        let started = Instant::now();
        drop(server);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(stalled);
        let deadline = Instant::now() + Duration::from_secs(1);
        while path.exists() && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        assert!(!path.exists());
        let _ = fs::remove_dir_all(root);
    }
}
