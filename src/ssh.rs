use anyhow::Result;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

/// Poll `host:port` until a real SSH server answers. A bare TCP connect is
/// NOT enough: QEMU's user-mode NAT completes the host-side handshake while
/// the guest is still in UEFI/Setup, so `connect()` succeeds minutes before
/// any `sshd` exists (false "ready" + hung `ssh` client). We require the
/// `SSH-2.0-...` banner, which only a live sshd sends.
///
/// Default budget 60 × 15s = 15 min. On timeout the caller should
/// `screendump` before failing (see canary test).
pub async fn wait_for_ssh(host: &str, port: u16, timeout_: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout_;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match TcpStream::connect((host, port)).await {
            Ok(stream) => {
                let mut reader = BufReader::new(stream);
                let mut banner = String::new();
                // Slirp may accept then stall; bound the banner wait so a
                // half-open forward degrades to another poll, not a hang.
                // Cap by the remaining budget so short timeouts stay short.
                let now = tokio::time::Instant::now();
                let budget = deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_secs(10));
                match timeout(budget, reader.read_line(&mut banner)).await {
                    Ok(Ok(_)) if banner.starts_with("SSH-") => {
                        tracing::info!("SSH banner after {attempt} polls: {}", banner.trim());
                        return Ok(());
                    }
                    other => {
                        tracing::debug!("poll {attempt}: no SSH banner yet ({other:?})");
                    }
                }
            }
            Err(e) => {
                tracing::debug!("SSH poll {attempt} refused: {e:#}");
            }
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            anyhow::bail!(
                "timed out waiting for SSH on {host}:{port} after {attempt} polls — likely stuck on a GUI prompt"
            );
        }
        // Sleep the poll interval, capped at the remaining budget so short
        // timeouts (and tests) don't overshoot by a full interval.
        sleep(Duration::from_secs(15).min(deadline.saturating_duration_since(now))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fast_fail_on_closed_port() {
        let err = wait_for_ssh("127.0.0.1", 1, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"));
    }

    #[tokio::test]
    async fn succeeds_on_ssh_banner() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(b"SSH-2.0-OpenSSH_for_Windows_9.5\r\n")
                .await
                .unwrap();
            // Hold open briefly so the reader sees EOF only after the banner.
            sleep(Duration::from_secs(5)).await;
        });
        wait_for_ssh("127.0.0.1", port, Duration::from_secs(30))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn silent_accept_is_not_ready() {
        // Regression: QEMU slirp completes TCP while the guest has no sshd.
        // A listener that accepts but never sends a banner must NOT resolve.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let _ = listener.accept().await;
            }
        });
        let err = wait_for_ssh("127.0.0.1", port, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"));
    }
}
