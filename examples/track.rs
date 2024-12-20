//! track
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

    // Create an abort signal
    let (abort_set, abort) = tokio::sync::oneshot::channel();

    // Signal to receive a port
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);

    // Create a stream to listen for events
    let stream = comport::listen("comport demo")
        .track(vec![("2FE3", "0100")])?
        .take_until(abort);

    // Spawn a task to listen for USB plug/unplug events
    let jh: JoinHandle<Result<(), TrackingError>> = tokio::spawn(async move {
        // Send the first connected device to our main task
        let mut pinned = pin!(stream);
        while let Some(tracked) = pinned.next().await {
            if let Err(error) = tx.send(tracked?).await {
                error!(port = ?error, "failed to send port");
            }
        }

        drop(tx);
        Ok(())
    });

    // get a new device and wait for its unplug
    let mut count = 0usize;
    while let Some(tracked) = rx.recv().await {
        info!(?tracked.port, "waiting for unplug event");
        tracked.unplugged.await?;
        info!(?tracked.port, "received unplug event");
        count += 1;
        if count == 3 {
            break;
        }
    }
    abort_set.send(()).unwrap();
    jh.await??;
    Ok(())
}
