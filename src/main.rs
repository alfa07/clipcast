#!/usr/bin/env rust-script
//! ```cargo
//! [dependencies]
//! clap = { version = "4.5.23", features = ["derive"] }
//! serde = { version = "1.0.215", features = ["derive"] }
//! serde_json = "1.0.133"
//! shlex = "1.3.0"
//! tokio = { version = "1.42.0", features = ["full"] }
//! tracing = "0.1.41"
//! tracing-subscriber = { version = "0.3.19", features = ["env-filter"] }
//! ```
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use shlex;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::{self, timeout, Duration};
use tracing::{error, info};

const TIMEOUT_DURATION: Duration = Duration::from_secs(5);
const CLIPBOARD_CHECK_INTERVAL: Duration = Duration::from_millis(500);
const PING_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    #[command(name = "server")]
    ServerCmd(ServerCmd),
    #[command(name = "client")]
    ClientCmd(ClientCmd),
}

#[derive(Args, Debug)]
struct ServerCmd {
    /// Command to write to clipboard
    #[arg(long, default_value = "xclip -selection clipboard")]
    write_clipboard_cmd: String,

    /// Command to read from clipboard
    #[arg(long, default_value = "xclip -selection clipboard -o")]
    read_clipboard_cmd: String,
}

#[derive(Args, Debug)]
struct ClientCmd {
    /// SSH host to connect to
    #[arg(long)]
    host: String,

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
}

struct Server {
    last_clipboard: String,
    write_cmd: String,
    read_cmd: String,
}

impl Server {
    fn new(write_cmd: String, read_cmd: String) -> Self {
        Server {
            last_clipboard: String::new(),
            write_cmd,
            read_cmd,
        }
    }

    async fn get_clipboard(&self) -> Result<String, std::io::Error> {
        let args = shlex::split(&self.read_cmd).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid read command")
        })?;

        if args.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Empty read command",
            ));
        }

        let output = Command::new(&args[0])
            .args(&args[1..])
            .output().await?;

        Ok(String::from_utf8(output.stdout).unwrap_or_default())
    }

    async fn set_clipboard(&self, content: &str) -> Result<(), std::io::Error> {
        let args = shlex::split(&self.write_cmd).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid write command")
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

    async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();

        let mut interval = time::interval(CLIPBOARD_CHECK_INTERVAL);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Ok(current_clip) = self.get_clipboard().await {
                        if current_clip != self.last_clipboard {
                            self.last_clipboard = current_clip.clone();
                            let message = Message::Clip { clip: current_clip };
                            send_with_timeout(&mut stdout, message).await?;
                        }
                    }
                }
                line_result = lines.next_line() => {
                    match line_result {
                        Ok(Some(line)) => {
                            match serde_json::from_str::<Message>(&line) {
                                Ok(message) => {
                                    match message {
                                        Message::Ping => {
                                            send_with_timeout(&mut stdout, Message::Pong).await?;
                                        }
                                        Message::Clip { clip } => {
                                            self.last_clipboard = clip.clone();
                                            if let Err(e) = self.set_clipboard(&clip).await {
                                                eprintln!("Error setting clipboard: {}", e);
                                                return Err(e.into());
                                            }
                                            send_with_timeout(&mut stdout, Message::Ack).await?;
                                        }
                                        _ => {}
                                    }
                                }
                                Err(e) => {
                                    eprintln!("Error parsing message: {}", e);
                                    return Err(e.into());
                                }
                            }
                        }
                        Ok(None) => {
                            // EOF reached
                            return Ok(());
                        }
                        Err(e) => {
                            eprintln!("Error reading from stdin: {}", e);
                            return Err(e.into());
                        }
                    }
                }
            }
        }
    }
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
            return Err(e.into());
        }
        Err(e) => {
            eprintln!("Timeout writing to stdout: {}", e);
            return Err(e.into());
        }
    }
}

struct Client {
    host: String,
    last_clipboard: String,
    write_cmd: String,
    read_cmd: String,
    remote_server_cmd: String,
    remote_write_cmd: String,
    remote_read_cmd: String,
}

impl Client {
    fn new(
        host: String,
        write_cmd: String,
        read_cmd: String,
        remote_server_cmd: String,
        remote_write_cmd: String,
        remote_read_cmd: String,
    ) -> Self {
        Client {
            host,
            last_clipboard: String::new(),
            write_cmd,
            read_cmd,
            remote_server_cmd,
            remote_write_cmd,
            remote_read_cmd,
        }
    }

    async fn get_clipboard(&self) -> Result<String, std::io::Error> {
        let args = shlex::split(&self.read_cmd).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid read command")
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

    async fn set_clipboard(&self, content: &str) -> Result<(), std::io::Error> {
        let args = shlex::split(&self.write_cmd).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid write command")
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
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(content.as_bytes()).await?;
        }
        child.wait().await?;
        Ok(())
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

    async fn run_connection(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut args = vec![self.host.as_str()];
        let mut remote_args = vec![self.remote_server_cmd.clone(), "server".into()];

        remote_args.push("--write-clipboard-cmd".into());
        remote_args.push(format!("'{}'", self.remote_write_cmd.clone()));

        remote_args.push("--read-clipboard-cmd".into());
        remote_args.push(format!("'{}'", self.remote_read_cmd.clone()));

        args.push("--");
        let remote_args = remote_args.join(" ");
        args.push(&remote_args);
        info!("Connecting to remote server: {:?}", args);

        let mut child = Command::new("ssh")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout).lines();

        let mut clip_interval = time::interval(CLIPBOARD_CHECK_INTERVAL);
        let mut ping_interval = time::interval(PING_INTERVAL);

        loop {
            tokio::select! {
                _ = clip_interval.tick() => {
                    if let Ok(current_clip) = self.get_clipboard().await {
                        if current_clip != self.last_clipboard {
                            info!("sending clipboard: len={}", current_clip.len());
                            self.last_clipboard = current_clip.clone();
                            let message = Message::Clip { clip: current_clip };
                            send_with_timeout(&mut stdin, message).await?;
                        }
                    }
                }
                _ = ping_interval.tick() => {
                    info!("sending ping");
                    send_with_timeout(&mut stdin, Message::Ping).await?;
                }
                line_result = reader.next_line() => {
                    match line_result {
                        Ok(Some(line)) => {
                            match serde_json::from_str::<Message>(&line) {
                                Ok(message) => {
                                    match message {
                                        Message::Clip { clip } => {
                                            info!("received clipboard: len={}", clip.len());
                                            self.last_clipboard = clip.clone();
                                            if let Err(e) = self.set_clipboard(&clip).await {
                                                error!("Error setting clipboard: {}", e);
                                                return Err(e.into());
                                            }
                                        }
                                        Message::Pong => {
                                            info!("received pong");
                                        }
                                        Message::Ack => {
                                            info!("received ack");
                                        }
                                        _ => {}
                                    }
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
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::ServerCmd(server) => run_server(server).await?,
        Cmd::ClientCmd(client) => run_client(client).await?,
    }
    Ok(())
}

async fn run_server(cli: ServerCmd) -> Result<(), Box<dyn std::error::Error>> {
    let mut server = Server::new(cli.write_clipboard_cmd, cli.read_clipboard_cmd);
    server.run().await
}

async fn run_client(cli: ClientCmd) -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let mut client = Client::new(
        cli.host,
        cli.write_clipboard_cmd,
        cli.read_clipboard_cmd,
        cli.remote_server_cmd,
        cli.remote_write_clipboard_cmd,
        cli.remote_read_clipboard_cmd,
    );
    client.run().await?;
    Ok(())
}

pub fn init_tracing() {
    use tracing_subscriber::{
        layer::SubscriberExt,
        util::SubscriberInitExt,
    };
    use std::str::FromStr;
    // Get log level from environment variable or use default
    let env_filter = tracing_subscriber::EnvFilter::from_str(
        std::env::var("RUST_LOG")
            .unwrap_or_else(|_| "info".into())
            .as_str(),
    )
    .unwrap();

    // Create a formatting layer with customized options
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)      // Include target in output
        .with_thread_ids(false)  // Include thread IDs
        .with_thread_names(false) // Include thread names
        .with_file(true)        // Include file name
        .with_line_number(true) // Include line number
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE) // Log spans when they close
        .compact();             // Use compact format

    // Initialize the tracing subscriber
    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .init();
}
