use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Disk + vvfat answer-file staging helpers.
pub struct DiskManager;

impl DiskManager {
    /// Sparse base image: `<qemu-img> create -f qcow2 <path> <size>G`.
    /// Pass `"qemu-img"` for PATH lookup or a portable tree's
    /// `<root>/bin/qemu-img` (see `PortableQemu`).
    pub async fn create_base(qemu_img: &Path, path: &Path, size_gb: u32) -> Result<()> {
        let status = Command::new(qemu_img)
            .arg("create")
            .arg("-f")
            .arg("qcow2")
            .arg(path)
            .arg(format!("{size_gb}G"))
            .status()
            .await
            .context("failed to execute qemu-img (is qemu-utils installed?)")?;
        if !status.success() {
            anyhow::bail!("qemu-img create base failed for {}", path.display());
        }
        Ok(())
    }

    /// Offline overlay: `<qemu-img> create -f qcow2 -F qcow2 -b <base> <overlay>`.
    /// Caller must guarantee a clean shutdown of the base first (see plan §3.4).
    pub async fn create_overlay(qemu_img: &Path, base: &Path, overlay: &Path) -> Result<()> {
        let status = Command::new(qemu_img)
            .arg("create")
            .arg("-f")
            .arg("qcow2")
            .arg("-F")
            .arg("qcow2")
            .arg("-b")
            .arg(base)
            .arg(overlay)
            .status()
            .await
            .context("failed to execute qemu-img")?;
        if !status.success() {
            anyhow::bail!(
                "qemu-img failed to create overlay {} over {}",
                overlay.display(),
                base.display()
            );
        }
        Ok(())
    }

    /// Write `autounattend.xml` at the root of `dir` (created if missing).
    /// Freeze rule: never touch this dir again once QEMU has launched.
    pub fn ensure_unattend_dir(dir: &Path, xml: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let file = dir.join(crate::paths::UNATTEND_XML);
        std::fs::write(&file, xml).with_context(|| format!("write {}", file.display()))?;
        Ok(file)
    }

    /// Opt-in secondary answer ISO (Packer-style): `mkisofs -o out -J -r <xml>`.
    pub async fn build_unattend_iso(xml_path: &Path, out_iso: &Path) -> Result<()> {
        let mkisofs = if which("mkisofs") {
            "mkisofs"
        } else if which("genisoimage") {
            "genisoimage"
        } else if which("xorriso") {
            "xorriso"
        } else {
            anyhow::bail!(
                "no mkisofs/genisoimage/xorriso on PATH (needed only for --unattend-iso)"
            );
        };
        let mut cmd = Command::new(mkisofs);
        if mkisofs == "xorriso" {
            cmd.arg("-as").arg("mkisofs");
        }
        let status = cmd
            .arg("-o")
            .arg(out_iso)
            .arg("-J")
            .arg("-r")
            .arg(xml_path)
            .status()
            .await
            .context("failed to execute ISO tool")?;
        if !status.success() {
            anyhow::bail!("ISO authoring failed");
        }
        Ok(())
    }
}

fn which(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unattend_staging_writes_root_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("unattend");
        let f = DiskManager::ensure_unattend_dir(&target, "<unattend/>").unwrap();
        assert_eq!(f, target.join("autounattend.xml"));
        assert_eq!(std::fs::read_to_string(f).unwrap(), "<unattend/>");
    }
}
