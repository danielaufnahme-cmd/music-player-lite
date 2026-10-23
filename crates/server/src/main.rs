//! Headless Geonix lite server (systemd, phone page, device sync).
//! Configuration comes from the environment; see the library docs.

fn main() -> anyhow::Result<()> {
    geonix_server::init_logging();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(16)
        .thread_keep_alive(std::time::Duration::from_secs(10))
        .enable_all()
        .build()?;
    rt.block_on(geonix_server::run(geonix_server::ServerConfig::from_env(), geonix_server::shutdown_signal()))?;
    // Don't wait for in-flight downloads or open streams.
    rt.shutdown_timeout(std::time::Duration::from_millis(500));
    Ok(())
}
