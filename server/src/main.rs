mod signal;
mod stun;

use std::{
    env,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use tokio::net::TcpListener;

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let state_dir =
        PathBuf::from(env::var_os("DUOLINK_STATE_DIR").context("DUOLINK_STATE_DIR is required")?);
    std::fs::create_dir_all(&state_dir)?;
    let signal = signal::Signal::open(&state_dir.join("signal.db"))?;
    let http =
        TcpListener::bind(env::var("DUOLINK_HTTP").unwrap_or_else(|_| "127.0.0.1:8705".into()))
            .await?;
    let stun_a =
        stun::bind(&env::var("DUOLINK_STUN_A").context("DUOLINK_STUN_A is required")?).await?;
    let stun_b =
        stun::bind(&env::var("DUOLINK_STUN_B").context("DUOLINK_STUN_B is required")?).await?;
    eprintln!(
        "DuoLink listening on {}, STUN {} and {}",
        http.local_addr()?,
        stun_a.local_addr()?,
        stun_b.local_addr()?
    );
    let cleanup = signal.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            if cleanup.cleanup().await.is_err() {
                eprintln!("signal store cleanup failed");
            }
        }
    });
    tokio::try_join!(
        async {
            axum::serve(http, signal.router())
                .await
                .map_err(anyhow::Error::from)
        },
        stun::serve(stun_a),
        stun::serve(stun_b),
    )?;
    Ok(())
}
