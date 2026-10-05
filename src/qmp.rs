use anyhow::{Context, Result};
use serde_json::{Value as Json, json};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Hand-rolled QMP client: transport-agnostic socket + newline-delimited
/// JSON. No `qapi` dependency; `id`-based demux of responses vs async
/// `event`s. One client = one session: single-client transports
/// (Windows named pipes) refuse overlap, so callers connect, act, and
/// drop — never hold two clients to the same endpoint.
pub struct QmpClient {
    writer: tokio::io::WriteHalf<crate::ipc::Stream>,
    reader: BufReader<tokio::io::ReadHalf<crate::ipc::Stream>>,
    next_id: AtomicU64,
    pub events: mpsc::UnboundedReceiver<Json>,
    event_tx: mpsc::UnboundedSender<Json>,
}

impl QmpClient {
    pub async fn connect(path: &str) -> Result<Self> {
        let stream = crate::ipc::connect(path).await?;
        let (rh, wh) = tokio::io::split(stream);
        let (tx, rx) = mpsc::unbounded_channel();
        let mut c = Self {
            writer: wh,
            reader: BufReader::new(rh),
            next_id: AtomicU64::new(1),
            events: rx,
            event_tx: tx,
        };
        // Greeting: {"QMP": {"version": ...}}
        let mut line = String::new();
        c.reader
            .read_line(&mut line)
            .await
            .context("QMP greeting")?;
        let v: Json = serde_json::from_str(&line).context("parse QMP greeting")?;
        if v.get("QMP").is_none() {
            anyhow::bail!("unexpected QMP greeting: {line}");
        }
        // Enter command mode.
        let res = c
            .execute_raw(json!({"execute": "qmp_capabilities"}))
            .await?;
        if res.get("error").is_some() {
            anyhow::bail!("qmp_capabilities failed: {res}");
        }
        Ok(c)
    }

    async fn execute_raw(&mut self, mut payload: Json) -> Result<Json> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        payload["id"] = json!(id);
        let mut msg = serde_json::to_vec(&payload)?;
        msg.push(b'\n');
        self.writer.write_all(&msg).await.context("QMP write")?;
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await.context("QMP read")?;
            if n == 0 {
                anyhow::bail!("QMP socket closed");
            }
            let v: Json =
                serde_json::from_str(&line).with_context(|| format!("parse QMP frame: {line}"))?;
            if v.get("event").is_some() {
                let _ = self.event_tx.send(v);
                continue;
            }
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                return Ok(v);
            }
            // Untagged return (older QEMU): accept if it carries return/error.
            if v.get("return").is_some() || v.get("error").is_some() {
                return Ok(v);
            }
        }
    }

    pub async fn execute(&mut self, payload: Json) -> Result<Json> {
        let res = self.execute_raw(payload).await?;
        if let Some(err) = res.get("error") {
            anyhow::bail!("QMP error: {err}");
        }
        Ok(res)
    }

    /// Zero-downtime pivot to a new overlay (modern `node-name` form).
    pub async fn live_overlay(&mut self, node: &str, overlay_path: &str) -> Result<()> {
        let res = self
            .execute(json!({
                "execute": "blockdev-snapshot-sync",
                "arguments": {
                    "node-name": node,
                    "snapshot-file": overlay_path,
                    "snapshot-node-name": format!("{node}-overlay"),
                    "format": "qcow2"
                }
            }))
            .await;
        if let Err(e) = res {
            // Fallback for old QEMU which wants `device` instead of `node-name`.
            let msg = format!("{e:#}");
            if msg.contains("node-name") || msg.contains("invalid") || msg.contains("not found") {
                self.execute(json!({
                    "execute": "blockdev-snapshot-sync",
                    "arguments": {
                        "device": node,
                        "snapshot-file": overlay_path,
                        "format": "qcow2"
                    }
                }))
                .await?;
                return Ok(());
            }
            return Err(e);
        }
        Ok(())
    }

    /// HMP escape hatch for full state checkpoints. Returns the HMP output
    /// text: success prints nothing, so ANY output is suspect (a refusal
    /// like the pflash veto surfaces here rather than as a QMP error —
    /// observed live as a silent no-op save). Callers check the text.
    pub async fn savevm(&mut self, name: &str) -> Result<String> {
        let res = self
            .execute(json!({
                "execute": "human-monitor-command",
                "arguments": { "command-line": format!("savevm {name}") }
            }))
            .await?;
        Ok(res
            .get("return")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// Live RAM + device migration to a file (`snapshot --live`): unlike
    /// `savevm`, migration transfers pflash content instead of refusing
    /// it, so the writable-VARS veto does not apply. Async job — poll
    /// [`QmpClient::query_migrate_status`] until `completed`.
    pub async fn migrate_to_file(&mut self, file: &std::path::Path) -> Result<()> {
        self.execute(json!({
            "execute": "migrate",
            "arguments": { "uri": crate::commands::migrate_write_uri(file) }
        }))
        .await?;
        Ok(())
    }

    /// Migration job status (`active`, `completed`, `failed`, `cancelled`,
    /// ...). Bails on unexpected shapes — a status we cannot parse is not
    /// a status we should wait on.
    pub async fn query_migrate_status(&mut self) -> Result<String> {
        let res = self.execute(json!({ "execute": "query-migrate" })).await?;
        res.get("return")
            .and_then(|r| r.get("status"))
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("unparseable query-migrate: {res}"))
    }

    /// VM run state (`running`, `paused`, ...). After a migration the guest
    /// must be `running` — a completed job can still leave it paused, which
    /// reads as a lockup from the outside.
    pub async fn query_vm_status(&mut self) -> Result<String> {
        let res = self.execute(json!({ "execute": "query-status" })).await?;
        res.get("return")
            .and_then(|r| r.get("status"))
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("unparseable query-status: {res}"))
    }

    /// Resume a paused VM. No-op against a running one is an error from
    /// QEMU, so callers check [`QmpClient::query_vm_status`] first.
    pub async fn resume_vm(&mut self) -> Result<()> {
        self.execute(json!({ "execute": "cont" })).await?;
        Ok(())
    }

    /// Pause a running VM. Used post-restore as half of a stop/cont cycle:
    /// HVF can fail to re-arm halted-vCPU timers on the incoming path, and
    /// a pause/resume round-trip re-arms them (observed downstream fix).
    pub async fn pause_vm(&mut self) -> Result<()> {
        self.execute(json!({ "execute": "stop" })).await?;
        Ok(())
    }

    /// HMP escape hatch for state restore. Same output-text contract as
    /// [`QmpClient::savevm`]: empty is success, anything else is a refusal.
    /// (Kept for forensics; the live path resumes via `-incoming`, not this.)
    pub async fn loadvm(&mut self, name: &str) -> Result<String> {
        let res = self
            .execute(json!({
                "execute": "human-monitor-command",
                "arguments": { "command-line": format!("loadvm {name}") }
            }))
            .await?;
        Ok(res
            .get("return")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// ACPI power-button event: the guest OS shuts itself down cleanly
    /// (no NTFS dirty bit), then QEMU exits on its own. Prefer over `quit`
    /// for Windows guests; `quit` is the last resort.
    pub async fn powerdown(&mut self) -> Result<()> {
        self.execute(json!({"execute": "system_powerdown"})).await?;
        Ok(())
    }

    /// Graceful shutdown: QEMU exits, usually without sending a response,
    /// so drain until EOF instead of waiting for a matching `id`.
    pub async fn quit(&mut self) -> Result<()> {
        use tokio::io::AsyncReadExt;
        let mut msg = serde_json::to_vec(&json!({"execute": "quit"}))?;
        msg.push(b'\n');
        self.writer
            .write_all(&msg)
            .await
            .context("QMP quit write")?;
        let mut rest = String::new();
        let _ = self.reader.read_to_string(&mut rest).await;
        Ok(())
    }

    /// Screenshot of the virtual framebuffer (preferred over monitor `nc -U`).
    pub async fn screendump(&mut self, filename: &str) -> Result<()> {
        self.execute(json!({
            "execute": "human-monitor-command",
            "arguments": { "command-line": format!("screendump {filename}") }
        }))
        .await?;
        Ok(())
    }

    /// Tap guest keys (QMP `send-key`), e.g. `["ret"]` to answer the
    /// Windows ISO's "press any key to boot from CD" prompt.
    pub async fn send_keys(&mut self, keys: &[&str]) -> Result<()> {
        self.execute(send_keys_payload(keys)).await?;
        Ok(())
    }

    /// Build (but do not send) a response-check helper for unit tests.
    pub fn check_no_error(res: &Json) -> Result<()> {
        if let Some(err) = res.get("error") {
            anyhow::bail!("QMP error: {err}");
        }
        Ok(())
    }
}

/// Pure payload builders (unit-testable without a live VM).
pub fn blockdev_snapshot_payload(node: &str, overlay: &str) -> Json {
    json!({
        "execute": "blockdev-snapshot-sync",
        "arguments": {
            "node-name": node,
            "snapshot-file": overlay,
            "snapshot-node-name": format!("{node}-overlay"),
            "format": "qcow2"
        }
    })
}

pub fn hmp_payload(command_line: &str) -> Json {
    json!({
        "execute": "human-monitor-command",
        "arguments": { "command-line": command_line }
    })
}

pub fn send_keys_payload(keys: &[&str]) -> Json {
    // QMP KeyValue union form: {"type": "qcode", "data": "<QKeyCode>"}.
    // ("ret" = Return.) The {"key": ...} shape is rejected with
    // "Invalid parameter type" — a silent failure we hit in testing.
    json!({
        "execute": "send-key",
        "arguments": {
            "keys": keys.iter().map(|k| json!({"type": "qcode", "data": k})).collect::<Vec<_>>()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_shapes() {
        let p = blockdev_snapshot_payload("drive0", "/tmp/o.qcow2");
        assert_eq!(p["execute"], "blockdev-snapshot-sync");
        assert_eq!(p["arguments"]["node-name"], "drive0");
        let h = hmp_payload("savevm ckpt");
        assert_eq!(h["arguments"]["command-line"], "savevm ckpt");
        let k = send_keys_payload(&["ret"]);
        assert_eq!(k["execute"], "send-key");
        assert_eq!(k["arguments"]["keys"][0]["type"], "qcode");
        assert_eq!(k["arguments"]["keys"][0]["data"], "ret");
    }

    #[test]
    fn error_detection() {
        assert!(QmpClient::check_no_error(&json!({"return": {}})).is_ok());
        assert!(QmpClient::check_no_error(&json!({"error": {"desc": "x"}})).is_err());
    }

    #[test]
    fn demux_id_vs_event() {
        // Logic-level: frames with "event" go to the channel, matching "id" wins.
        let frames = vec![
            json!({"event": "DEVICE_DELETED"}),
            json!({"return": {}, "id": 7}),
        ];
        let mut got_event = false;
        for f in frames {
            if f.get("event").is_some() {
                got_event = true;
                continue;
            }
            assert_eq!(f["id"], 7);
        }
        assert!(got_event);
    }
}
