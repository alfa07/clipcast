//! `clipcast deploy` — one-command cross-compile + scp install.
//!
//! Probes the remote for arch/OS, cross-compiles the binary locally
//! (using `cargo-zigbuild` + `zig` on macOS→Linux), scps it atomically
//! to `~/.clipcast/bin/clipcast` on the remote, and creates an `open`
//! symlink next to it.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use clap::Args;
use tokio::process::Command;
use tracing::{info, warn};

type BoxError = Box<dyn std::error::Error>;

#[derive(Args, Debug)]
pub struct DeployCmd {
    /// SSH host to deploy to
    #[arg(long)]
    host: String,

    /// Arguments for ssh/scp invocations (space-separated)
    #[arg(long, allow_hyphen_values = true, num_args = 1, default_value = "")]
    ssh_args: String,

    /// Remote install directory. `~/` is expanded on the remote side.
    #[arg(long, default_value = "~/.clipcast/bin")]
    install_dir: String,

    /// Comma-separated symlink names to create in the install directory,
    /// each pointing at the deployed `clipcast` binary.
    #[arg(long, default_value = "open")]
    symlinks: String,

    /// Override the auto-detected target triple.
    #[arg(long, default_value = "")]
    target: String,

    /// Auto-install user-scope missing tools (`rustup target add`,
    /// `cargo install cargo-zigbuild`). Never runs Homebrew.
    #[arg(long, default_value_t = false)]
    yes: bool,

    /// Skip the build step; use the existing
    /// `target/<triple>/release/clipcast` binary.
    #[arg(long, default_value_t = false)]
    skip_build: bool,

    /// Print every step without executing any of them.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

pub async fn run(cmd: DeployCmd) -> Result<(), BoxError> {
    validate_shell_safe("install-dir", &cmd.install_dir)?;
    for s in parse_symlinks(&cmd.symlinks) {
        validate_shell_safe("symlinks", &s)?;
    }

    let ssh_args: Vec<String> = if cmd.ssh_args.is_empty() {
        Vec::new()
    } else {
        cmd.ssh_args.split(' ').map(|s| s.to_string()).collect()
    };

    let host_target = host_target_triple()?;
    info!("local host target: {}", host_target);

    let target = if !cmd.target.is_empty() {
        cmd.target.clone()
    } else {
        println!("probing {} for arch/os...", cmd.host);
        let (arch, os) = probe_remote_uname(&cmd.host, &ssh_args).await?;
        let t = map_target(&arch, &os)?;
        println!("remote: arch={} os={} -> target={}", arch, os, t);
        t
    };

    let needs_cross = host_target != target;
    let binary = if cmd.skip_build {
        let p = if needs_cross {
            PathBuf::from(format!("target/{}/release/clipcast", target))
        } else {
            PathBuf::from("target/release/clipcast")
        };
        if !p.exists() {
            return Err(format!(
                "--skip-build set but {} not found",
                p.display()
            )
            .into());
        }
        p
    } else if needs_cross {
        ensure_cross_toolchain(&target, cmd.yes, cmd.dry_run)?;
        build_cross(&target, cmd.dry_run).await?
    } else {
        build_native(cmd.dry_run).await?
    };

    println!("binary: {}", binary.display());

    upload_and_install(
        &cmd.host,
        &ssh_args,
        &binary,
        &cmd.install_dir,
        &parse_symlinks(&cmd.symlinks),
        cmd.dry_run,
    )
    .await?;

    if !cmd.dry_run {
        println!();
        println!("deployed clipcast to {}:{}", cmd.host, cmd.install_dir);
        println!("ensure it's on the remote PATH, e.g. add to ~/.bashrc:");
        println!("  export PATH=\"{}:$PATH\"", cmd.install_dir);
        println!();
        println!(
            "note: any running `clipcast server` on the remote still holds the old"
        );
        println!(
            "binary via an open FD; reconnect the client (or `pkill clipcast` on"
        );
        println!("the remote) to pick up the new code.");
    }
    Ok(())
}

async fn probe_remote_uname(
    host: &str,
    ssh_args: &[String],
) -> Result<(String, String), BoxError> {
    let mut args: Vec<String> = ssh_args.to_vec();
    args.push("-o".into());
    args.push("BatchMode=yes".into());
    args.push(host.to_string());
    args.push("uname -m; uname -s".into());
    let out = Command::new("ssh").args(&args).output().await?;
    if !out.status.success() {
        return Err(format!(
            "ssh probe of {} failed: {}",
            host,
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut lines = s.lines();
    let arch = lines.next().unwrap_or("").trim().to_string();
    let os = lines.next().unwrap_or("").trim().to_string();
    if arch.is_empty() || os.is_empty() {
        return Err(format!(
            "failed to parse uname output from {}: {:?}",
            host, s
        )
        .into());
    }
    Ok((arch, os))
}

fn map_target(arch: &str, os: &str) -> Result<String, BoxError> {
    let triple = match (arch, os) {
        ("x86_64", "Linux") | ("amd64", "Linux") => "x86_64-unknown-linux-musl",
        ("aarch64", "Linux") | ("arm64", "Linux") => {
            "aarch64-unknown-linux-musl"
        }
        ("x86_64", "Darwin") => "x86_64-apple-darwin",
        ("arm64", "Darwin") | ("aarch64", "Darwin") => "aarch64-apple-darwin",
        _ => {
            return Err(format!(
                "unsupported remote platform: arch={} os={}. Pass --target \
                 explicitly.",
                arch, os
            )
            .into());
        }
    };
    Ok(triple.to_string())
}

fn host_target_triple() -> Result<String, BoxError> {
    let out = std::process::Command::new("rustc").arg("-vV").output().map_err(
        |e| format!("failed to run `rustc -vV` (is rust installed?): {}", e),
    )?;
    if !out.status.success() {
        return Err("`rustc -vV` exited non-zero".into());
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(rest) = line.strip_prefix("host: ") {
            return Ok(rest.trim().to_string());
        }
    }
    Err("`rustc -vV` output missing `host:` line".into())
}

fn ensure_cross_toolchain(
    target: &str,
    yes: bool,
    dry_run: bool,
) -> Result<(), BoxError> {
    // 1. rustup target
    if !has_rustup_target(target) {
        if dry_run {
            println!("[dry-run] rustup target add {}", target);
        } else if yes {
            println!("installing rust target {}...", target);
            let status = std::process::Command::new("rustup")
                .args(["target", "add", target])
                .status()?;
            if !status.success() {
                return Err(
                    format!("`rustup target add {}` failed", target).into()
                );
            }
        } else {
            return Err(format!(
                "missing rust target `{t}`. Install it with:\n  rustup \
                 target add {t}\nor re-run with --yes.",
                t = target
            )
            .into());
        }
    }

    // 2. For linux targets from macOS, need zig + cargo-zigbuild
    let host_is_mac = cfg!(target_os = "macos");
    if host_is_mac && target.contains("linux") {
        if !binary_on_path("zig") {
            let msg = format!(
                "missing `zig` (required to cross-compile to {target}). \
                 Install it with:\n  brew install zig\n(Homebrew packages \
                 are never auto-installed; run this manually and re-run \
                 deploy.)"
            );
            if dry_run {
                println!("[dry-run] would fail: {}", msg);
            } else {
                return Err(msg.into());
            }
        }
        if !binary_on_path("cargo-zigbuild") {
            if dry_run {
                println!("[dry-run] cargo install cargo-zigbuild");
            } else if yes {
                println!("installing cargo-zigbuild...");
                let status = std::process::Command::new("cargo")
                    .args(["install", "cargo-zigbuild"])
                    .status()?;
                if !status.success() {
                    return Err("`cargo install cargo-zigbuild` failed".into());
                }
            } else {
                return Err("missing `cargo-zigbuild`. Install it with:\n  cargo install cargo-zigbuild\nor re-run with --yes.".into());
            }
        }
    }
    Ok(())
}

fn has_rustup_target(target: &str) -> bool {
    let out = std::process::Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.trim() == target),
        _ => false,
    }
}

/// True if a binary with this name is present on `$PATH`. Uses
/// `command -v` (a POSIX builtin) rather than invoking the binary
/// itself, so we don't care whether the tool understands `--version`
/// or what exit code it returns for it.
fn binary_on_path(name: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", shell_escape_single_arg(name)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn shell_escape_single_arg(s: &str) -> String {
    // Binary names from our own code are always safe (alphanumerics +
    // `-`), but be defensive: wrap in single quotes and escape any
    // embedded quote.
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn build_cross(target: &str, dry_run: bool) -> Result<PathBuf, BoxError> {
    let host_is_mac = cfg!(target_os = "macos");
    let use_zig = host_is_mac && target.contains("linux");
    let subcommand = if use_zig { "zigbuild" } else { "build" };
    let args = [subcommand, "--release", "--target", target];
    println!("building: cargo {}", args.join(" "));
    if dry_run {
        println!("[dry-run] skipping actual build");
    } else {
        let status = Command::new("cargo").args(args).status().await?;
        if !status.success() {
            return Err(format!("cargo {} failed", subcommand).into());
        }
    }
    Ok(PathBuf::from(format!("target/{}/release/clipcast", target)))
}

async fn build_native(dry_run: bool) -> Result<PathBuf, BoxError> {
    println!("building: cargo build --release");
    if dry_run {
        println!("[dry-run] skipping actual build");
    } else {
        let status =
            Command::new("cargo").args(["build", "--release"]).status().await?;
        if !status.success() {
            return Err("cargo build failed".into());
        }
    }
    Ok(PathBuf::from("target/release/clipcast"))
}

async fn upload_and_install(
    host: &str,
    ssh_args: &[String],
    binary: &Path,
    install_dir: &str,
    symlinks: &[String],
    dry_run: bool,
) -> Result<(), BoxError> {
    if !dry_run && !binary.exists() {
        return Err(format!("binary not found: {}", binary.display()).into());
    }

    run_ssh(host, ssh_args, &format!("mkdir -p {}", install_dir), dry_run)
        .await?;

    let scp_dest_path = remote_scp_path(install_dir, "clipcast.new");
    let scp_dest = format!("{}:{}", host, scp_dest_path);
    run_scp(ssh_args, binary, &scp_dest, dry_run).await?;

    run_ssh(
        host,
        ssh_args,
        &format!(
            "chmod 0755 {d}/clipcast.new && mv {d}/clipcast.new {d}/clipcast",
            d = install_dir
        ),
        dry_run,
    )
    .await?;

    for name in symlinks {
        if name.is_empty() {
            continue;
        }
        run_ssh(
            host,
            ssh_args,
            &format!("ln -sf {d}/clipcast {d}/{n}", d = install_dir, n = name),
            dry_run,
        )
        .await?;
    }
    Ok(())
}

/// Turn an install_dir into the path we hand to scp after `host:`.
///
/// scp evaluates the path after `host:` in the remote shell, but
/// quoting around `~` is awkward. For tilde paths we strip the `~/`
/// prefix so scp treats the path as relative to `$HOME` on the
/// remote (its default). Absolute paths pass through unchanged.
fn remote_scp_path(install_dir: &str, filename: &str) -> String {
    if let Some(rest) = install_dir.strip_prefix("~/") {
        format!("{}/{}", rest, filename)
    } else if install_dir == "~" {
        filename.to_string()
    } else {
        format!("{}/{}", install_dir, filename)
    }
}

async fn run_ssh(
    host: &str,
    ssh_args: &[String],
    script: &str,
    dry_run: bool,
) -> Result<(), BoxError> {
    println!("ssh {}: {}", host, script);
    if dry_run {
        return Ok(());
    }
    let mut args: Vec<String> = ssh_args.to_vec();
    args.push(host.to_string());
    args.push(script.to_string());
    let status = Command::new("ssh").args(&args).status().await?;
    if !status.success() {
        return Err(format!("ssh {} failed: {}", host, script).into());
    }
    Ok(())
}

async fn run_scp(
    ssh_args: &[String],
    src: &Path,
    dst: &str,
    dry_run: bool,
) -> Result<(), BoxError> {
    println!("scp {} -> {}", src.display(), dst);
    if dry_run {
        return Ok(());
    }
    let mut args: Vec<String> = ssh_args.to_vec();
    args.push(src.display().to_string());
    args.push(dst.to_string());
    let status = Command::new("scp").args(&args).status().await?;
    if !status.success() {
        return Err(format!("scp to {} failed", dst).into());
    }
    Ok(())
}

fn parse_symlinks(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn validate_shell_safe(field: &str, value: &str) -> Result<(), BoxError> {
    for c in value.chars() {
        if c.is_whitespace()
            || matches!(
                c,
                '\'' | '"'
                    | '`'
                    | '$'
                    | '\\'
                    | '|'
                    | '&'
                    | ';'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '*'
                    | '?'
                    | '#'
            )
        {
            warn!("rejecting unsafe character {:?} in --{}", c, field);
            return Err(format!(
                "--{} {:?} contains unsafe character {:?}; use only \
                 alphanumerics, `-`, `_`, `/`, `.`, and leading `~/`",
                field, value, c
            )
            .into());
        }
    }
    Ok(())
}
