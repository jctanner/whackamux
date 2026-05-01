use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::pane::{PaneGeometry, PaneInfo, PaneStatus, WindowInfo};
use crate::ssh::SshSession;

pub enum TmuxRunner {
    Local,
    Remote(Arc<Mutex<SshSession>>),
}

const LIST_PANES_FMT: &str = "#{session_name}\t#{window_id}\t#{window_index}\t#{window_name}\t#{window_width}\t#{window_height}\t#{pane_id}\t#{pane_index}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}\t#{pane_current_command}\t#{pane_pid}";

impl TmuxRunner {
    fn run_local(args: &[&str]) -> anyhow::Result<String> {
        let output = Command::new("tmux").args(args).output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("tmux {} failed: {}", args[0], stderr);
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn run_remote(session: &Arc<Mutex<SshSession>>, args: &[&str]) -> anyhow::Result<String> {
        // Quote arguments that contain special characters for the remote shell
        let quoted_args: Vec<String> = args
            .iter()
            .map(|a| {
                if a.contains(|c: char| c.is_whitespace() || c == '#' || c == '{' || c == '}') {
                    format!("'{}'", a)
                } else {
                    a.to_string()
                }
            })
            .collect();
        let cmd = format!("tmux {}", quoted_args.join(" "));
        let session = session.lock().await;
        session.run_command(&cmd).await
    }

    pub async fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        match self {
            TmuxRunner::Local => Self::run_local(args),
            TmuxRunner::Remote(session) => Self::run_remote(session, args).await,
        }
    }
}

pub async fn discover_windows(
    runner: &TmuxRunner,
    host_name: &str,
    attention_patterns: &[String],
) -> anyhow::Result<Vec<WindowInfo>> {
    let fmt_arg = format!("-F{}", LIST_PANES_FMT);
    let stdout = runner.run(&["list-panes", "-a", &fmt_arg]).await?;

    let mut window_map: HashMap<String, (String, u32, String, u32, u32, Vec<PaneInfo>)> =
        HashMap::new();

    struct ParsedPane {
        session: String,
        window_id: String,
        window_index: u32,
        window_name: String,
        window_width: u32,
        window_height: u32,
        pane_id: String,
        pane_index: u32,
        pane_left: u32,
        pane_top: u32,
        pane_width: u32,
        pane_height: u32,
        current_command: String,
    }

    let mut parsed: Vec<ParsedPane> = Vec::new();
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 14 {
            continue;
        }
        parsed.push(ParsedPane {
            session: fields[0].to_string(),
            window_id: fields[1].to_string(),
            window_index: fields[2].parse().unwrap_or(0),
            window_name: fields[3].to_string(),
            window_width: fields[4].parse().unwrap_or(200),
            window_height: fields[5].parse().unwrap_or(50),
            pane_id: fields[6].to_string(),
            pane_index: fields[7].parse().unwrap_or(0),
            pane_left: fields[8].parse().unwrap_or(0),
            pane_top: fields[9].parse().unwrap_or(0),
            pane_width: fields[10].parse().unwrap_or(80),
            pane_height: fields[11].parse().unwrap_or(24),
            current_command: fields[12].to_string(),
        });
    }

    let capture_futures: Vec<_> = parsed
        .iter()
        .map(|p| capture_pane(runner, &p.pane_id))
        .collect();
    let capture_results = futures::future::join_all(capture_futures).await;

    for (p, content_result) in parsed.iter().zip(capture_results) {
        let content = content_result.unwrap_or_default();
        let status = detect_status(&content, &p.current_command, attention_patterns);

        let pane = PaneInfo {
            id: p.pane_id.clone(),
            index: p.pane_index,
            geometry: PaneGeometry {
                left: p.pane_left,
                top: p.pane_top,
                width: p.pane_width,
                height: p.pane_height,
            },
            status,
            content,
        };

        window_map
            .entry(p.window_id.clone())
            .or_insert_with(|| {
                (
                    p.session.clone(),
                    p.window_index,
                    p.window_name.clone(),
                    p.window_width,
                    p.window_height,
                    Vec::new(),
                )
            })
            .5
            .push(pane);
    }

    let mut windows: Vec<WindowInfo> = window_map
        .into_iter()
        .map(|(id, (session, window_index, window_name, width, height, panes))| {
            WindowInfo {
                id,
                host: host_name.to_string(),
                session,
                window_index,
                window_name,
                width,
                height,
                panes,
            }
        })
        .collect();

    windows.sort_by(|a, b| {
        a.session
            .cmp(&b.session)
            .then(a.window_index.cmp(&b.window_index))
    });

    Ok(windows)
}

async fn capture_pane(runner: &TmuxRunner, pane_id: &str) -> anyhow::Result<Vec<String>> {
    let stdout = runner.run(&["capture-pane", "-t", pane_id, "-p"]).await?;
    let lines: Vec<String> = stdout.lines().map(|l| l.to_string()).collect();
    let last_non_empty = lines.iter().rposition(|l| !l.is_empty()).unwrap_or(0);
    Ok(lines[..=last_non_empty].to_vec())
}

fn detect_status(
    content: &[String],
    current_command: &str,
    attention_patterns: &[String],
) -> PaneStatus {
    let check_lines = content.iter().rev().take(15);
    for line in check_lines {
        for pattern in attention_patterns {
            if line.contains(pattern.as_str()) {
                return PaneStatus::NeedsAttention;
            }
        }
    }

    match current_command {
        "bash" | "zsh" | "fish" | "sh" | "dash" | "tcsh" | "csh" => PaneStatus::Idle,
        _ => PaneStatus::Active,
    }
}

pub async fn send_keys(runner: &TmuxRunner, pane_id: &str, keys: &str) -> anyhow::Result<()> {
    let parts: Vec<&str> = keys.split('\n').collect();
    for (i, part) in parts.iter().enumerate() {
        if !part.is_empty() {
            runner.run(&["send-keys", "-t", pane_id, "-l", part]).await?;
        }
        if i < parts.len() - 1 {
            runner.run(&["send-keys", "-t", pane_id, "Enter"]).await?;
        }
    }
    Ok(())
}

pub async fn send_key_name(runner: &TmuxRunner, pane_id: &str, key_name: &str) -> anyhow::Result<()> {
    runner.run(&["send-keys", "-t", pane_id, key_name]).await?;
    Ok(())
}

pub async fn send_literal(runner: &TmuxRunner, pane_id: &str, text: &str) -> anyhow::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    runner.run(&["send-keys", "-t", pane_id, "-l", text]).await?;
    Ok(())
}

pub async fn run_command(runner: &TmuxRunner, args: &[&str]) -> anyhow::Result<()> {
    runner.run(args).await?;
    Ok(())
}
