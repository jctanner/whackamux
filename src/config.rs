use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub hosts: Vec<HostConfig>,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_attention_patterns")]
    pub attention_patterns: Vec<String>,
    #[serde(default = "default_quick_actions")]
    pub quick_actions: Vec<QuickAction>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HostConfig {
    pub name: String,
    pub ssh: Option<String>,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub local: bool,
    pub key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuickAction {
    pub label: String,
    pub keys: String,
}

fn default_poll_interval() -> u64 {
    2
}

fn default_port() -> u16 {
    22
}

fn default_attention_patterns() -> Vec<String> {
    vec![
        "Do you want to".into(),
        "Allow".into(),
        "yes/no".into(),
        "Permission".into(),
        "Press enter".into(),
    ]
}

fn default_quick_actions() -> Vec<QuickAction> {
    vec![
        QuickAction { label: "yes".into(), keys: "yes\n".into() },
        QuickAction { label: "y".into(), keys: "y\n".into() },
        QuickAction { label: "enter".into(), keys: "\n".into() },
    ]
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&content)?;
        Ok(config)
    }
}
