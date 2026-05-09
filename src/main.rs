#!/usr/bin/env rust-script
//! ```cargo
//! [dependencies]
//! base64 = "0.22"
//! clap = { version = "4.5.23", features = ["derive"] }
//! clap_complete = "4.4.10"
//! rand = "0.8"
//! serde = { version = "1.0.215", features = ["derive"] }
//! serde_json = "1.0.133"
//! shlex = "1.3.0"
//! tokio = { version = "1.42.0", features = ["full"] }
//! tracing = "0.1.41"
//! tracing-subscriber = { version = "0.3.19", features = ["env-filter"] }
//! ```
mod deploy;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use rand::distributions::{Alphanumeric, DistString};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs as tfs;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt,
    BufReader,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{self, timeout, Duration};
use tracing::{error, info, warn};

const TIMEOUT_DURATION: Duration = Duration::from_secs(5);
const CLIPBOARD_CHECK_INTERVAL: Duration = Duration::from_millis(500);
const PING_INTERVAL: Duration = Duration::from_secs(3);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);

const MAX_OPEN_FILE_SIZE: u64 = 50 * 1024 * 1024;
const MAX_OPEN_FILES: usize = 1024;
const MAX_OPEN_TOTAL: u64 = 250 * 1024 * 1024;
const OPEN_CHUNK_SIZE: usize = 256 * 1024;

const DEFAULT_OPEN_ALLOWLIST: &str = "pdf,png,jpg,jpeg,gif,webp,svg,txt,md,html,htm,csv,json,log,mp4,mov,mp3,wav,zip";

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    #[command(name = "server")]
    Server(ServerCmd),

    #[command(name = "client")]
    Client(ClientCmd),

    #[command(name = "generate")]
    Generate(GenerateCmd),

    #[command(name = "deploy")]
    Deploy(deploy::DeployCmd),
}

#[derive(Args, Debug)]
struct ServerCmd {
    /// Command to write to clipboard
    #[arg(long, default_value = "xclip -selection clipboard")]
    write_clipboard_cmd: String,

    /// Command to read from clipboard
    #[arg(long, default_value = "xclip -selection clipboard -o")]
    read_clipboard_cmd: String,

    /// Unix socket path that the server listens on for local `open`
    /// requests. Empty = default (`$XDG_RUNTIME_DIR/clipcast-$USER.sock`
    /// or `/tmp/clipcast-$USER.sock`).
    #[arg(long, default_value = "")]
    control_socket: String,
}

#[derive(Args, Debug)]
struct ClientCmd {
    /// SSH host to connect to
    #[arg(long)]
    host: String,

    /// Arguments for SSH session invoked by clipcast
    #[arg(long, allow_hyphen_values = true, num_args = 1, default_value = "")]
    ssh_args: String,

    /// Command to write to clipboard
    #[arg(long, default_value = "pbcopy")]
    write_clipboard_cmd: String,

    /// Command to read from clipboard
    #[arg(long, default_value = "pbpaste")]
    read_clipboard_cmd: String,

    #[arg(long, default_value = "clipcast")]
    remote_server_cmd: String,

    /// Remote command to write to clipboard
    #[arg(long, default_value = "xclip -selection clipboard")]
    remote_write_clipboard_cmd: String,

    /// Remote command to read from clipboard
    #[arg(long, default_value = "xclip -selection clipboard -o")]
    remote_read_clipboard_cmd: String,

    /// Override the remote server's control socket path (passed through
    /// as `--control-socket`). Empty = server uses its default.
    #[arg(long, default_value = "")]
    remote_control_socket: String,

    /// Local command used to open synced files on the Mac.
    #[arg(long, default_value = "open")]
    local_open_cmd: String,

    /// Comma-separated list of file extensions (lowercased, no dot) that
    /// are allowed to be passed to `local_open_cmd`. Files outside this
    /// list are still saved under `open_base_dir` but are NOT opened.
    #[arg(long, default_value = DEFAULT_OPEN_ALLOWLIST)]
    open_allowlist: String,

    /// Directory under which remote-synced files are stored. Supports
    /// `~/` prefix.
    #[arg(long, default_value = "~/.clipcast/remote")]
    open_base_dir: String,
}

#[derive(Debug, Copy, Clone, ValueEnum)]
pub enum Shell {
    #[value(name = "complete-bash")]
    Bash,
    #[value(name = "complete-zsh")]
    Zsh,
    #[value(name = "complete-fish")]
    Fish,
}

#[derive(Args, Debug)]
struct GenerateCmd {
    /// Generate shell completion script
    #[arg(value_enum)]
    shell: Shell,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
enum Message {
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "clip")]
    Clip { clip: String },
    #[serde(rename = "ack")]
    Ack,
    #[serde(rename = "open_begin")]
    OpenBegin {
        request_id: u64,
        files: Vec<OpenFileMeta>,
        extra_args: Vec<ArgSlot>,
    },
    #[serde(rename = "open_chunk")]
    OpenChunk { request_id: u64, index: u32, data_b64: String, eof: bool },
    #[serde(rename = "open_result")]
    OpenResult { request_id: u64, ok: bool, error: Option<String> },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct OpenFileMeta {
    basename: String,
    size: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ArgSlot {
    Literal { value: String },
    File { index: u32 },
}

struct ReceiverCtx {
    host: String,
    base_dir: PathBuf,
    allowlist: HashSet<String>,
    open_cmd: String,
    states: HashMap<u64, ReceiverState>,
}

struct ReceiverState {
    paths: Vec<PathBuf>,
    handles: Vec<Option<tfs::File>>,
    extra_args: Vec<ArgSlot>,
    remaining: usize,
}

struct RelayCtx {
    pending: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Message>>>>,
}

enum OpenRole {
    Receiver(ReceiverCtx),
    Relay(RelayCtx),
}

struct Server {
    cmd: ServerCmd,
}

impl Server {
    fn new(cmd: ServerCmd) -> Self {
        Server { cmd }
    }

    async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let socket_path = if self.cmd.control_socket.is_empty() {
            default_control_socket()
        } else {
            PathBuf::from(&self.cmd.control_socket)
        };
        if let Some(parent) = socket_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)?;
        let _ = std::fs::set_permissions(
            &socket_path,
            std::fs::Permissions::from_mode(0o600),
        );
        info!("control socket listening at {}", socket_path.display());

        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<Message>();
        let pending: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Message>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let accept_handle = {
            let outbound_tx = outbound_tx.clone();
            let pending = pending.clone();
            tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _addr)) => {
                            let tx = outbound_tx.clone();
                            let pending = pending.clone();
                            tokio::spawn(handle_socket_client(
                                stream, tx, pending,
                            ));
                        }
                        Err(e) => {
                            error!("socket accept error: {}", e);
                            break;
                        }
                    }
                }
            })
        };

        let _sentinel = outbound_tx;

        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let reader = BufReader::new(stdin);
        let lines = reader.lines();

        let mut role = OpenRole::Relay(RelayCtx { pending });
        let result = run_message_loop(
            &self.cmd.read_clipboard_cmd,
            &self.cmd.write_clipboard_cmd,
            &mut stdout,
            lines,
            outbound_rx,
            &mut role,
        )
        .await;

        accept_handle.abort();
        let _ = std::fs::remove_file(&socket_path);
        result
    }
}

async fn check_and_send_update<T>(
    read_cmd: &str,
    last_clipboard: &mut String,
    stdout: &mut T,
) -> Result<(), Box<dyn std::error::Error>>
where
    T: AsyncWrite + Unpin,
{
    if let Ok(current_clip) = get_clipboard(read_cmd).await {
        if current_clip != *last_clipboard {
            info!("sending clipboard: len={}", current_clip.len());
            *last_clipboard = current_clip.clone();
            let message = Message::Clip { clip: current_clip };
            send_with_timeout(stdout, message).await?;
        }
    }
    Ok(())
}

async fn get_clipboard(read_cmd: &str) -> Result<String, std::io::Error> {
    let args = shlex::split(read_cmd).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Invalid read command",
        )
    })?;

    if args.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Empty read command",
        ));
    }

    let output = Command::new(&args[0]).args(&args[1..]).output().await?;

    Ok(String::from_utf8(output.stdout).unwrap_or_default())
}

async fn set_clipboard(
    write_cmd: &str,
    content: &str,
) -> Result<(), std::io::Error> {
    let args = shlex::split(write_cmd).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Invalid write command",
        )
    })?;

    if args.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Empty write command",
        ));
    }

    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(content.as_bytes()).await?;
        stdin.flush().await?;
        stdin.shutdown().await?;
    }
    child.wait().await?;
    Ok(())
}

async fn send_with_timeout<T>(
    stdout: &mut T,
    message: Message,
) -> Result<(), Box<dyn std::error::Error>>
where
    T: AsyncWrite + Unpin,
{
    match timeout(TIMEOUT_DURATION, async {
        let message = serde_json::to_string(&message)?;
        stdout.write_all(message.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
        Ok::<(), std::io::Error>(())
    })
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            eprintln!("Error writing to stdout: {}", e);
            Err(e.into())
        }
        Err(e) => {
            eprintln!("Timeout writing to stdout: {}", e);
            Err(e.into())
        }
    }
}

struct Client {
    cmd: ClientCmd,
}

impl Client {
    fn new(cmd: ClientCmd) -> Self {
        Client { cmd }
    }

    async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            match self.run_connection().await {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("Connection error: {}", e);
                    time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
    }

    async fn run_connection(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut args: Vec<&str>;
        if self.cmd.ssh_args.is_empty() {
            args = vec![self.cmd.host.as_str()];
        } else {
            args = self.cmd.ssh_args.split(' ').collect();
            args.push(self.cmd.host.as_str());
        }
        let mut remote_args =
            vec![self.cmd.remote_server_cmd.clone(), "server".into()];

        remote_args.push("--write-clipboard-cmd".into());
        remote_args
            .push(format!("'{}'", self.cmd.remote_write_clipboard_cmd.clone()));

        remote_args.push("--read-clipboard-cmd".into());
        remote_args
            .push(format!("'{}'", self.cmd.remote_read_clipboard_cmd.clone()));

        if !self.cmd.remote_control_socket.is_empty() {
            remote_args.push("--control-socket".into());
            remote_args
                .push(format!("'{}'", self.cmd.remote_control_socket.clone()));
        }

        args.push("--");
        let remote_args = remote_args.join(" ");
        args.push(&remote_args);
        info!("connecting to remote server: {:?}", args);

        let mut child = Command::new("ssh")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout).lines();

        let base_dir = expand_home(&self.cmd.open_base_dir);
        let allowlist: HashSet<String> = self
            .cmd
            .open_allowlist
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let mut role = OpenRole::Receiver(ReceiverCtx {
            host: self.cmd.host.clone(),
            base_dir,
            allowlist,
            open_cmd: self.cmd.local_open_cmd.clone(),
            states: HashMap::new(),
        });

        let (_outbound_tx, outbound_rx) = mpsc::unbounded_channel::<Message>();
        // _outbound_tx is held for the duration of the connection so that
        // outbound_rx never sees a closed channel. The Mac client has no
        // external injectors; it writes directly to stdin inside dispatch.

        run_message_loop(
            &self.cmd.read_clipboard_cmd,
            &self.cmd.write_clipboard_cmd,
            &mut stdin,
            reader,
            outbound_rx,
            &mut role,
        )
        .await
    }
}

async fn run_message_loop<R, W>(
    read_cmd: &str,
    write_cmd: &str,
    stdin: &mut W,
    mut reader: tokio::io::Lines<R>,
    mut outbound_rx: mpsc::UnboundedReceiver<Message>,
    role: &mut OpenRole,
) -> Result<(), Box<dyn std::error::Error>>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut last_clipboard = String::new();
    let mut clip_interval = time::interval(CLIPBOARD_CHECK_INTERVAL);
    let mut ping_interval = time::interval(PING_INTERVAL);

    let mut last_pong = time::Instant::now();

    while (time::Instant::now() - last_pong) < PONG_TIMEOUT {
        tokio::select! {
            _ = clip_interval.tick() => {
                check_and_send_update(read_cmd, &mut last_clipboard, stdin).await?;
            }
            _ = ping_interval.tick() => {
                info!("sending ping");
                send_with_timeout(stdin, Message::Ping).await?;
            }
            Some(injected) = outbound_rx.recv() => {
                send_with_timeout(stdin, injected).await?;
            }
            line_result = reader.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        match serde_json::from_str::<Message>(&line) {
                            Ok(message) => {
                                dispatch_message(
                                    message,
                                    write_cmd,
                                    &mut last_clipboard,
                                    &mut last_pong,
                                    role,
                                    stdin,
                                ).await?;
                            }
                            Err(e) => {
                                error!("Error parsing message: {}", e);
                                return Err(e.into());
                            }
                        }
                    }
                    Ok(None) => {
                        return Err("Connection closed".into());
                    }
                    Err(e) => {
                        error!("Error reading from stdout: {}", e);
                        return Err(e.into());
                    }
                }
            }
        }
    }
    error!("pong timed out");
    Err("Pong timeout".into())
}

async fn dispatch_message<W>(
    message: Message,
    write_cmd: &str,
    last_clipboard: &mut String,
    last_pong: &mut time::Instant,
    role: &mut OpenRole,
    stdin: &mut W,
) -> Result<(), Box<dyn std::error::Error>>
where
    W: AsyncWrite + Unpin,
{
    match message {
        Message::Clip { clip } => {
            info!("received clipboard: len={}", clip.len());
            *last_clipboard = clip.clone();
            if let Err(e) = set_clipboard(write_cmd, &clip).await {
                error!("Error setting clipboard: {}", e);
                return Err(e.into());
            }
        }
        Message::Ping => {
            info!("received ping");
            send_with_timeout(stdin, Message::Pong).await?;
        }
        Message::Pong => {
            info!("received pong");
            *last_pong = time::Instant::now();
        }
        Message::Ack => {
            info!("received ack");
        }
        Message::OpenBegin { request_id, files, extra_args } => match role {
            OpenRole::Receiver(ctx) => {
                match handle_open_begin(ctx, request_id, files, extra_args)
                    .await
                {
                    Ok(Some(result)) => {
                        send_with_timeout(stdin, result).await?;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("open_begin {} failed: {}", request_id, e);
                        ctx.states.remove(&request_id);
                        send_with_timeout(
                            stdin,
                            Message::OpenResult {
                                request_id,
                                ok: false,
                                error: Some(e.to_string()),
                            },
                        )
                        .await?;
                    }
                }
            }
            _ => warn!("unexpected OpenBegin on non-receiver role"),
        },
        Message::OpenChunk { request_id, index, data_b64, eof } => match role {
            OpenRole::Receiver(ctx) => {
                match handle_open_chunk(ctx, request_id, index, &data_b64, eof)
                    .await
                {
                    Ok(Some(result)) => {
                        send_with_timeout(stdin, result).await?;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("open_chunk {} failed: {}", request_id, e);
                        ctx.states.remove(&request_id);
                        send_with_timeout(
                            stdin,
                            Message::OpenResult {
                                request_id,
                                ok: false,
                                error: Some(e.to_string()),
                            },
                        )
                        .await?;
                    }
                }
            }
            _ => warn!("unexpected OpenChunk on non-receiver role"),
        },
        Message::OpenResult { request_id, ok, error } => match role {
            OpenRole::Relay(ctx) => {
                let sender_opt = ctx.pending.lock().await.remove(&request_id);
                if let Some(sender) = sender_opt {
                    let _ = sender.send(Message::OpenResult {
                        request_id,
                        ok,
                        error,
                    });
                } else {
                    warn!(
                        "OpenResult for unknown request {} (dropped)",
                        request_id
                    );
                }
            }
            _ => warn!("unexpected OpenResult on non-relay role"),
        },
    }
    Ok(())
}

async fn handle_open_begin(
    ctx: &mut ReceiverCtx,
    request_id: u64,
    files: Vec<OpenFileMeta>,
    extra_args: Vec<ArgSlot>,
) -> Result<Option<Message>, Box<dyn std::error::Error>> {
    if files.len() > MAX_OPEN_FILES {
        return Err(format!("too many files: {}", files.len()).into());
    }
    let total: u64 = files.iter().map(|f| f.size).sum();
    if total > MAX_OPEN_TOTAL {
        return Err(format!("total size {} exceeds limit", total).into());
    }
    for f in &files {
        if f.size > MAX_OPEN_FILE_SIZE {
            return Err(
                format!("file {} exceeds per-file limit", f.basename).into()
            );
        }
    }

    // Zero-file request — all positional args were literals (flags, URLs,
    // or paths that didn't stat as a regular file on the remote). Run the
    // local `open` command immediately with the literal args; no files to
    // stream, no target directory needed.
    if files.is_empty() {
        info!(
            "open_begin request_id={} files=0 (literal-only, running \
             immediately)",
            request_id
        );
        let state = ReceiverState {
            paths: Vec::new(),
            handles: Vec::new(),
            extra_args,
            remaining: 0,
        };
        let result =
            finalize_open(&ctx.allowlist, &ctx.open_cmd, request_id, state)
                .await;
        return Ok(Some(result));
    }

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rand_suffix =
        Alphanumeric.sample_string(&mut rand::thread_rng(), 6).to_lowercase();
    let host_dir = ctx.base_dir.join(sanitize_component(&ctx.host));
    let dir = host_dir.join(format!("{}-{}", secs, rand_suffix));

    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;

    let mut used: HashSet<String> = HashSet::new();
    let mut paths = Vec::with_capacity(files.len());
    let mut handles = Vec::with_capacity(files.len());
    for meta in &files {
        let base = sanitize_basename(&meta.basename)
            .ok_or_else(|| format!("invalid basename: {:?}", meta.basename))?;
        let unique = dedupe_name(&used, &base);
        used.insert(unique.clone());
        let path = dir.join(&unique);
        let f = tfs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .await?;
        paths.push(path);
        handles.push(Some(f));
    }

    let remaining = files.len();
    ctx.states.insert(
        request_id,
        ReceiverState { paths, handles, extra_args, remaining },
    );
    info!(
        "open_begin request_id={} files={} into {}",
        request_id,
        remaining,
        dir.display()
    );
    Ok(None)
}

async fn handle_open_chunk(
    ctx: &mut ReceiverCtx,
    request_id: u64,
    index: u32,
    data_b64: &str,
    eof: bool,
) -> Result<Option<Message>, Box<dyn std::error::Error>> {
    let state =
        ctx.states.get_mut(&request_id).ok_or("chunk for unknown request")?;
    let idx = index as usize;
    let slot = state.handles.get_mut(idx).ok_or("chunk index out of range")?;
    let f = slot.as_mut().ok_or("chunk for already-closed file")?;
    if !data_b64.is_empty() {
        let bytes = B64.decode(data_b64)?;
        f.write_all(&bytes).await?;
    }
    if eof {
        if let Some(mut f) = slot.take() {
            f.flush().await?;
        }
        state.remaining -= 1;
        if state.remaining == 0 {
            let state = ctx.states.remove(&request_id).unwrap();
            let result =
                finalize_open(&ctx.allowlist, &ctx.open_cmd, request_id, state)
                    .await;
            return Ok(Some(result));
        }
    }
    Ok(None)
}

async fn finalize_open(
    allowlist: &HashSet<String>,
    open_cmd: &str,
    request_id: u64,
    state: ReceiverState,
) -> Message {
    for p in &state.paths {
        let ext_ok = p
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| allowlist.contains(&s.to_lowercase()))
            .unwrap_or(false);
        if !ext_ok {
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            warn!(
                "request_id={} refusing to open {}: extension not in allowlist",
                request_id, name
            );
            return Message::OpenResult {
                request_id,
                ok: false,
                error: Some(format!("{}: extension not in allowlist", name)),
            };
        }
    }

    let parts = match shlex::split(open_cmd) {
        Some(p) if !p.is_empty() => p,
        _ => {
            return Message::OpenResult {
                request_id,
                ok: false,
                error: Some("invalid local_open_cmd".into()),
            };
        }
    };
    let mut cmd = Command::new(&parts[0]);
    cmd.args(&parts[1..]);
    for slot in &state.extra_args {
        match slot {
            ArgSlot::Literal { value } => {
                cmd.arg(value);
            }
            ArgSlot::File { index } => match state.paths.get(*index as usize) {
                Some(p) => {
                    cmd.arg(p);
                }
                None => {
                    return Message::OpenResult {
                        request_id,
                        ok: false,
                        error: Some(format!("bad file slot index {}", index)),
                    };
                }
            },
        }
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let cmd_display = format_command(cmd.as_std());
    info!("running local open (request_id={}): {}", request_id, cmd_display);
    match cmd.output().await {
        Ok(out) => {
            let code = match out.status.code() {
                Some(c) => c.to_string(),
                None => format!("signal ({})", out.status),
            };
            let stderr =
                String::from_utf8_lossy(&out.stderr).trim().to_string();
            let stdout =
                String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() {
                info!(
                    "local open finished (request_id={}): exit={} stdout={:?} \
                     stderr={:?}",
                    request_id, code, stdout, stderr
                );
                Message::OpenResult { request_id, ok: true, error: None }
            } else {
                warn!(
                    "local open failed (request_id={}): exit={} stdout={:?} \
                     stderr={:?}",
                    request_id, code, stdout, stderr
                );
                Message::OpenResult {
                    request_id,
                    ok: false,
                    error: Some(format!("open exit {}: {}", out.status, stderr)),
                }
            }
        }
        Err(e) => {
            warn!(
                "local open spawn failed (request_id={}): {}",
                request_id, e
            );
            Message::OpenResult {
                request_id,
                ok: false,
                error: Some(format!("spawn failed: {}", e)),
            }
        }
    }
}

fn format_command(cmd: &std::process::Command) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(cmd.get_program().to_string_lossy().into_owned());
    for a in cmd.get_args() {
        parts.push(a.to_string_lossy().into_owned());
    }
    parts
        .iter()
        .map(|s| match shlex::try_quote(s) {
            Ok(q) => q.into_owned(),
            Err(_) => format!("{:?}", s),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

async fn handle_socket_client(
    stream: UnixStream,
    outbound_tx: mpsc::UnboundedSender<Message>,
    pending: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Message>>>>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Message>();
    let mut req_id: Option<u64> = None;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        match serde_json::from_str::<Message>(&l) {
                            Ok(msg) => {
                                if req_id.is_none() {
                                    if let Message::OpenBegin { request_id, .. } = &msg {
                                        req_id = Some(*request_id);
                                        pending
                                            .lock()
                                            .await
                                            .insert(*request_id, reply_tx.clone());
                                    }
                                }
                                if outbound_tx.send(msg).is_err() {
                                    warn!("outbound channel closed");
                                    break;
                                }
                            }
                            Err(e) => {
                                error!("socket parse error: {}", e);
                                break;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        error!("socket read error: {}", e);
                        break;
                    }
                }
            }
            reply = reply_rx.recv() => {
                match reply {
                    Some(msg) => {
                        let done = matches!(msg, Message::OpenResult { .. });
                        let s = match serde_json::to_string(&msg) {
                            Ok(s) => s,
                            Err(e) => {
                                error!("serialize reply: {}", e);
                                break;
                            }
                        };
                        if writer.write_all(s.as_bytes()).await.is_err() {
                            break;
                        }
                        if writer.write_all(b"\n").await.is_err() {
                            break;
                        }
                        let _ = writer.flush().await;
                        if done {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    if let Some(id) = req_id {
        pending.lock().await.remove(&id);
    }
}

async fn run_open_client(
    raw_args: Vec<OsString>,
) -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = raw_args
        .into_iter()
        .map(|a| {
            a.into_string()
                .map_err(|bad| format!("non-UTF-8 argument: {:?}", bad))
        })
        .collect::<Result<_, _>>()?;

    if args.is_empty() {
        return Err("open: no arguments".into());
    }

    let mut file_paths: Vec<PathBuf> = Vec::new();
    let mut file_metas: Vec<OpenFileMeta> = Vec::new();
    let mut slots: Vec<ArgSlot> = Vec::with_capacity(args.len());
    let mut total: u64 = 0;

    for arg in &args {
        let is_flag = arg.starts_with('-');
        let is_url = arg.contains("://");
        let mut is_file = false;
        let mut size: u64 = 0;
        let mut canon: Option<PathBuf> = None;
        let mut literal_reason = String::new();

        if is_flag {
            literal_reason = "flag".into();
        } else if is_url {
            literal_reason = "url".into();
        } else {
            match std::fs::metadata(arg) {
                Ok(meta) if meta.is_file() => {
                    is_file = true;
                    size = meta.len();
                    canon = std::fs::canonicalize(arg).ok();
                }
                Ok(meta) => {
                    literal_reason = format!(
                        "stat ok but not a regular file (dir={}, symlink={})",
                        meta.is_dir(),
                        meta.file_type().is_symlink()
                    );
                }
                Err(e) => {
                    literal_reason = format!("stat failed: {}", e);
                }
            }
        }

        if is_file {
            if size > MAX_OPEN_FILE_SIZE {
                return Err(format!(
                    "{}: exceeds per-file limit of {} bytes",
                    arg, MAX_OPEN_FILE_SIZE
                )
                .into());
            }
            total = total.saturating_add(size);
            if total > MAX_OPEN_TOTAL {
                return Err(format!(
                    "total size exceeds {} bytes",
                    MAX_OPEN_TOTAL
                )
                .into());
            }
            if file_metas.len() >= MAX_OPEN_FILES {
                return Err(
                    format!("too many files (max {})", MAX_OPEN_FILES).into()
                );
            }
            let path = canon.unwrap();
            let basename = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or("bad file basename")?
                .to_string();
            let index = file_metas.len() as u32;
            info!(
                "open-client: {:?} -> sync as file ({} bytes)",
                arg, size
            );
            file_metas.push(OpenFileMeta { basename, size });
            file_paths.push(path);
            slots.push(ArgSlot::File { index });
        } else {
            info!(
                "open-client: {:?} -> passed through literally ({})",
                arg, literal_reason
            );
            slots.push(ArgSlot::Literal { value: arg.clone() });
        }
    }

    let sock = std::env::var("CLIPCAST_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_control_socket());
    let stream = UnixStream::connect(&sock)
        .await
        .map_err(|e| format!("connect {}: {}", sock.display(), e))?;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let request_id: u64 = rand::thread_rng().gen();

    let begin = Message::OpenBegin {
        request_id,
        files: file_metas.clone(),
        extra_args: slots,
    };
    write_json_line(&mut writer, &begin).await?;

    for (idx, path) in file_paths.iter().enumerate() {
        let mut f = tfs::File::open(path).await?;
        let mut buf = vec![0u8; OPEN_CHUNK_SIZE];
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                let msg = Message::OpenChunk {
                    request_id,
                    index: idx as u32,
                    data_b64: String::new(),
                    eof: true,
                };
                write_json_line(&mut writer, &msg).await?;
                break;
            }
            let data_b64 = B64.encode(&buf[..n]);
            let msg = Message::OpenChunk {
                request_id,
                index: idx as u32,
                data_b64,
                eof: false,
            };
            write_json_line(&mut writer, &msg).await?;
        }
    }

    while let Some(line) = lines.next_line().await? {
        match serde_json::from_str::<Message>(&line)? {
            Message::OpenResult { request_id: rid, ok, error }
                if rid == request_id =>
            {
                if ok {
                    return Ok(());
                } else {
                    return Err(error
                        .unwrap_or_else(|| "open failed".into())
                        .into());
                }
            }
            _ => continue,
        }
    }
    Err("socket closed before result".into())
}

async fn write_json_line<W: AsyncWrite + Unpin>(
    w: &mut W,
    msg: &Message,
) -> Result<(), Box<dyn std::error::Error>> {
    let s = serde_json::to_string(msg)?;
    w.write_all(s.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;
    Ok(())
}

fn default_control_socket() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    base.join(format!("clipcast-{}.sock", user))
}

fn sanitize_basename(s: &str) -> Option<String> {
    let p = Path::new(s);
    let name = p.file_name()?.to_str()?;
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(name.to_string())
}

fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn dedupe_name(existing: &HashSet<String>, name: &str) -> String {
    if !existing.contains(name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1..u32::MAX {
        let candidate = format!("{}-{}{}", stem, n, ext);
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    format!("{}.dup", name)
}

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argv0 = std::env::args_os().next();
    let basename = argv0
        .as_ref()
        .and_then(|p| Path::new(p).file_name())
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    if let Some(name) = basename.as_deref() {
        if name != "clipcast" && !name.is_empty() {
            init_tracing();
            let rest: Vec<OsString> = std::env::args_os().skip(1).collect();
            return match run_open_client(rest).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("{}: {}", name, e);
                    std::process::exit(1);
                }
            };
        }
    }

    let cli = Cli::parse();

    match cli.command {
        Cmd::Server(server) => run_server(server).await?,
        Cmd::Client(client) => run_client(client).await?,
        Cmd::Generate(generate) => generate_completion(generate.shell),
        Cmd::Deploy(deploy_cmd) => {
            init_tracing();
            deploy::run(deploy_cmd).await?
        }
    }
    Ok(())
}

async fn run_server(cli: ServerCmd) -> Result<(), Box<dyn std::error::Error>> {
    let mut server = Server::new(cli);
    server.run().await
}

async fn run_client(cli: ClientCmd) -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let mut client = Client::new(cli);
    client.run().await?;
    Ok(())
}

pub fn generate_completion(shell: Shell) {
    let mut cli = Cli::command();
    match shell {
        Shell::Bash => clap_complete::generate(
            clap_complete::shells::Bash,
            &mut cli,
            "clipcast",
            &mut std::io::stdout(),
        ),
        Shell::Zsh => clap_complete::generate(
            clap_complete::shells::Zsh,
            &mut cli,
            "clipcast",
            &mut std::io::stdout(),
        ),
        Shell::Fish => clap_complete::generate(
            clap_complete::shells::Fish,
            &mut cli,
            "clipcast",
            &mut std::io::stdout(),
        ),
    }
}

pub fn init_tracing() {
    use std::str::FromStr;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let env_filter = tracing_subscriber::EnvFilter::from_str(
        std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()).as_str(),
    )
    .unwrap();

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_file(true)
        .with_line_number(false)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .compact();

    tracing_subscriber::registry().with(env_filter).with(fmt_layer).init();
}
