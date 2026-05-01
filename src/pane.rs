#[derive(Debug, Clone, PartialEq)]
pub enum PaneStatus {
    Active,
    Idle,
    NeedsAttention,
    Disconnected,
}

impl PaneStatus {
    pub fn border_color(&self) -> egui::Color32 {
        match self {
            PaneStatus::NeedsAttention => egui::Color32::from_rgb(220, 50, 50),
            PaneStatus::Active => egui::Color32::from_rgb(50, 180, 50),
            PaneStatus::Idle => egui::Color32::from_rgb(100, 100, 100),
            PaneStatus::Disconnected => egui::Color32::from_rgb(180, 100, 30),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            PaneStatus::NeedsAttention => "WAITING",
            PaneStatus::Active => "ACTIVE",
            PaneStatus::Idle => "IDLE",
            PaneStatus::Disconnected => "DISCONNECTED",
        }
    }
}

/// Geometry of a pane within its parent window.
/// Values are in tmux cell coordinates (columns/rows).
/// Used to compute proportional positioning inside the window tile.
#[derive(Debug, Clone)]
pub struct PaneGeometry {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub id: String,
    pub index: u32,
    pub geometry: PaneGeometry,
    pub status: PaneStatus,
    pub content: Vec<String>,
}

impl PaneInfo {
    pub fn plain_content(&self) -> Vec<String> {
        self.content.iter().map(|line| strip_ansi(line)).collect()
    }
}

/// A tmux window containing one or more panes in a split layout.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: String,
    pub host: String,
    pub session: String,
    pub window_index: u32,
    pub window_name: String,
    pub width: u32,
    pub height: u32,
    pub panes: Vec<PaneInfo>,
}

impl WindowInfo {
    pub fn display_label(&self) -> String {
        format!("{} | {}:{}", self.host, self.session, self.window_name)
    }

    pub fn worst_status(&self) -> &PaneStatus {
        if self.panes.iter().any(|p| p.status == PaneStatus::NeedsAttention) {
            return &PaneStatus::NeedsAttention;
        }
        if self.panes.iter().any(|p| p.status == PaneStatus::Active) {
            return &PaneStatus::Active;
        }
        if self.panes.iter().any(|p| p.status == PaneStatus::Disconnected) {
            return &PaneStatus::Disconnected;
        }
        &PaneStatus::Idle
    }

    pub fn attention_count(&self) -> usize {
        self.panes.iter().filter(|p| p.status == PaneStatus::NeedsAttention).count()
    }
}

fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(next) = chars.next() {
                if next == '[' {
                    // CSI sequence: skip until final byte (@ through ~)
                    for c2 in chars.by_ref() {
                        if c2 >= '@' && c2 <= '~' {
                            break;
                        }
                    }
                }
                // OSC/other escapes: single char consumed, continue
            }
            continue;
        }
        result.push(c);
    }
    result
}
