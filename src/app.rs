use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{Config, HostConfig, QuickAction};
use crate::pane::{PaneInfo, PaneStatus, WindowInfo};
use crate::ssh::SshSession;
use crate::tmux::{self, TmuxRunner};
use egui::StrokeKind;

type PollResult = (Vec<WindowInfo>, HashMap<String, String>);

pub struct App {
    windows: Vec<WindowInfo>,
    runners: HashMap<String, Arc<TmuxRunner>>,
    quick_actions: Vec<QuickAction>,
    attention_patterns: Vec<String>,
    host_configs: Vec<HostConfig>,
    poll_interval: Duration,
    last_poll: Instant,
    runtime: tokio::runtime::Runtime,
    focused_pane: Option<String>,
    filter_host: Option<String>,
    show_only_attention: bool,
    connection_errors: HashMap<String, String>,
    poll_rx: Option<tokio::sync::oneshot::Receiver<PollResult>>,
    poll_in_flight: bool,
    hidden_windows: std::collections::HashSet<String>,
    tmux_prefix_pending: bool,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>, config: Config) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        let mut runners: HashMap<String, Arc<TmuxRunner>> = HashMap::new();

        // Create runners for each configured host
        for host in &config.hosts {
            if host.local {
                runners.insert(host.name.clone(), Arc::new(TmuxRunner::Local));
            }
            // Remote runners are created lazily on first poll
        }

        let attention_patterns = config.attention_patterns.clone();
        let host_configs = config.hosts.clone();

        // Initial discovery (local only, remotes connect on first poll)
        let mut windows = Vec::new();
        for host in &config.hosts {
            if host.local {
                if let Some(runner) = runners.get(&host.name) {
                    match runtime.block_on(tmux::discover_windows(
                        runner,
                        &host.name,
                        &attention_patterns,
                    )) {
                        Ok(w) => windows.extend(w),
                        Err(e) => log::warn!("Initial discovery for {} failed: {}", host.name, e),
                    }
                }
            }
        }

        Self {
            windows,
            runners,
            quick_actions: config.quick_actions,
            attention_patterns,
            host_configs,
            poll_interval: Duration::from_secs(config.poll_interval_secs),
            last_poll: Instant::now(),
            runtime,
            focused_pane: None,
            filter_host: None,
            show_only_attention: false,
            connection_errors: HashMap::new(),
            poll_rx: None,
            poll_in_flight: false,
            hidden_windows: std::collections::HashSet::new(),
            tmux_prefix_pending: false,
        }
    }

    fn hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self.windows.iter().map(|w| w.host.clone()).collect();
        // Also include configured hosts that might not have windows yet
        for h in &self.host_configs {
            if !hosts.contains(&h.name) {
                hosts.push(h.name.clone());
            }
        }
        hosts.sort();
        hosts.dedup();
        hosts
    }

    fn window_key(w: &WindowInfo) -> String {
        make_pane_key(&w.host, &w.id)
    }

    fn filtered_window_indices(&self) -> Vec<usize> {
        self.windows
            .iter()
            .enumerate()
            .filter(|(_, w)| {
                if self.hidden_windows.contains(&Self::window_key(w)) {
                    return false;
                }
                if let Some(ref host) = self.filter_host {
                    if &w.host != host {
                        return false;
                    }
                }
                if self.show_only_attention && w.attention_count() == 0 {
                    return false;
                }
                true
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn total_attention(&self) -> usize {
        self.windows.iter().map(|w| w.attention_count()).sum()
    }

    fn total_panes(&self) -> usize {
        self.windows.iter().map(|w| w.panes.len()).sum()
    }

    fn runner_for_pane(&self, focused_key: &str) -> Option<(Arc<TmuxRunner>, String)> {
        let (host, pane_id) = split_pane_key(focused_key)?;
        let runner = self.runners.get(host)?.clone();
        Some((runner, pane_id.to_string()))
    }

    fn ensure_remote_connections(&mut self) {
        for host in &self.host_configs {
            if host.local || self.runners.contains_key(&host.name) {
                continue;
            }
            if let Some(ref ssh_spec) = host.ssh {
                let (user, addr) = parse_ssh_spec(ssh_spec);
                let port = host.port;
                let key = host.key.clone();
                let host_name = host.name.clone();
                match self.runtime.block_on(SshSession::connect(
                    &addr,
                    port,
                    &user,
                    key.as_deref(),
                )) {
                    Ok(session) => {
                        log::info!("SSH connected to {}", host_name);
                        let runner = TmuxRunner::Remote(Arc::new(tokio::sync::Mutex::new(session)));
                        self.runners.insert(host_name.clone(), Arc::new(runner));
                        self.connection_errors.remove(&host_name);
                    }
                    Err(e) => {
                        let msg = format!("{}", e);
                        if self.connection_errors.get(&host_name).map(|s| s.as_str()) != Some(&msg) {
                            log::warn!("SSH connection to {} failed: {}", host_name, msg);
                            self.connection_errors.insert(host_name, msg);
                        }
                    }
                }
            }
        }
    }

    fn start_poll(&mut self) {
        if self.poll_in_flight {
            return;
        }
        self.ensure_remote_connections();

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.poll_rx = Some(rx);
        self.poll_in_flight = true;

        let hosts: Vec<(String, Arc<TmuxRunner>, bool)> = self
            .host_configs
            .iter()
            .filter_map(|h| {
                self.runners
                    .get(&h.name)
                    .map(|r| (h.name.clone(), r.clone(), h.local))
            })
            .collect();
        let patterns = self.attention_patterns.clone();

        self.runtime.spawn(async move {
            let mut all_windows = Vec::new();
            let mut errors: HashMap<String, String> = HashMap::new();

            let futs: Vec<_> = hosts
                .iter()
                .map(|(name, runner, _)| {
                    let name = name.clone();
                    let runner = runner.clone();
                    let patterns = patterns.clone();
                    async move {
                        let result = tmux::discover_windows(&runner, &name, &patterns).await;
                        (name, result)
                    }
                })
                .collect();

            let results = futures::future::join_all(futs).await;

            for (name, result) in results {
                match result {
                    Ok(w) => all_windows.extend(w),
                    Err(e) => {
                        log::warn!("Discovery for {} failed: {}", name, e);
                        errors.insert(name, format!("{}", e));
                    }
                }
            }

            let _ = tx.send((all_windows, errors));
        });
    }

    fn check_poll_results(&mut self) {
        if let Some(mut rx) = self.poll_rx.take() {
            match rx.try_recv() {
                Ok((windows, errors)) => {
                    self.windows = windows;
                    for (name, _) in &errors {
                        let is_local = self.host_configs.iter().any(|h| h.name == *name && h.local);
                        if !is_local {
                            self.runners.remove(name);
                        }
                    }
                    for (name, msg) in errors {
                        self.connection_errors.insert(name, msg);
                    }
                    self.poll_in_flight = false;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    self.poll_rx = Some(rx);
                }
                Err(_) => {
                    self.poll_in_flight = false;
                }
            }
        }
    }
}

fn make_pane_key(host: &str, pane_id: &str) -> String {
    format!("{}\t{}", host, pane_id)
}

fn split_pane_key(key: &str) -> Option<(&str, &str)> {
    key.split_once('\t')
}

fn parse_ssh_spec(spec: &str) -> (String, String) {
    if let Some((user, host)) = spec.split_once('@') {
        (user.to_string(), host.to_string())
    } else {
        ("root".to_string(), spec.to_string())
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Check for completed background poll
        self.check_poll_results();

        // Start a new poll if interval has elapsed and none in flight
        if self.last_poll.elapsed() >= self.poll_interval && !self.poll_in_flight {
            self.last_poll = Instant::now();
            self.start_poll();
        }

        ctx.request_repaint_after(Duration::from_millis(100));

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("whackamux");
                ui.separator();

                let attention = self.total_attention();
                if attention > 0 {
                    let label = format!("{} needs attention", attention);
                    let text = egui::RichText::new(label)
                        .color(egui::Color32::from_rgb(220, 50, 50))
                        .strong();
                    ui.label(text);
                    ui.separator();
                }

                if ui
                    .selectable_label(self.filter_host.is_none(), "All")
                    .clicked()
                {
                    self.filter_host = None;
                }
                for host in self.hosts() {
                    let selected = self.filter_host.as_ref() == Some(&host);
                    let has_error = self.connection_errors.contains_key(&host);
                    let label_text = if has_error {
                        format!("{} !", host)
                    } else {
                        host.clone()
                    };
                    if ui.selectable_label(selected, &label_text).clicked() {
                        self.filter_host = if selected { None } else { Some(host) };
                    }
                }

                ui.separator();

                let attn_label = if self.show_only_attention {
                    "Showing: attention only"
                } else {
                    "Showing: all"
                };
                if ui
                    .selectable_label(self.show_only_attention, attn_label)
                    .clicked()
                {
                    self.show_only_attention = !self.show_only_attention;
                }

                if !self.hidden_windows.is_empty() {
                    let unhide_label = format!("{} hidden", self.hidden_windows.len());
                    if ui.button(&unhide_label).clicked() {
                        self.hidden_windows.clear();
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!(
                        "{} windows / {} panes",
                        self.windows.len(),
                        self.total_panes()
                    ));
                    if self.tmux_prefix_pending {
                        ui.separator();
                        let prefix_label = egui::RichText::new("PREFIX")
                            .color(egui::Color32::from_rgb(255, 200, 50))
                            .strong();
                        ui.label(prefix_label);
                    }
                    if let Some(ref pane_key) = self.focused_pane {
                        if let Some((host, pane_id)) = split_pane_key(pane_key) {
                            ui.separator();
                            let input_label = egui::RichText::new(format!("INPUT: {}:{}", host, pane_id))
                                .color(egui::Color32::from_rgb(80, 180, 255))
                                .strong();
                            ui.label(input_label);
                        }
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let indices = self.filtered_window_indices();
            if indices.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No windows to display");
                });
                return;
            }

            let display_windows: Vec<WindowInfo> =
                indices.iter().map(|&idx| self.windows[idx].clone()).collect();

            let available = ui.available_size();
            let count = display_windows.len();
            let cols = compute_columns(count, available.x, available.y);
            let rows = (count + cols - 1) / cols;

            let spacing = 8.0;
            let tile_width = (available.x - spacing * (cols as f32 + 1.0)) / cols as f32;
            let tile_height = (available.y - spacing * (rows as f32 + 1.0)) / rows as f32;

            let mut pending_actions: Vec<(String, String)> = Vec::new();
            let mut hide_requests: Vec<String> = Vec::new();

            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("window_grid")
                    .num_columns(cols)
                    .spacing([spacing, spacing])
                    .show(ui, |ui| {
                        for (i, window) in display_windows.iter().enumerate() {
                            draw_window_tile(
                                ui,
                                window,
                                tile_width,
                                tile_height,
                                &self.quick_actions,
                                &mut self.focused_pane,
                                &mut pending_actions,
                                &mut hide_requests,
                            );
                            if (i + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                    });
            });

            for key in hide_requests {
                self.hidden_windows.insert(key);
            }

            for (pane_key, keys) in pending_actions {
                if let Some((runner, pane_id)) = self.runner_for_pane(&pane_key) {
                    log::info!("Sending keys to pane {}: {:?}", pane_id, keys);
                    self.runtime.spawn(async move {
                        if let Err(e) = tmux::send_keys(&runner, &pane_id, &keys).await {
                            log::error!("send-keys failed: {}", e);
                        }
                    });
                }
                self.last_poll = Instant::now() - self.poll_interval - Duration::from_millis(1);
            }
        });

        // Keyboard input forwarding to focused pane
        if let Some(ref pane_key) = self.focused_pane.clone() {
            if let Some((runner, pane_id)) = self.runner_for_pane(pane_key) {
                let mut sent_input = false;
                let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());

                for event in &events {
                    match event {
                        egui::Event::Key {
                            key,
                            pressed: true,
                            modifiers,
                            ..
                        } if modifiers.ctrl && *key == egui::Key::B => {
                            self.tmux_prefix_pending = true;
                            continue;
                        }
                        _ => {}
                    }

                    if self.tmux_prefix_pending {
                        let prefix_char = match event {
                            egui::Event::Text(t) => Some(t.clone()),
                            egui::Event::Key { key, pressed: true, .. } => {
                                match key {
                                    egui::Key::ArrowUp => Some("Up".into()),
                                    egui::Key::ArrowDown => Some("Down".into()),
                                    egui::Key::ArrowLeft => Some("Left".into()),
                                    egui::Key::ArrowRight => Some("Right".into()),
                                    _ => None,
                                }
                            }
                            _ => None,
                        };
                        if let Some(ch) = prefix_char {
                            self.tmux_prefix_pending = false;
                            if let Some(args) = map_prefix_to_tmux_cmd(&ch, &pane_id) {
                                let runner = runner.clone();
                                self.runtime.spawn(async move {
                                    let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                                    if let Err(e) = tmux::run_command(&runner, &str_args).await {
                                        log::error!("tmux prefix cmd failed: {}", e);
                                    }
                                });
                                sent_input = true;
                            }
                        }
                        continue;
                    }

                    match event {
                        egui::Event::Text(text) => {
                            let runner = runner.clone();
                            let pane_id = pane_id.clone();
                            let text = text.clone();
                            self.runtime.spawn(async move {
                                if let Err(e) = tmux::send_literal(&runner, &pane_id, &text).await {
                                    log::error!("send literal failed: {}", e);
                                }
                            });
                            sent_input = true;
                        }
                        egui::Event::Key {
                            key,
                            pressed: true,
                            modifiers,
                            ..
                        } => {
                            if let Some(key_name) = map_key_to_tmux(*key, modifiers) {
                                let runner = runner.clone();
                                let pane_id = pane_id.clone();
                                self.runtime.spawn(async move {
                                    if let Err(e) = tmux::send_key_name(&runner, &pane_id, &key_name).await {
                                        log::error!("send key '{}' failed: {}", key_name, e);
                                    }
                                });
                                sent_input = true;
                            }
                        }
                        _ => {}
                    }
                }

                if sent_input {
                    self.last_poll = Instant::now() - self.poll_interval - Duration::from_millis(1);
                }
            }
        }
    }
}

fn map_prefix_to_tmux_cmd(ch: &str, pane_id: &str) -> Option<Vec<String>> {
    let args: Vec<String> = match ch {
        "c" => vec!["new-window".into()],
        "n" => vec!["next-window".into()],
        "p" => vec!["previous-window".into()],
        "l" => vec!["last-window".into()],
        "w" => vec!["choose-tree".into(), "-t".into(), pane_id.into()],
        "\"" => vec!["split-window".into(), "-t".into(), pane_id.into()],
        "%" => vec!["split-window".into(), "-h".into(), "-t".into(), pane_id.into()],
        "x" => vec!["kill-pane".into(), "-t".into(), pane_id.into()],
        "z" => vec!["resize-pane".into(), "-Z".into(), "-t".into(), pane_id.into()],
        "," => vec!["command-prompt".into(), "-t".into(), pane_id.into(), "-I".into(), "#W".into(), "rename-window -- '%%'".into()],
        "[" => vec!["copy-mode".into(), "-t".into(), pane_id.into()],
        "o" => vec!["select-pane".into(), "-t".into(), format!("{}.+", pane_id)],
        ";" => vec!["last-pane".into()],
        "Up" => vec!["select-pane".into(), "-U".into(), "-t".into(), pane_id.into()],
        "Down" => vec!["select-pane".into(), "-D".into(), "-t".into(), pane_id.into()],
        "Left" => vec!["select-pane".into(), "-L".into(), "-t".into(), pane_id.into()],
        "Right" => vec!["select-pane".into(), "-R".into(), "-t".into(), pane_id.into()],
        d if d.len() == 1 && d.chars().next().unwrap().is_ascii_digit() => {
            vec!["select-window".into(), "-t".into(), format!(":{}", d)]
        }
        _ => return None,
    };
    Some(args)
}

fn map_key_to_tmux(key: egui::Key, modifiers: &egui::Modifiers) -> Option<String> {
    if modifiers.ctrl {
        let ctrl_char = match key {
            egui::Key::A => "a",
            egui::Key::B => "b",
            egui::Key::C => "c",
            egui::Key::D => "d",
            egui::Key::E => "e",
            egui::Key::F => "f",
            egui::Key::G => "g",
            egui::Key::H => "h",
            egui::Key::I => "i",
            egui::Key::J => "j",
            egui::Key::K => "k",
            egui::Key::L => "l",
            egui::Key::M => "m",
            egui::Key::N => "n",
            egui::Key::O => "o",
            egui::Key::P => "p",
            egui::Key::Q => "q",
            egui::Key::R => "r",
            egui::Key::S => "s",
            egui::Key::T => "t",
            egui::Key::U => "u",
            egui::Key::V => "v",
            egui::Key::W => "w",
            egui::Key::X => "x",
            egui::Key::Y => "y",
            egui::Key::Z => "z",
            _ => return None,
        };
        return Some(format!("C-{}", ctrl_char));
    }

    match key {
        egui::Key::Enter => Some("Enter".into()),
        egui::Key::Backspace => Some("BSpace".into()),
        egui::Key::Tab => Some("Tab".into()),
        egui::Key::Escape => Some("Escape".into()),
        egui::Key::ArrowUp => Some("Up".into()),
        egui::Key::ArrowDown => Some("Down".into()),
        egui::Key::ArrowLeft => Some("Left".into()),
        egui::Key::ArrowRight => Some("Right".into()),
        egui::Key::Delete => Some("DC".into()),
        egui::Key::Home => Some("Home".into()),
        egui::Key::End => Some("End".into()),
        egui::Key::PageUp => Some("PPage".into()),
        egui::Key::PageDown => Some("NPage".into()),
        _ => None,
    }
}

fn compute_columns(count: usize, width: f32, height: f32) -> usize {
    if count <= 1 {
        return 1;
    }
    if count <= 4 {
        return 2;
    }
    if count <= 9 {
        return 3;
    }
    let aspect = width / height;
    let cols = ((count as f32).sqrt() * aspect.sqrt()).ceil() as usize;
    cols.max(2).min(count)
}

fn draw_window_tile(
    ui: &mut egui::Ui,
    window: &WindowInfo,
    width: f32,
    height: f32,
    quick_actions: &[QuickAction],
    focused_pane: &mut Option<String>,
    pending_actions: &mut Vec<(String, String)>,
    hide_requests: &mut Vec<String>,
) {
    let header_height = 20.0;
    let border_status = window.worst_status();
    let border_color = border_status.border_color();

    let (rect, _response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());

    let bg = egui::Color32::from_rgb(18, 18, 22);
    ui.painter().rect_filled(rect, 4.0, bg);
    ui.painter().rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(2.0, border_color),
        StrokeKind::Outside,
    );

    let header_rect = egui::Rect::from_min_size(rect.min, egui::vec2(width, header_height));
    let header_bg = egui::Color32::from_rgb(30, 30, 38);
    ui.painter().rect_filled(
        header_rect,
        egui::CornerRadius {
            nw: 4,
            ne: 4,
            sw: 0,
            se: 0,
        },
        header_bg,
    );

    let label = window.display_label();
    ui.painter().text(
        header_rect.min + egui::vec2(6.0, 3.0),
        egui::Align2::LEFT_TOP,
        &label,
        egui::FontId::proportional(11.0),
        egui::Color32::from_rgb(180, 180, 200),
    );

    let close_size = 14.0;
    let close_rect = egui::Rect::from_min_size(
        egui::pos2(header_rect.max.x - close_size - 3.0, header_rect.min.y + 3.0),
        egui::vec2(close_size, close_size),
    );
    let close_id = egui::Id::new(&window.host).with(&window.id).with("close");
    let close_response = ui.interact(close_rect, close_id, egui::Sense::click());
    let close_color = if close_response.hovered() {
        egui::Color32::from_rgb(220, 80, 80)
    } else {
        egui::Color32::from_rgb(100, 100, 120)
    };
    ui.painter().text(
        close_rect.center(),
        egui::Align2::CENTER_CENTER,
        "x",
        egui::FontId::proportional(10.0),
        close_color,
    );
    if close_response.clicked() {
        hide_requests.push(make_pane_key(&window.host, &window.id));
    }

    if window.panes.len() > 1 {
        let pane_count = format!("{} panes", window.panes.len());
        ui.painter().text(
            egui::pos2(header_rect.max.x - close_size - 10.0, header_rect.min.y + 3.0),
            egui::Align2::RIGHT_TOP,
            &pane_count,
            egui::FontId::proportional(9.0),
            egui::Color32::from_rgb(120, 120, 140),
        );
    }

    let content_rect = egui::Rect::from_min_max(
        egui::pos2(rect.min.x + 1.0, rect.min.y + header_height),
        egui::pos2(rect.max.x - 1.0, rect.max.y - 1.0),
    );

    let win_w = window.width as f32;
    let win_h = window.height as f32;

    for pane in &window.panes {
        let px = pane.geometry.left as f32 / win_w;
        let py = pane.geometry.top as f32 / win_h;
        let pw = pane.geometry.width as f32 / win_w;
        let ph = pane.geometry.height as f32 / win_h;

        let pane_rect = egui::Rect::from_min_size(
            egui::pos2(
                content_rect.min.x + px * content_rect.width(),
                content_rect.min.y + py * content_rect.height(),
            ),
            egui::vec2(pw * content_rect.width(), ph * content_rect.height()),
        );

        draw_pane_in_tile(ui, pane, pane_rect, &window.host, quick_actions, focused_pane, pending_actions);
    }
}

fn draw_pane_in_tile(
    ui: &mut egui::Ui,
    pane: &PaneInfo,
    rect: egui::Rect,
    host: &str,
    quick_actions: &[QuickAction],
    focused_pane: &mut Option<String>,
    pending_actions: &mut Vec<(String, String)>,
) {
    let pane_key = make_pane_key(host, &pane.id);
    let is_focused = focused_pane.as_ref() == Some(&pane_key);
    let pane_border_color = pane.status.border_color();

    let pane_bg = if is_focused {
        egui::Color32::from_rgb(25, 25, 35)
    } else {
        egui::Color32::from_rgb(10, 10, 15)
    };
    ui.painter().rect_filled(rect, 0.0, pane_bg);

    let indicator_width = 3.0;
    let indicator_rect =
        egui::Rect::from_min_size(rect.min, egui::vec2(indicator_width, rect.height()));
    ui.painter()
        .rect_filled(indicator_rect, 0.0, pane_border_color);

    let (border_width, border_color) = if is_focused {
        (2.0, egui::Color32::from_rgb(80, 180, 255))
    } else {
        (0.5, egui::Color32::from_rgb(50, 50, 60))
    };
    ui.painter().rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(border_width, border_color),
        StrokeKind::Inside,
    );

    let pane_interact_id = egui::Id::new(&pane_key).with("click");
    let click_rect = ui.interact(rect, pane_interact_id, egui::Sense::click());
    if click_rect.clicked() {
        *focused_pane = if is_focused {
            None
        } else {
            Some(pane_key.clone())
        };
    }

    let text_rect = rect.shrink2(egui::vec2(indicator_width + 4.0, 2.0));
    let text_left = text_rect.min.x + 2.0;
    let clipped = ui.painter().with_clip_rect(rect);

    if is_focused {
        clipped.text(
            egui::pos2(rect.max.x - 4.0, rect.min.y + 2.0),
            egui::Align2::RIGHT_TOP,
            "INPUT",
            egui::FontId::proportional(8.0),
            egui::Color32::from_rgb(80, 180, 255),
        );
    } else if pane.status == PaneStatus::NeedsAttention {
        clipped.text(
            egui::pos2(rect.max.x - 4.0, rect.min.y + 2.0),
            egui::Align2::RIGHT_TOP,
            pane.status.label(),
            egui::FontId::proportional(8.0),
            pane_border_color,
        );
    }

    let font = egui::FontId::monospace(9.0);
    let line_height = 11.0;
    let plain_lines = pane.plain_content();
    let max_lines = ((text_rect.height() - 16.0) / line_height).max(0.0) as usize;
    let char_width = 5.4;
    let max_chars = ((text_rect.width()) / char_width) as usize;

    let skip = plain_lines.len().saturating_sub(max_lines);
    for (line_idx, line) in plain_lines.iter().skip(skip).take(max_lines).enumerate() {
        let y = text_rect.min.y + 2.0 + line_idx as f32 * line_height;
        if y + line_height > text_rect.max.y - 14.0 {
            break;
        }
        let truncated: String = if line.is_empty() {
            " ".into()
        } else if line.chars().count() > max_chars {
            line.chars().take(max_chars).collect()
        } else {
            line.clone()
        };
        clipped.text(
            egui::pos2(text_left, y),
            egui::Align2::LEFT_TOP,
            &truncated,
            font.clone(),
            egui::Color32::from_rgb(190, 190, 190),
        );
    }

    if pane.status == PaneStatus::NeedsAttention && rect.height() > 40.0 {
        let btn_y = rect.max.y - 16.0;
        let mut btn_x = text_left;
        for action in quick_actions {
            let btn_width = 28.0 + action.label.len() as f32 * 4.0;
            let btn_rect = egui::Rect::from_min_size(
                egui::pos2(btn_x, btn_y),
                egui::vec2(btn_width, 14.0),
            );

            if btn_x + btn_width > rect.max.x - 4.0 {
                break;
            }

            let btn_id = egui::Id::new(&pane_key).with("btn").with(&action.label);
            let btn_response = ui.interact(btn_rect, btn_id, egui::Sense::click());
            let btn_bg = if btn_response.hovered() {
                egui::Color32::from_rgb(60, 60, 80)
            } else {
                egui::Color32::from_rgb(40, 40, 55)
            };
            clipped.rect_filled(btn_rect, 2.0, btn_bg);
            clipped.rect_stroke(
                btn_rect,
                2.0,
                egui::Stroke::new(0.5, egui::Color32::from_rgb(80, 80, 100)),
                StrokeKind::Inside,
            );
            clipped.text(
                btn_rect.center(),
                egui::Align2::CENTER_CENTER,
                &action.label,
                egui::FontId::proportional(9.0),
                egui::Color32::from_rgb(180, 180, 200),
            );

            if btn_response.clicked() {
                pending_actions.push((pane_key.clone(), action.keys.clone()));
            }

            btn_x += btn_width + 4.0;
        }
    }
}
