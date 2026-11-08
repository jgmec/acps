//! Database connection: settings, a pool that tolerates outages, and migrations.

use anyhow::{Context, Result};
use sqlx::{postgres::PgPoolOptions, PgPool};

/// How long to wait for a database connection before treating it as unreachable.
pub(crate) const DB_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A pool that connects on first use, so a database outage doesn't stop the command or
/// server from starting; each use waits at most DB_CONNECT_TIMEOUT for a connection.
pub(crate) fn lazy_pool(max_connections: u32) -> Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(DB_CONNECT_TIMEOUT)
        .connect_lazy(&database_url()?)?)
}

/// Apply pending migrations, retrying every 10 s until the database can be reached, then
/// set `ready`. Until then the API treats the database as unavailable: on a new database the
/// tables don't exist yet.
pub(crate) async fn migrate_when_reachable(pool: PgPool, ready: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    loop {
        match sqlx::migrate!().run(&pool).await {
            Ok(()) => {
                ready.store(true, std::sync::atomic::Ordering::Relaxed);
                return info!("database schema is up to date");
            }
            Err(e) => {
                warn!("can't apply migrations yet ({e}); retrying in 10 s");
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        }
    }
}

pub(crate) fn database_url() -> Result<String> {
    dotenvy::var("DATABASE_URL").context(
        "DATABASE_URL is not set; export it or put it in a .env file in the current directory, \
         e.g. DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/wtfps",
    )
}
