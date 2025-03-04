#!/usr/bin/env rust-script
//! ```cargo
//! [dependencies]
//! clap = { version = "4.5.23", features = ["derive"] }
//! clap_complete = "4.4.10"
//! serde = { version = "1.0.215", features = ["derive"] }
//! serde_json = "1.0.133"
//! shlex = "1.3.0"
//! tokio = { version = "1.42.0", features = ["full"] }
//! tracing = "0.1.41"
//! tracing-subscriber = { version = "0.3.19", features = ["env-filter"] }
//! ```
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::process::Command;
use tokio::time::{self, timeout, Duration};
use tracing::{error, info};

const TIMEOUT_DURATION: Duration = Duration::from_secs(5);
const CLIPBOARD_CHECK_INTERVAL: Duration = Duration::from_millis(500);
const PING_INTERVAL: Duration = Duration::from_secs(3);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);

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

    // SSH args
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
}

struct Server {
    cmd: ServerCmd,
}

impl Server {
    fn new(cmd: ServerCmd) -> Self {
        Server { cmd }
    }

    async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let reader = BufReader::new(stdin);
        let lines = reader.lines();

        run_message_loop(
            &self.cmd.read_clipboard_cmd,
            &self.cmd.write_clipboard_cmd,
            &mut stdout,
            lines,
        )
        .await
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
        // let mut args = vec![self.cmd.ssh_args.as_str()];
        let mut args: Vec<&str> = self.cmd.ssh_args.split(' ').collect();
        args.push(self.cmd.host.as_str());
        // let mut args = vec![self.cmd.host.as_str()];
        println!("{:?}", args);
        let mut remote_args =
            vec![self.cmd.remote_server_cmd.clone(), "server".into()];

        remote_args.push("--write-clipboard-cmd".into());
        remote_args
            .push(format!("'{}'", self.cmd.remote_write_clipboard_cmd.clone()));

        remote_args.push("--read-clipboard-cmd".into());
        remote_args
            .push(format!("'{}'", self.cmd.remote_read_clipboard_cmd.clone()));

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

        run_message_loop(
            &self.cmd.read_clipboard_cmd,
            &self.cmd.write_clipboard_cmd,
            &mut stdin,
            reader,
        )
        .await
    }
}

async fn run_message_loop<R, W>(
    read_cmd: &str,
    write_cmd: &str,
    stdin: &mut W,
    mut reader: tokio::io::Lines<R>,
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
            line_result = reader.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        match serde_json::from_str::<Message>(&line) {
                            Ok(message) => {
                                match message {
                                    Message::Clip { clip } => {
                                        info!("received clipboard: len={}", clip.len());
                                        last_clipboard = clip.clone();
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
                                        last_pong = time::Instant::now();
                                    }
                                    Message::Ack => {
                                        info!("received ack");
                                    }
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
    error!("pong timed out");
    Err("Pong timeout".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Server(server) => run_server(server).await?,
        Cmd::Client(client) => run_client(client).await?,
        Cmd::Generate(generate) => generate_completion(generate.shell),
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
