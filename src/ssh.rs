use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::keys::{load_secret_key, PublicKey, PrivateKeyWithHashAlg};
use russh::{client, ChannelMsg, Disconnect};

struct ClientHandler;

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub struct SshSession {
    handle: client::Handle<ClientHandler>,
}

impl SshSession {
    pub async fn connect(
        host: &str,
        port: u16,
        user: &str,
        key_path: Option<&str>,
    ) -> anyhow::Result<Self> {
        let config = Arc::new(client::Config::default());
        let handler = ClientHandler;
        let mut session = client::connect(config, (host, port), handler).await?;

        // Try explicit key path, then default key paths
        let key_paths = if let Some(p) = key_path {
            vec![PathBuf::from(p)]
        } else {
            default_key_paths()
        };

        let mut authenticated = false;
        for path in &key_paths {
            if !path.exists() {
                continue;
            }
            match try_key_auth(&mut session, user, path).await {
                Ok(true) => {
                    authenticated = true;
                    log::info!("Authenticated to {}@{}:{} with {:?}", user, host, port, path);
                    break;
                }
                Ok(false) => {
                    log::debug!("Key {:?} rejected by server", path);
                }
                Err(e) => {
                    log::debug!("Key {:?} failed: {}", path, e);
                }
            }
        }

        if !authenticated {
            anyhow::bail!(
                "SSH authentication failed for {}@{}:{} (tried {} keys)",
                user, host, port, key_paths.len()
            );
        }

        Ok(Self { handle: session })
    }

    pub async fn run_command(&self, command: &str) -> anyhow::Result<String> {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut stdout = Vec::new();
        let mut exit_code: Option<u32> = None;

        let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let Some(msg) = channel.wait().await else {
                    break;
                };
                match msg {
                    ChannelMsg::Data { ref data } => {
                        stdout.extend_from_slice(data);
                    }
                    ChannelMsg::ExitStatus { exit_status } => {
                        exit_code = Some(exit_status);
                    }
                    ChannelMsg::Eof | ChannelMsg::Close => {
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;

        if result.is_err() {
            anyhow::bail!("SSH command timed out: {}", command);
        }

        if let Some(code) = exit_code {
            if code != 0 {
                let stderr_hint = String::from_utf8_lossy(&stdout);
                anyhow::bail!(
                    "Remote command exited with {}: {}",
                    code,
                    stderr_hint.chars().take(200).collect::<String>()
                );
            }
        }

        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    pub async fn close(&self) -> anyhow::Result<()> {
        self.handle
            .disconnect(Disconnect::ByApplication, "", "")
            .await?;
        Ok(())
    }
}

async fn try_key_auth(
    session: &mut client::Handle<ClientHandler>,
    user: &str,
    key_path: &Path,
) -> anyhow::Result<bool> {
    let key_pair = load_secret_key(key_path, None)?;
    let hash_alg = session.best_supported_rsa_hash().await?.flatten();
    let auth_res = session
        .authenticate_publickey(
            user,
            PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash_alg),
        )
        .await?;
    Ok(auth_res.success())
}

fn default_key_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        let ssh_dir = home.join(".ssh");
        paths.push(ssh_dir.join("id_ed25519"));
        paths.push(ssh_dir.join("id_rsa"));
        paths.push(ssh_dir.join("id_ecdsa"));
    }
    paths
}
