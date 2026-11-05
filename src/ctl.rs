//! Управление по Unix-сокету (`spread-ctl`) и командам Telegram. Протокол — одна строка
//! JSON запроса и одна строка JSON ответа. Сокет создаётся с правами 600; доступ к нему
//! извне — только через WireGuard/SSH (SPEC.md, «Build and deployment»).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::engine::OperatorCommand;

/// Запрос к демону.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CtlRequest {
    /// Состояние движка.
    Status,
    /// Команда оператора.
    Command(OperatorCommand),
}

/// Ответ демона.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CtlResponse {
    /// Успех.
    pub ok: bool,
    /// Текст или JSON состояния.
    pub body: serde_json::Value,
}

/// Запрос с каналом ответа.
pub type CtlEnvelope = (CtlRequest, oneshot::Sender<CtlResponse>);

/// Слушает сокет и пересылает запросы в цикл движка.
pub async fn serve(path: PathBuf, tx: mpsc::Sender<CtlEnvelope>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // старый сокет от прошлого запуска
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(socket = %path.display(), "control socket listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let tx = tx.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let resp = match serde_json::from_str::<CtlRequest>(&line) {
                    Ok(req) => {
                        let (otx, orx) = oneshot::channel();
                        if tx.send((req, otx)).await.is_err() {
                            CtlResponse { ok: false, body: "engine stopped".into() }
                        } else {
                            orx.await.unwrap_or(CtlResponse { ok: false, body: "no reply".into() })
                        }
                    }
                    Err(e) => CtlResponse { ok: false, body: format!("bad request: {e}").into() },
                };
                let mut s = serde_json::to_string(&resp).unwrap_or_else(|_| "{\"ok\":false}".into());
                s.push('\n');
                if w.write_all(s.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
    }
}

/// Клиент: отправляет запрос и ждёт ответ.
pub async fn request(path: &Path, req: &CtlRequest) -> anyhow::Result<CtlResponse> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    let (r, mut w) = stream.into_split();
    let mut s = serde_json::to_string(req)?;
    s.push('\n');
    w.write_all(s.as_bytes()).await?;
    let mut lines = BufReader::new(r).lines();
    let line = lines.next_line().await?.ok_or_else(|| anyhow::anyhow!("connection closed"))?;
    Ok(serde_json::from_str(&line)?)
}

/// Разбор команды Telegram (`/kill`, `/kill flatten`, `/status`, `/pause`, `/resume_entries`).
pub fn parse_telegram(text: &str) -> Option<CtlRequest> {
    let mut it = text.split_whitespace();
    let cmd = it.next()?.split('@').next()?;
    let arg = it.next();
    Some(match cmd {
        "/status" => CtlRequest::Status,
        "/kill" => {
            CtlRequest::Command(OperatorCommand::Kill { flatten: arg == Some("flatten"), source: "telegram".into() })
        }
        "/pause" => CtlRequest::Command(OperatorCommand::PauseEntries { paused: true }),
        "/resume_entries" => CtlRequest::Command(OperatorCommand::PauseEntries { paused: false }),
        _ => return None,
    })
}

/// Опрос команд Telegram: принимаются только сообщения пользователей из `allowed_user_ids`.
pub async fn telegram_commands(tg: sh_obs::Telegram, allowed: Vec<i64>, tx: mpsc::Sender<CtlEnvelope>) {
    let mut offset = 0i64;
    loop {
        match tg.get_updates(offset, 25).await {
            Ok(updates) => {
                for (id, user, text) in updates {
                    offset = offset.max(id + 1);
                    if !allowed.contains(&user) {
                        tracing::warn!(user, "telegram command from unauthorised user ignored");
                        continue;
                    }
                    let Some(req) = parse_telegram(&text) else { continue };
                    let (otx, orx) = oneshot::channel();
                    if tx.send((req, otx)).await.is_err() {
                        return;
                    }
                    if let Ok(resp) = orx.await {
                        let body = match &resp.body {
                            serde_json::Value::String(s) => s.clone(),
                            v => v.to_string().chars().take(3500).collect(),
                        };
                        let _ = tg.send(&body).await;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "telegram polling failed");
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_parsing() {
        assert_eq!(parse_telegram("/status"), Some(CtlRequest::Status));
        assert_eq!(
            parse_telegram("/kill@my_bot flatten"),
            Some(CtlRequest::Command(OperatorCommand::Kill { flatten: true, source: "telegram".into() }))
        );
        assert_eq!(parse_telegram("hello"), None);
    }

    #[test]
    fn request_json_shape() {
        let r = CtlRequest::Command(OperatorCommand::Kill { flatten: false, source: "ctl".into() });
        let s = serde_json::to_string(&r).unwrap_or_default();
        assert_eq!(serde_json::from_str::<CtlRequest>(&s).ok(), Some(r));
    }

    #[tokio::test]
    async fn socket_roundtrip() {
        let dir = std::env::temp_dir().join(format!("shctl-{}", std::process::id()));
        let path = dir.join("t.sock");
        let (tx, mut rx) = mpsc::channel::<CtlEnvelope>(4);
        let p = path.clone();
        tokio::spawn(async move { serve(p, tx).await });
        tokio::spawn(async move {
            while let Some((req, reply)) = rx.recv().await {
                let _ = reply.send(CtlResponse { ok: true, body: format!("{req:?}").into() });
            }
        });
        for _ in 0..50 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let r = request(&path, &CtlRequest::Status).await.unwrap_or(CtlResponse { ok: false, body: "x".into() });
        assert!(r.ok);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
