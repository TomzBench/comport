//! scan
use comport::prelude::*;
use futures::StreamExt;
use std::pin::pin;
use tokio::task::JoinHandle;
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, fmt, layer::SubscriberExt, prelude::*};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Setup logging
    let stdout = fmt::layer()
        .compact()
        .with_ansi(true)
        .with_level(true)
        .with_file(false)
        .with_line_number(false)
        .with_target(true);
    tracing_subscriber::registry()
        .with(stdout)
        .with(LevelFilter::TRACE)
        .init();

    // Welcome message
    info!("Application service starting...");

    // Channel to receive events
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);

    // Create a stream to listen for events
    let stream = comport::listen("COMPORT_DEMO")?.track(vec![("2FE3", "0100")])?;
    let jh: JoinHandle<Result<(), TrackingError>> = tokio::spawn(async move {
        let mut pinned = pin!(stream);
        let mut count = 0usize;
        while let Some(msg) = pinned.next().await {
            if let Err(error) = tx.send(msg?).await {
                error!(port = ?error, "failed to send port");
            }
            count += 1;
            if count == 8 {
                break;
            }
        }
        Ok(())
    });

    // Scan the same port 5 times (1 on startup + 4 extras)
    info!("scanning COMPORT_DEMO");
    std::thread::sleep(std::time::Duration::from_millis(100));
    comport::rescan("COMPORT_DEMO")?;
    comport::rescan("COMPORT_DEMO")?;
    comport::rescan("COMPORT_DEMO")?;
    comport::rescan("COMPORT_DEMO")?;
    comport::rescan("COMPORT_DEMO")?;
    comport::rescan("COMPORT_DEMO")?;
    comport::rescan("COMPORT_DEMO")?;
    while let Some(tracked) = rx.recv().await {
        info!(?tracked, "received scan");
    }

    sdfdsf;
    info!("demo over");
    jh.await??;
    Ok(())
}
