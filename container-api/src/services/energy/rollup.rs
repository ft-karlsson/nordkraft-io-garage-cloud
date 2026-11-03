//! Hourly rollup of the minute rows, and pruning. Runs on the controller
//! every ten minutes; every step is idempotent, so a missed run just means
//! the next one does more.

use super::store;
use sqlx::PgPool;
use std::time::Duration;
use tracing::{debug, info, warn};

pub fn spawn(pool: PgPool, every: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match store::rollup_and_prune(&pool).await {
                Ok((hours, pruned)) if pruned > 0 => {
                    info!("⚡ Energy rollup: {hours} hour rows, {pruned} minute rows pruned")
                }
                Ok((hours, _)) => debug!("Energy rollup: {hours} hour rows"),
                Err(e) => warn!("Energy rollup failed: {e}"),
            }
        }
    });
}
