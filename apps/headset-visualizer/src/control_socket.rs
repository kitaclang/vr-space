use serde_json::{json, Value};
use std::os::fd::AsRawFd;
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
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
#[cfg(not(test))]
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const DISPATCH_TIMEOUT: Duration = Duration::from_millis(80);
const POLL: Duration = Duration::from_millis(5);
const SHUTDOWN_REPLY_GRACE: Duration = Duration::from_millis(200);
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
    prepare_runtime_dir(dir, true)?;

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
    let cleanup = SocketCleanup::new(path.clone())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = thread::Builder::new()
        .name("control-socket".into())
        .spawn(move || {
            let _cleanup = cleanup;
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
        })?;
    Ok(Server {
        stop,
        thread: Some(thread),
    })
}

pub fn client(request: Value) -> Result<Value, ClientError> {
    let path = socket_path().map_err(|e| ClientError::known(e.to_string()))?;
    check_path_length(&path).map_err(|e| ClientError::known(e.to_string()))?;
    prepare_runtime_dir(path.parent().unwrap(), false)
        .map_err(|e| ClientError::known(e.to_string()))?;
    verify_socket(&path).map_err(|e| ClientError::known(e.to_string()))?;
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
    let deadline = Instant::now() + DISPATCH_TIMEOUT;
    let mut shutdown_deadline = None;
    let mut reported_unknown = false;
    loop {
        let stopping = stop.load(Ordering::Acquire);
        if stopping {
            shutdown_deadline.get_or_insert_with(|| Instant::now() + SHUTDOWN_REPLY_GRACE);
        }
        match rx.try_recv() {
            Ok(reply) if !reported_unknown && Instant::now() < deadline => {
                let _ = write_ready_reply(stream, &reply, stop);
                return;
            }
            Ok(_) if !reported_unknown => {
                if !stopping {
                    let _ = write_json(
                        stream,
                        &error_reply(
                            "command outcome unknown; check status before retrying".into(),
                            "unknown",
                        ),
                        stop,
                    );
                }
                reported_unknown = true;
            }
            Ok(_) | Err(mpsc::TryRecvError::Disconnected) => return,
            Err(mpsc::TryRecvError::Empty)
                if stopping && Instant::now() >= shutdown_deadline.unwrap() =>
            {
                return
            }
            Err(mpsc::TryRecvError::Empty) if !reported_unknown && Instant::now() >= deadline => {
                let _ = write_json(
                    stream,
                    &error_reply(
                        "command outcome unknown; check status before retrying".into(),
                        "unknown",
                    ),
                    stop,
                );
                reported_unknown = true;
            }
            Err(mpsc::TryRecvError::Empty) => thread::sleep(POLL),
        }
    }
}

fn error_reply(error: String, outcome: &str) -> Value {
    json!({"version": 1, "ok": false, "error": error, "outcome": outcome})
}

fn write_json(stream: &mut UnixStream, value: &Value, stop: &AtomicBool) -> io::Result<()> {
    write_json_until(stream, value, Instant::now() + IO_TIMEOUT, Some(stop))
}

fn write_json_until(
    stream: &mut UnixStream,
    value: &Value,
    deadline: Instant,
    stop: Option<&AtomicBool>,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() >= MAX_MESSAGE {
        bytes = serde_json::to_vec(&error_reply("reply exceeds 64 KiB".into(), "unknown")).unwrap();
    }
    bytes.push(b'\n');
    write_all_until(stream, &bytes, deadline, stop)
}

fn write_ready_reply(stream: &mut UnixStream, value: &Value, stop: &AtomicBool) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() >= MAX_MESSAGE {
        bytes = serde_json::to_vec(&error_reply("reply exceeds 64 KiB".into(), "unknown")).unwrap();
    }
    bytes.push(b'\n');
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut shutdown_deadline = None;
    let mut written = 0;
    while written < bytes.len() {
        if stop.load(Ordering::Acquire) {
            shutdown_deadline.get_or_insert_with(|| Instant::now() + SHUTDOWN_REPLY_GRACE);
        }
        if shutdown_deadline.is_some_and(|d| Instant::now() >= d)
            || (shutdown_deadline.is_none() && Instant::now() >= deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "reply write timed out",
            ));
        }
        match stream.write(&bytes[written..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "socket closed")),
            Ok(n) => written += n,
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

fn prepare_runtime_dir(dir: &Path, create: bool) -> anyhow::Result<()> {
    let caches = dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid control runtime path"))?;
    let home = caches
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow::anyhow!("invalid control runtime path"))?;
    for ancestor in [home, &home.join("Library"), caches] {
        let meta = fs::symlink_metadata(ancestor)?;
        if !meta.is_dir() || meta.file_type().is_symlink() || meta.uid() != effective_uid() {
            anyhow::bail!("unsafe control runtime ancestor {}", ancestor.display());
        }
    }
    if create {
        match fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != effective_uid()
        || meta.mode() & 0o777 != 0o700
    {
        anyhow::bail!(
            "unsafe control runtime directory {} (must be an owned 0700 directory)",
            dir.display()
        );
    }
    Ok(())
}

struct SocketCleanup {
    path: PathBuf,
    metadata: fs::Metadata,
}
impl SocketCleanup {
    fn new(path: PathBuf) -> anyhow::Result<Self> {
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_socket() || metadata.uid() != effective_uid() {
            anyhow::bail!("refusing to own unexpected socket path {}", path.display());
        }
        Ok(Self { path, metadata })
    }
}
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if same_inode(&self.path, &self.metadata) {
            let _ = fs::remove_file(&self.path);
        }
    }
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
fn verify_socket(path: &Path) -> anyhow::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_socket()
        || meta.uid() != effective_uid()
        || meta.mode() & 0o777 != 0o600
    {
        anyhow::bail!(
            "unsafe control socket {} (must be an owned 0600 socket)",
            path.display()
        );
    }
    Ok(())
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
        let (root, path) = test_runtime("lock");
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

    #[test]
    fn start_server_at_accepts_and_replies_to_a_real_request() {
        let (root, path) = test_runtime("request-reply");
        let server = start_server_at(path.clone(), |request, tx| {
            tx.send(json!({"version":1,"ok":true,"echo":request["value"]}))
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        client.set_nonblocking(true).unwrap();
        client.write_all(b"{\"value\":42}\n").unwrap();
        let reply =
            read_line_until(&mut client, Instant::now() + Duration::from_secs(1), None).unwrap();
        assert_eq!(reply["echo"], 42);
        drop(server);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn client_socket_validation_rejects_wrong_types_owner_mode_and_symlinks() {
        let (root, path) = test_runtime("socket-check");
        fs::write(&path, b"file").unwrap();
        assert!(verify_socket(&path).is_err());
        fs::remove_file(&path).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        verify_socket(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(verify_socket(&path).is_err());
        let alias = path.parent().unwrap().join("alias.sock");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(verify_socket(&alias).is_err());
        drop(listener);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ready_reply_is_flushed_when_server_stops_after_event_loop_ack() {
        let (root, path) = test_runtime("quit-ack");
        let (senders, replies) = mpsc::channel();
        let server = start_server_at(path.clone(), move |_, tx| {
            senders.send(tx).unwrap();
            Ok(())
        })
        .unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        client.set_nonblocking(true).unwrap();
        client.write_all(b"{}\n").unwrap();
        let reply = replies.recv_timeout(Duration::from_secs(1)).unwrap();
        reply
            .send(json!({"version":1,"ok":true,"quit":"accepted"}))
            .unwrap();
        drop(server);
        let ack =
            read_line_until(&mut client, Instant::now() + Duration::from_secs(1), None).unwrap();
        assert_eq!(ack["quit"], "accepted");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stalled_reply_write_observes_shutdown_grace() {
        let (mut writer, _reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = stop.clone();
        let task = thread::spawn(move || {
            write_ready_reply(
                &mut writer,
                &json!({"data":"x".repeat(MAX_MESSAGE - 128)}),
                &writer_stop,
            )
        });
        thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        stop.store(true, Ordering::Release);
        assert!(task.join().unwrap().is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn partial_startup_cleanup_is_inode_guarded() {
        let (root, path) = test_runtime("cleanup");
        let listener = UnixListener::bind(&path).unwrap();
        let cleanup = SocketCleanup::new(path.clone()).unwrap();
        drop(listener);
        drop(cleanup);
        assert!(!path.exists());

        let listener = UnixListener::bind(&path).unwrap();
        let cleanup = SocketCleanup::new(path.clone()).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
        drop(listener);
        drop(cleanup);
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsafe_runtime_paths_are_rejected_without_touching_socket() {
        let (home, path) = test_runtime("unsafe");
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(start_server_at(path.clone(), |_, _| Ok(())).is_err());
        assert!(!path.exists());
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir(path.parent().unwrap()).unwrap();
        let target = home.join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, path.parent().unwrap()).unwrap();
        assert!(start_server_at(path.clone(), |_, _| Ok(())).is_err());
        fs::remove_file(path.parent().unwrap()).unwrap();
        fs::create_dir(path.parent().unwrap()).unwrap();
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&path, b"not a socket").unwrap();
        assert!(start_server_at(path.clone(), |_, _| Ok(())).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"not a socket");
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn timed_out_request_blocks_later_dispatch_until_sender_finishes() {
        let (root, path) = test_runtime("bounded");
        let (txs, rxs) = mpsc::channel();
        let dispatched = Arc::new(AtomicUsize::new(0));
        let count = dispatched.clone();
        let server = start_server_at(path.clone(), move |_, tx| {
            count.fetch_add(1, Ordering::SeqCst);
            txs.send(tx).unwrap();
            Ok(())
        })
        .unwrap();
        let mut first = UnixStream::connect(&path).unwrap();
        first.set_nonblocking(true).unwrap();
        first.write_all(b"{}\n").unwrap();
        assert_eq!(
            read_line_until(&mut first, Instant::now() + Duration::from_secs(1), None).unwrap()
                ["outcome"],
            "unknown"
        );
        let first_reply = rxs.recv().unwrap();
        let mut second = UnixStream::connect(&path).unwrap();
        second.set_nonblocking(true).unwrap();
        second.write_all(b"{}\n").unwrap();
        thread::sleep(Duration::from_millis(30));
        assert_eq!(dispatched.load(Ordering::SeqCst), 1);
        first_reply.send(json!({"late":true})).unwrap();
        let second_reply = rxs.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(dispatched.load(Ordering::SeqCst), 2);
        second_reply.send(json!({"ok":true})).unwrap();
        drop(server);
        drop((first, second));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn disconnected_client_does_not_panic_when_reply_is_sent() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        client.write_all(b"{}\n").unwrap();
        let (saved_tx, saved_rx) = mpsc::channel();
        let task = thread::spawn(move || {
            serve_one(&mut server, &AtomicBool::new(false), &move |_, tx| {
                saved_tx.send(tx).unwrap();
                Ok(())
            });
        });
        let delayed = saved_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(client);
        delayed.send(json!({"version":1,"ok":true})).unwrap();
        task.join().unwrap();
    }

    #[test]
    fn late_reply_is_reported_unknown_and_receiver_stays_live_until_completion() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        client.set_nonblocking(true).unwrap();
        client.write_all(b"{}\n").unwrap();
        let (saved_tx, saved_rx) = mpsc::channel();
        let task = thread::spawn(move || {
            serve_one(&mut server, &AtomicBool::new(false), &move |_, tx| {
                saved_tx.send(tx).unwrap();
                Ok(())
            });
        });
        let reply =
            read_line_until(&mut client, Instant::now() + Duration::from_secs(1), None).unwrap();
        assert_eq!(reply["outcome"], "unknown");
        let delayed = saved_rx.recv().unwrap();
        assert!(!task.is_finished());
        delayed.send(json!({"version":1,"ok":true})).unwrap();
        task.join().unwrap();
    }

    fn test_runtime(name: &str) -> (PathBuf, PathBuf) {
        let home = PathBuf::from(format!("/tmp/vc{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("Library/Caches")).unwrap();
        let runtime = home.join("Library/Caches/vr-space");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        (home, runtime.join("control.sock"))
    }
}
