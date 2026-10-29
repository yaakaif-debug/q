//! Утилита разработчика: снимает N сообщений WebSocket для фикстур.
//! Использование: ws_capture <url> <count> [subscribe-json ...]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sh_venue_api::{ProxyConfig, connect_ws};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let url = args.get(1).ok_or("url required")?;
    let count: usize = args.get(2).ok_or("count required")?.parse()?;
    let proxy = ProxyConfig::from_env();
    let mut ws = connect_ws(url, proxy.as_ref(), Duration::from_secs(15)).await?;
    for sub in &args[3..] {
        ws.send(Message::text(sub.clone())).await?;
    }
    let mut n = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while n < count {
        let msg = tokio::time::timeout_at(deadline, ws.next()).await?;
        match msg {
            Some(Ok(Message::Text(t))) => {
                println!("{}", t.as_str());
                n += 1;
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
            None => break,
        }
    }
    Ok(())
}
