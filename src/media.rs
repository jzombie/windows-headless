//! Bootable USB installer media, built with pure Rust (no host tools).
//!
//! Proven design (spike-media, 2026-09-22): one FAT32 partition with the
//! full ISO tree except `install.wim`, plus `wimlib-imagex split` output
//! streamed into `sources/`. EDK2 boots it; Setup installs from it.
//! macOS builds that partition as MBR (the layout the proving run used);
//! Linux builds it as GPT + Microsoft Basic Data — see
//! [`build_installer_media`].
//! The only non-Rust input is the `wimlib-imagex` binary (vendored release
//! asset); everything else is crates.io.
use anyhow::{Context, Result};
use hadris_fat::dir::FatDir;
use hadris_fat::format::{FatFormatOptions, FatTypeSelection, FatVolumeFormatter};
use hadris_fat::{FatVolume, FatVolumeWriteExt};
use hadris_udf::{UdfDir, UdfVolume};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// First data partition LBA (conventional 1 MiB alignment).
pub const PART_LBA: u64 = 2048;

/// Microsoft Basic Data partition type GUID (little-endian wire format).
///
/// Not the ESP type: WinPE mounts an ESP Hidden with no drive letter, so
/// Setup never treats the volume as media ("Install driver to show
/// hardware").  OVMF still boots `\EFI\BOOT\BOOTX64.EFI` from any FAT32
/// volume regardless of partition type (Rufus rufus#461 did the same).
const BASIC_DATA_TYPE_GUID: [u8; 16] = [
    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];

/// GPT header signature "EFI PART".
const GPT_SIG: [u8; 8] = *b"EFI PART";

/// GPT header revision 1.0.
const GPT_REV: u32 = 0x00010000;

/// Number of partition entries (128 is standard).
const GPT_NUM_ENTRIES: u32 = 128;

/// Size of each partition entry (128 bytes is standard).
const GPT_ENTRY_SIZE: u32 = 128;

/// Write a GPT partition table + protective MBR for a UEFI-bootable image.
/// Returns `(partition offset, partition size)` in bytes — the FAT32 volume
/// must not exceed the size (the partition stops short of the disk end,
/// where the backup GPT header and entries live).
fn write_gpt_partition_table(
    img: &mut File,
    size_bytes: u64,
    disk_guid: [u8; 16],
) -> Result<(u64, u64)> {
    let sector_size: u64 = 512;
    let total_sectors = size_bytes / sector_size;
    let part_start_lba = PART_LBA;
    let entries_lba: u64 = 2;
    let entries_bytes = GPT_NUM_ENTRIES as usize * GPT_ENTRY_SIZE as usize;
    let entries_sectors = entries_bytes as u64 / sector_size; // 128
    let backup_header_lba = total_sectors - 1;
    let backup_entries_lba = backup_header_lba - entries_sectors;
    let part_end_lba = backup_entries_lba - 1;

    // --- Protective MBR (LBA 0) ---
    let mut mbr = [0u8; 512];
    mbr[446] = 0x00; // not bootable (GPT protective MBR)
    mbr[447] = 0x00; // starting head
    mbr[448] = 0x01; // starting sector
    mbr[450] = 0xEE; // GPT protective
    mbr[451] = 0xFF;
    mbr[452] = 0xFF;
    mbr[453] = 0xFF;
    mbr[454..458].copy_from_slice(&1u32.to_le_bytes());
    let mbr_size = (total_sectors - 1).min(u32::MAX as u64) as u32;
    mbr[458..462].copy_from_slice(&mbr_size.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    img.seek(SeekFrom::Start(0))?;
    img.write_all(&mbr)?;

    // --- Partition entries (LBA 2) ---
    // GPT entry layout (128 bytes each):
    //   0..16   type GUID
    //  16..32   unique partition GUID
    //  32..40   starting LBA
    //  40..48   ending LBA
    //  48..56   attribute flags
    //  56..128  partition name (UTF-16LE, 36 code units)
    let mut entries = vec![0u8; entries_bytes];
    {
        let off = 0usize;
        entries[off..off + 16].copy_from_slice(&BASIC_DATA_TYPE_GUID);
        let part_guid = rand_guid();
        entries[off + 16..off + 32].copy_from_slice(&part_guid);
        entries[off + 32..off + 40].copy_from_slice(&part_start_lba.to_le_bytes());
        entries[off + 40..off + 48].copy_from_slice(&part_end_lba.to_le_bytes());
        // attributes: 0x5 = RequiredForFunctionality (0x1) + RequiredForBoot (0x4)
        entries[off + 48..off + 56].copy_from_slice(&0x5u64.to_le_bytes());
        let name = "Basic data partition";
        for (i, ch) in name.encode_utf16().enumerate() {
            let b = ch.to_le_bytes();
            entries[off + 56 + 2 * i..off + 58 + 2 * i].copy_from_slice(&b);
        }
    }
    img.seek(SeekFrom::Start(entries_lba * sector_size))?;
    img.write_all(&entries)?;

    let entries_crc = crc32fast::hash(&entries);

    // --- Primary GPT header (LBA 1) ---
    // Header layout (92 bytes used, rest zero):
    //   0..8    signature
    //   8..12   revision
    //  12..16   header size (92)
    //  16..20   CRC32 of header (computed last, with this field zeroed)
    //  20..24   reserved
    //  24..32   my LBA (primary header = 1)
    //  32..40   alternate LBA (backup header)
    //  40..48   first usable LBA
    //  48..56   last usable LBA
    //  56..72   disk GUID
    //  72..80   partition entries start LBA
    //  80..84   number of partition entries
    //  84..88   size of partition entry
    //  88..92   CRC32 of partition entries
    let mut header = [0u8; 512];
    header[0..8].copy_from_slice(&GPT_SIG);
    header[8..12].copy_from_slice(&GPT_REV.to_le_bytes());
    header[12..16].copy_from_slice(&92u32.to_le_bytes());
    header[24..32].copy_from_slice(&1u64.to_le_bytes()); // my LBA
    header[32..40].copy_from_slice(&backup_header_lba.to_le_bytes());
    header[40..48].copy_from_slice(&(PART_LBA).to_le_bytes()); // first usable = part start
    header[48..56].copy_from_slice(&part_end_lba.to_le_bytes());
    header[56..72].copy_from_slice(&disk_guid);
    header[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    header[80..84].copy_from_slice(&GPT_NUM_ENTRIES.to_le_bytes());
    header[84..88].copy_from_slice(&GPT_ENTRY_SIZE.to_le_bytes());
    header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    let hdr_crc = crc32fast::hash(&header[0..92]);
    header[16..20].copy_from_slice(&hdr_crc.to_le_bytes());
    img.seek(SeekFrom::Start(sector_size))?;
    img.write_all(&header)?;

    // --- Backup partition entries (before backup header) ---
    img.seek(SeekFrom::Start(backup_entries_lba * sector_size))?;
    img.write_all(&entries)?;

    // --- Backup GPT header (last LBA) ---
    let mut backup = [0u8; 512];
    backup[0..8].copy_from_slice(&GPT_SIG);
    backup[8..12].copy_from_slice(&GPT_REV.to_le_bytes());
    backup[12..16].copy_from_slice(&92u32.to_le_bytes());
    backup[24..32].copy_from_slice(&backup_header_lba.to_le_bytes()); // my LBA
    backup[32..40].copy_from_slice(&1u64.to_le_bytes()); // alternate = primary
    backup[40..48].copy_from_slice(&PART_LBA.to_le_bytes());
    backup[48..56].copy_from_slice(&part_end_lba.to_le_bytes());
    backup[56..72].copy_from_slice(&disk_guid);
    backup[72..80].copy_from_slice(&backup_entries_lba.to_le_bytes());
    backup[80..84].copy_from_slice(&GPT_NUM_ENTRIES.to_le_bytes());
    backup[84..88].copy_from_slice(&GPT_ENTRY_SIZE.to_le_bytes());
    backup[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    let backup_crc = crc32fast::hash(&backup[0..92]);
    backup[16..20].copy_from_slice(&backup_crc.to_le_bytes());
    img.seek(SeekFrom::Start(backup_header_lba * sector_size))?;
    img.write_all(&backup)?;

    let part_off = part_start_lba * sector_size;
    let part_size = (part_end_lba - part_start_lba + 1) * sector_size;
    Ok((part_off, part_size))
}

/// Generate a random GUID (16 bytes, RFC 4122 v4).
fn rand_guid() -> [u8; 16] {
    use std::io::Read;
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        buf[..16].copy_from_slice(&t.to_le_bytes());
    }
    buf[6] = (buf[6] & 0x0F) | 0x40;
    buf[8] = (buf[8] & 0x3F) | 0x80;
    buf
}

/// Write an MBR partition table (BIOS-bootable). Returns the partition data offset in bytes.
fn write_mbr_partition_table(img: &mut File, size_bytes: u64) -> Result<u64> {
    let part_off = PART_LBA * 512;
    let part_sectors = ((size_bytes - part_off) / 512) as u32;
    let mut mbr = vec![0u8; 512];
    mbr[446] = 0x80;
    mbr[450] = 0x0C;
    mbr[454..458].copy_from_slice(&(PART_LBA as u32).to_le_bytes());
    mbr[458..462].copy_from_slice(&part_sectors.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    img.seek(SeekFrom::Start(0))?;
    img.write_all(&mbr)?;
    Ok(part_off)
}

/// What to build: the retail Windows ISO in, a bootable raw image out.
pub struct MediaSpec {
    /// Source retail Windows ISO (UDF).
    pub iso: PathBuf,
    /// Output raw image path (created/truncated).
    pub out: PathBuf,
    /// Image size in bytes (10 GiB covers the ~8 GB tree + SWMs).
    pub size_bytes: u64,
    /// FAT32 volume label (`WINSETUP`).
    pub label: String,
    /// ISO file paths to stream into the image (one file at a time,
    /// never all in RAM).  Pre-computed via [`list_iso_tree`].
    pub iso_paths: Vec<String>,
    /// Small files laid onto image (in-memory).
    pub staged: Vec<(String, Vec<u8>)>,
    /// SWM split-WIM parts (file paths, streamed).
    pub swm_parts: Vec<PathBuf>,
}

/// Build the installer image: partition table + FAT32 + trees + staged + SWMs.
///
/// On Linux (pure UEFI via TianoCore/KVM), produces GPT + Basic Data FAT32.
/// On macOS (HVF, Apple Silicon EDK2 — there is no CSM there), produces the
/// proven MBR + FAT32 layout.
///
/// ISO tree files are streamed one at a time — never all in RAM — so peak
/// memory is the size of the largest single file (~600 MB for boot.wim)
/// instead of the entire tree (~6 GB).
pub fn build_installer_media(spec: &MediaSpec) -> Result<()> {
    let total: usize = spec.staged.iter().map(|(_, b)| b.len()).sum();
    tracing::info!(
        "media: {} staged ({total} bytes) + {} ISO paths + {} SWM parts",
        spec.staged.len(),
        spec.iso_paths.len(),
        spec.swm_parts.len(),
    );

    let mut img = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&spec.out)
        .with_context(|| format!("create {}", spec.out.display()))?;
    img.set_len(spec.size_bytes)?;

    // Partition table per host OS. macOS keeps the proven spike-media
    // layout: one MBR/FAT32 partition. Linux writes GPT + Microsoft Basic
    // Data — WinPE letters that volume, so Setup finds sources/install.swm
    // (an ESP-typed or otherwise unlettered volume is mounted Hidden and
    // Setup stops at "Install driver to show hardware"). EDK2 boots the
    // same FAT32 tree from either table.
    let (part_off, part_size) = match std::env::consts::OS {
        "linux" => {
            eprintln!("media: writing GPT + Basic Data partition ...");
            write_gpt_partition_table(&mut img, spec.size_bytes, rand_guid())?
        }
        _ => {
            eprintln!("media: writing MBR partition table ...");
            let off = write_mbr_partition_table(&mut img, spec.size_bytes)?;
            (off, spec.size_bytes - off)
        }
    };
    let fat_size = part_size;

    // FAT32 + full tree (dirs first, then files; writers finish() so
    // directory-entry sizes commit — silent short files otherwise).
    eprintln!(
        "media: formatting {:.0} GB FAT32 ({}) ...",
        fat_size as f64 / 1e9,
        spec.label
    );
    let options = FatFormatOptions::new(fat_size)
        .fat_type(FatTypeSelection::Fat32)
        .volume_label(&spec.label)
        .hidden_sectors(PART_LBA as u32);
    let volume =
        FatVolumeFormatter::format(PartitionView::new(img.try_clone()?, part_off), options)?;
    tracing::info!("media: formatted {:?}", volume.fat_type());
    drop(volume);
    let f = OpenOptions::new().read(true).write(true).open(&spec.out)?;
    let volume = FatVolume::open(PartitionView::new(f, part_off))?;

    // --- Pass 1: create all directories (paths only, no content) ---
    let mut created = HashSet::new();
    created.insert(String::new());
    let mut ordered: Vec<String> = spec
        .iso_paths
        .iter()
        .chain(spec.staged.iter().map(|(p, _)| p))
        .filter_map(|p| p.rsplit_once('/').map(|(d, _)| d.to_string()))
        .collect();
    ordered.sort_by_key(|d| d.len());
    ordered.dedup();
    for d in &ordered {
        mkdir_p(&volume, d, &mut created)?;
    }

    // --- Pass 2: write staged files (small, in-memory) ---
    let mut seen: HashSet<String> = HashSet::new();
    for (path, data) in &spec.staged {
        let key = path.to_lowercase();
        if !seen.insert(key.clone()) {
            eprintln!("media: duplicate staged entry skipped: {path}");
            continue;
        }
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        let parent = open_path(&volume, dir).with_context(|| format!("open parent for {path}"))?;
        let file = volume
            .create_file(&parent, name)
            .with_context(|| format!("create {path}"))?;
        let mut w = volume.write_file(&file)?;
        let mut done = 0;
        while done < data.len() {
            let k = w.write(&data[done..])?;
            if k == 0 {
                anyhow::bail!("stalled write on {path}");
            }
            done += k;
        }
        w.finish()?;
    }
    let n_staged = spec.staged.len();
    tracing::info!("media: {n_staged} staged files written");

    // --- Pass 3: stream ISO tree files (one in RAM at a time) ---
    //
    // hadris_udf only offers `read_file() -> Result<Vec<u8>>` — no `Read`
    // trait or streaming API.  We open the UDF volume once and read each
    // file individually so at most one file's worth of bytes is live at a
    // time (peak ≈ largest file, ~600 MB for boot.wim) instead of the
    // entire tree (~6 GB) held simultaneously.
    //
    // hadris-udf rejects some images with `invalid Information Control
    // Block` (observed inside an OrbStack Linux VM when extracting
    // sources/install.wim from the ARM64 retail ISO; x86_64 Linux and macOS
    // hosts parse with hadris).  isomage auto-detects ISO9660/UDF and
    // tolerates those images, so every hadris failure falls back to it —
    // per file when the volume opened, wholesale when even the open fails.
    if !spec.iso_paths.is_empty() {
        match UdfVolume::open(std::io::BufReader::new(File::open(&spec.iso)?)) {
            Ok(udf) => {
                eprintln!("media: streaming {} ISO files ...", spec.iso_paths.len());
                // Lazily-parsed isomage tree, built on the first hadris
                // per-file failure and reused for the rest.
                let mut fallback: Option<(File, isomage::TreeNode)> = None;
                for (i, path) in spec.iso_paths.iter().enumerate() {
                    if seen.contains(&path.to_lowercase()) {
                        eprintln!("media: duplicate ISO entry skipped: {path}");
                        continue;
                    }
                    seen.insert(path.to_lowercase());
                    let data = match read_file_from_udf(&udf, path)
                        .with_context(|| format!("read {path} from ISO"))
                    {
                        Ok(d) => d,
                        Err(e) => {
                            eprintln!(
                                "media: hadris read failed for {path} ({e:#}); isomage fallback ..."
                            );
                            read_iso_single_isomage_cached(&spec.iso, path, &mut fallback)?
                        }
                    };
                    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
                    let parent = open_path(&volume, dir)
                        .with_context(|| format!("open parent for {path}"))?;
                    let file = volume
                        .create_file(&parent, name)
                        .with_context(|| format!("create {path}"))?;
                    let mut w = volume.write_file(&file)?;
                    let mut done = 0;
                    while done < data.len() {
                        let k = w.write(&data[done..])?;
                        if k == 0 {
                            anyhow::bail!("stalled write on {path}");
                        }
                        done += k;
                    }
                    w.finish()?;
                    let n = i + 1;
                    if n % 250 == 0 || n == spec.iso_paths.len() {
                        eprintln!("media: tree {n}/{} files ...", spec.iso_paths.len());
                    }
                }
                tracing::info!("media: {} ISO files written", spec.iso_paths.len());
            }
            Err(e) => {
                eprintln!(
                    "media: hadris-udf open failed ({e:#}); falling back to isomage for {} files ...",
                    spec.iso_paths.len()
                );
                stream_iso_paths_isomage(&spec.iso, &spec.iso_paths, &seen, &volume)?;
                tracing::info!(
                    "media: {} ISO files written (isomage)",
                    spec.iso_paths.len()
                );
            }
        }
    }
    drop(volume);

    // Split-WIM parts, streamed (never whole in RAM).
    if !spec.swm_parts.is_empty() {
        let f = OpenOptions::new().read(true).write(true).open(&spec.out)?;
        let volume = FatVolume::open(PartitionView::new(f, part_off))?;
        let sources = open_path(&volume, "sources")?;
        for src_path in &spec.swm_parts {
            let name = src_path
                .file_name()
                .and_then(|n| n.to_str())
                .with_context(|| format!("bad SWM name {}", src_path.display()))?;
            let meta = std::fs::metadata(src_path)?;
            let file = volume.create_file(&sources, name)?;
            let mut w = volume.write_file(&file)?;
            let mut src = File::open(src_path)?;
            let mut chunk = vec![0u8; 64 * 1024 * 1024];
            let mut total_w = 0u64;
            loop {
                let n = src.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                let mut done = 0;
                while done < n {
                    let k = w.write(&chunk[done..n])?;
                    if k == 0 {
                        anyhow::bail!("stall {name}");
                    }
                    done += k;
                }
                total_w += n as u64;
                if total_w / (1024 * 1024 * 1024) != (total_w - n as u64) / (1024 * 1024 * 1024) {
                    eprintln!("media: {name} ... {:.1} GB", total_w as f64 / 1e9);
                }
            }
            w.finish()?;
            tracing::info!("media: {name} streamed ({total_w} of {} bytes)", meta.len());
            eprintln!("media: {name} done ({total_w} bytes)");
        }
        drop(volume);
    }
    Ok(())
}

/// Read ISO trees + extras + subtree-minus-skips into memory.
/// `install.wim` (7 GB) must be in `skip`, never here — it arrives via
/// `swm_parts` (split) instead.
///
/// Tries hadris-udf first; on failure (e.g. `invalid Information Control
/// Block` on images hadris rejects) retries the same selection via
/// isomage, which auto-detects ISO9660/UDF.
pub fn read_iso_tree(
    iso: &Path,
    trees: &[String],
    extra_files: &[String],
    full_subtree: Option<&(String, Vec<String>)>,
) -> Result<Vec<(String, Vec<u8>)>> {
    match read_iso_tree_hadris(iso, trees, extra_files, full_subtree) {
        Ok(v) => Ok(v),
        Err(e) => {
            eprintln!("media: hadris-udf failed ({e:#}); retrying with isomage fallback ...");
            read_iso_tree_isomage(iso, trees, extra_files, full_subtree)
        }
    }
}

fn read_iso_tree_hadris(
    iso: &Path,
    trees: &[String],
    extra_files: &[String],
    full_subtree: Option<&(String, Vec<String>)>,
) -> Result<Vec<(String, Vec<u8>)>> {
    let udf = UdfVolume::open(std::io::BufReader::new(File::open(iso)?))
        .with_context(|| format!("open UDF volume {}", iso.display()))?;
    eprintln!("media: reading ISO tree from {} ...", iso.display());
    let root = udf.root_dir()?;
    let mut out = Vec::new();
    for tree in trees {
        let entry = find_dir(&root, tree)?.ok_or_else(|| anyhow::anyhow!("no /{tree} in ISO"))?;
        let sub = udf.read_directory(&entry.icb)?;
        collect(&udf, &sub, tree, &mut out)?;
    }
    for only in extra_files {
        let parts: Vec<&str> = only.split('/').collect();
        let dir = open_udf_path(&udf, &parts[..parts.len() - 1])?
            .ok_or_else(|| anyhow::anyhow!("no dir for {only}"))?;
        let file =
            find_file(&dir, parts[parts.len() - 1])?.ok_or_else(|| anyhow::anyhow!("no {only}"))?;
        out.push((only.clone(), udf.read_file(&file)?));
    }
    if let Some((tree, skip)) = full_subtree {
        let dir = open_udf_path(&udf, &[tree])?.ok_or_else(|| anyhow::anyhow!("no /{tree}"))?;
        collect_except(&udf, &dir, tree, &mut out, skip)?;
    }
    Ok(out)
}

/// List file paths in ISO trees + extras + subtree-minus-skips **without
/// reading content**.  Use with [`read_file_from_udf`] to stream each file
/// one at a time during FAT32 image building.
///
/// Same hadris-first / isomage-fallback policy as [`read_iso_tree`].
pub fn list_iso_tree(
    iso: &Path,
    trees: &[String],
    extra_files: &[String],
    full_subtree: Option<&(String, Vec<String>)>,
) -> Result<Vec<String>> {
    match list_iso_tree_hadris(iso, trees, extra_files, full_subtree) {
        Ok(v) => Ok(v),
        Err(e) => {
            eprintln!("media: hadris-udf listing failed ({e:#}); retrying with isomage ...");
            list_iso_tree_isomage(iso, trees, extra_files, full_subtree)
        }
    }
}

fn list_iso_tree_hadris(
    iso: &Path,
    trees: &[String],
    extra_files: &[String],
    full_subtree: Option<&(String, Vec<String>)>,
) -> Result<Vec<String>> {
    let udf = UdfVolume::open(std::io::BufReader::new(File::open(iso)?))?;
    eprintln!("media: listing ISO tree from {} ...", iso.display());
    let root = udf.root_dir()?;
    let mut paths = Vec::new();
    for tree in trees {
        let entry = find_dir(&root, tree)?.ok_or_else(|| anyhow::anyhow!("no /{tree} in ISO"))?;
        let sub = udf.read_directory(&entry.icb)?;
        list_collect(&udf, &sub, tree, &mut paths)?;
    }
    for only in extra_files {
        paths.push(only.clone());
    }
    if let Some((tree, skip)) = full_subtree {
        let dir = open_udf_path(&udf, &[tree])?.ok_or_else(|| anyhow::anyhow!("no /{tree}"))?;
        list_collect_except(&udf, &dir, tree, &mut paths, skip)?;
    }
    eprintln!("media: listed {} files", paths.len());
    Ok(paths)
}

fn list_collect(udf: &SyncUdf, dir: &UdfDir, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    for e in dir.all_entries() {
        if e.is_parent() {
            continue;
        }
        let p = format!("{prefix}/{}", e.name());
        if e.is_dir() {
            let sub = udf.read_directory(&e.icb)?;
            list_collect(udf, &sub, &p, out)?;
        } else {
            out.push(p);
        }
    }
    Ok(())
}

fn list_collect_except(
    udf: &SyncUdf,
    dir: &UdfDir,
    prefix: &str,
    out: &mut Vec<String>,
    skip: &[String],
) -> Result<()> {
    for e in dir.all_entries() {
        if e.is_parent() {
            continue;
        }
        let p = format!("{prefix}/{}", e.name());
        if e.is_dir() {
            let sub = udf.read_directory(&e.icb)?;
            list_collect_except(udf, &sub, &p, out, skip)?;
        } else if !skip.iter().any(|s| e.name().eq_ignore_ascii_case(s)) {
            out.push(p);
        }
    }
    Ok(())
}

/// Read a single file from a UDF volume by relative path.
/// The caller should open the UDF volume once and call this for each path
/// to stream files without holding them all in RAM.
pub fn read_file_from_udf(udf: &SyncUdf, rel_path: &str) -> Result<Vec<u8>> {
    let parts: Vec<&str> = rel_path.split('/').collect();
    let root = udf.root_dir()?;
    navigate_and_read(udf, &root, &parts)
}

fn navigate_and_read(udf: &SyncUdf, dir: &UdfDir, parts: &[&str]) -> Result<Vec<u8>> {
    if parts.is_empty() {
        anyhow::bail!("empty path");
    }
    let part = parts[0];
    let is_last = parts.len() == 1;
    for e in dir.all_entries() {
        if e.is_parent() {
            continue;
        }
        if !e.name().eq_ignore_ascii_case(part) {
            continue;
        }
        if is_last {
            return udf.read_file(e).map_err(|e| anyhow::anyhow!("{e}"));
        } else if e.is_dir() {
            let sub = udf.read_directory(&e.icb)?;
            return navigate_and_read(udf, &sub, &parts[1..]);
        }
    }
    let full = parts.join("/");
    anyhow::bail!("not found in ISO: {full}")
}

// ---------------------------------------------------------------------------
// isomage fallback (ISO9660/UDF auto-detect)
//
// hadris-udf is strict UDF and rejects some genuine retail ISOs with
// `invalid Information Control Block`.  isomage already parses those
// (it is the reader used for the ISO9660 virtio-win image), so every
// hadris entry point above retries here before surfacing an error.
// Name matching stays case-insensitive to mirror the hadris helpers.
// ---------------------------------------------------------------------------

fn iso_find_ci<'a>(node: &'a isomage::TreeNode, comp: &str) -> Option<&'a isomage::TreeNode> {
    node.children
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(comp))
}

fn iso_find_path<'a>(root: &'a isomage::TreeNode, rel: &str) -> Option<&'a isomage::TreeNode> {
    let mut node = root;
    for comp in rel.split('/').filter(|c| !c.is_empty()) {
        node = iso_find_ci(node, comp)?;
    }
    Some(node)
}

fn iso_collect_files(
    file: &mut File,
    node: &isomage::TreeNode,
    prefix: &str,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    if node.is_directory {
        for child in &node.children {
            let p = if prefix.is_empty() {
                child.name.clone()
            } else {
                format!("{prefix}/{}", child.name)
            };
            if child.is_directory {
                iso_collect_files(file, child, &p, out)?;
            } else {
                let mut data = Vec::new();
                isomage::cat_node(file, child, &mut data)
                    .map_err(|e| anyhow::anyhow!("read {p}: {e}"))?;
                out.push((p, data));
            }
        }
        return Ok(());
    }
    let mut data = Vec::new();
    isomage::cat_node(file, node, &mut data).map_err(|e| anyhow::anyhow!("read {prefix}: {e}"))?;
    out.push((prefix.to_string(), data));
    Ok(())
}

fn iso_collect_files_except(
    file: &mut File,
    node: &isomage::TreeNode,
    prefix: &str,
    out: &mut Vec<(String, Vec<u8>)>,
    skip: &[String],
) -> Result<()> {
    if node.is_directory {
        for child in &node.children {
            let p = if prefix.is_empty() {
                child.name.clone()
            } else {
                format!("{prefix}/{}", child.name)
            };
            if child.is_directory {
                iso_collect_files_except(file, child, &p, out, skip)?;
            } else if !skip.iter().any(|s| child.name.eq_ignore_ascii_case(s)) {
                let mut data = Vec::new();
                isomage::cat_node(file, child, &mut data)
                    .map_err(|e| anyhow::anyhow!("read {p}: {e}"))?;
                out.push((p, data));
            }
        }
        return Ok(());
    }
    if !skip.iter().any(|s| node.name.eq_ignore_ascii_case(s)) {
        let mut data = Vec::new();
        isomage::cat_node(file, node, &mut data)
            .map_err(|e| anyhow::anyhow!("read {prefix}: {e}"))?;
        out.push((prefix.to_string(), data));
    }
    Ok(())
}

fn iso_list_paths(node: &isomage::TreeNode, prefix: &str, out: &mut Vec<String>) {
    if node.is_directory {
        for child in &node.children {
            let p = if prefix.is_empty() {
                child.name.clone()
            } else {
                format!("{prefix}/{}", child.name)
            };
            if child.is_directory {
                iso_list_paths(child, &p, out);
            } else {
                out.push(p);
            }
        }
    } else {
        out.push(prefix.to_string());
    }
}

fn iso_list_paths_except(
    node: &isomage::TreeNode,
    prefix: &str,
    out: &mut Vec<String>,
    skip: &[String],
) {
    if node.is_directory {
        for child in &node.children {
            let p = if prefix.is_empty() {
                child.name.clone()
            } else {
                format!("{prefix}/{}", child.name)
            };
            if child.is_directory {
                iso_list_paths_except(child, &p, out, skip);
            } else if !skip.iter().any(|s| child.name.eq_ignore_ascii_case(s)) {
                out.push(p);
            }
        }
    } else if !skip.iter().any(|s| node.name.eq_ignore_ascii_case(s)) {
        out.push(prefix.to_string());
    }
}

fn read_iso_tree_isomage(
    iso: &Path,
    trees: &[String],
    extra_files: &[String],
    full_subtree: Option<&(String, Vec<String>)>,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut file = File::open(iso).with_context(|| format!("open {}", iso.display()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let root = isomage::detect_and_parse_filesystem(&mut file, &iso.to_string_lossy())
        .map_err(|e| anyhow::anyhow!("isomage parse {} ({} bytes): {e}", iso.display(), size))?;
    eprintln!(
        "media: reading ISO tree via isomage from {} ...",
        iso.display()
    );
    let mut out = Vec::new();
    for tree in trees {
        let node =
            iso_find_path(&root, tree).ok_or_else(|| anyhow::anyhow!("no /{tree} in ISO"))?;
        iso_collect_files(&mut file, node, tree, &mut out)?;
    }
    for only in extra_files {
        let node = iso_find_path(&root, only).ok_or_else(|| anyhow::anyhow!("no {only} in ISO"))?;
        if node.is_directory {
            anyhow::bail!("{only} is a directory, not a file");
        }
        let mut data = Vec::new();
        isomage::cat_node(&mut file, node, &mut data)
            .map_err(|e| anyhow::anyhow!("read {only}: {e}"))?;
        out.push((only.clone(), data));
    }
    if let Some((tree, skip)) = full_subtree {
        let node = iso_find_path(&root, tree).ok_or_else(|| anyhow::anyhow!("no /{tree}"))?;
        iso_collect_files_except(&mut file, node, tree, &mut out, skip)?;
    }
    Ok(out)
}

fn list_iso_tree_isomage(
    iso: &Path,
    trees: &[String],
    extra_files: &[String],
    full_subtree: Option<&(String, Vec<String>)>,
) -> Result<Vec<String>> {
    let mut file = File::open(iso).with_context(|| format!("open {}", iso.display()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let root = isomage::detect_and_parse_filesystem(&mut file, &iso.to_string_lossy())
        .map_err(|e| anyhow::anyhow!("isomage parse {} ({} bytes): {e}", iso.display(), size))?;
    eprintln!(
        "media: listing ISO tree via isomage from {} ...",
        iso.display()
    );
    let mut paths = Vec::new();
    for tree in trees {
        let node =
            iso_find_path(&root, tree).ok_or_else(|| anyhow::anyhow!("no /{tree} in ISO"))?;
        iso_list_paths(node, tree, &mut paths);
    }
    for only in extra_files {
        paths.push(only.clone());
    }
    if let Some((tree, skip)) = full_subtree {
        let node = iso_find_path(&root, tree).ok_or_else(|| anyhow::anyhow!("no /{tree}"))?;
        iso_list_paths_except(node, tree, &mut paths, skip);
    }
    eprintln!("media: listed {} files (isomage)", paths.len());
    Ok(paths)
}

/// Read one ISO file via a lazily-parsed isomage tree. The caller keeps
/// `(File, TreeNode)` across files so a 1000-file tree parses once, not
/// once per file.
fn read_iso_single_isomage_cached(
    iso: &Path,
    rel_path: &str,
    cached: &mut Option<(File, isomage::TreeNode)>,
) -> Result<Vec<u8>> {
    if cached.is_none() {
        let mut file = File::open(iso).with_context(|| format!("open {}", iso.display()))?;
        let root = isomage::detect_and_parse_filesystem(&mut file, &iso.to_string_lossy())
            .map_err(|e| anyhow::anyhow!("isomage parse {}: {e}", iso.display()))?;
        *cached = Some((file, root));
    }
    let (file, root) = cached.as_mut().expect("just populated");
    let node = iso_find_path(root, rel_path)
        .ok_or_else(|| anyhow::anyhow!("not found in ISO: {rel_path}"))?;
    if node.is_directory {
        anyhow::bail!("{rel_path} is a directory, not a file");
    }
    let mut data = Vec::new();
    isomage::cat_node(file, node, &mut data)
        .map_err(|e| anyhow::anyhow!("read {rel_path}: {e}"))?;
    Ok(data)
}

/// Whole-tree isomage streaming for [`build_installer_media`] Pass 3: parse
/// once, then `cat_node` each requested path straight into the FAT volume.
/// `already_seen` mirrors the hadris path's duplicate suppression.
fn stream_iso_paths_isomage<D: hadris_fat::Read + hadris_fat::Seek + hadris_fat::Write>(
    iso: &Path,
    iso_paths: &[String],
    already_seen: &HashSet<String>,
    volume: &FatVolume<D>,
) -> Result<()> {
    let mut file = File::open(iso).with_context(|| format!("open {}", iso.display()))?;
    let root = isomage::detect_and_parse_filesystem(&mut file, &iso.to_string_lossy())
        .map_err(|e| anyhow::anyhow!("isomage parse {}: {e}", iso.display()))?;
    eprintln!(
        "media: streaming {} ISO files (isomage) ...",
        iso_paths.len()
    );
    let mut seen = already_seen.clone();
    for (i, path) in iso_paths.iter().enumerate() {
        if seen.contains(&path.to_lowercase()) {
            eprintln!("media: duplicate ISO entry skipped: {path}");
            continue;
        }
        seen.insert(path.to_lowercase());
        let node = iso_find_path(&root, path).ok_or_else(|| anyhow::anyhow!("no {path} in ISO"))?;
        if node.is_directory {
            anyhow::bail!("{path} is a directory, not a file");
        }
        // One file in RAM at a time (same peak as the hadris path;
        // the largest tree file is ~600 MB boot.wim).
        let mut data = Vec::new();
        isomage::cat_node(&mut file, node, &mut data)
            .map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        let parent = open_path(volume, dir).with_context(|| format!("open parent for {path}"))?;
        let out = volume
            .create_file(&parent, name)
            .with_context(|| format!("create {path}"))?;
        let mut w = volume.write_file(&out)?;
        let mut done = 0;
        while done < data.len() {
            let k = w.write(&data[done..])?;
            if k == 0 {
                anyhow::bail!("stalled write on {path}");
            }
            done += k;
        }
        w.finish()?;
        let n = i + 1;
        if n % 250 == 0 || n == iso_paths.len() {
            eprintln!("media: tree {n}/{} files ...", iso_paths.len());
        }
    }
    Ok(())
}

/// Stream one (possibly huge) ISO file straight to `out` on disk without
/// ever holding it in RAM. Used for `sources/install.wim` (~7 GB): the old
/// `read_iso_tree` + `fs::write` path needed that much RAM at once and
/// OOMed small Linux VMs. isomage streams in 8 MiB chunks; hadris is the
/// fallback (whole-file Vec) for images only it can parse.
pub fn extract_iso_file_to_disk(iso: &Path, rel_path: &str, out: &Path) -> Result<()> {
    // isomage first: streaming, no RAM spike, tolerates the ICB variants
    // hadris rejects.
    let stream = (|| -> Result<()> {
        let mut file = File::open(iso).with_context(|| format!("open {}", iso.display()))?;
        let root = isomage::detect_and_parse_filesystem(&mut file, &iso.to_string_lossy())
            .map_err(|e| anyhow::anyhow!("isomage parse {}: {e}", iso.display()))?;
        let node = iso_find_path(&root, rel_path)
            .ok_or_else(|| anyhow::anyhow!("no {rel_path} in ISO"))?;
        if node.is_directory {
            anyhow::bail!("{rel_path} is a directory, not a file");
        }
        let out_file = File::create(out).with_context(|| format!("create {}", out.display()))?;
        let mut writer = std::io::BufWriter::new(out_file);
        isomage::cat_node(&mut file, node, &mut writer)
            .map_err(|e| anyhow::anyhow!("read {rel_path}: {e}"))?;
        writer.flush()?;
        Ok(())
    })();
    if stream.is_ok() {
        return stream;
    }
    eprintln!(
        "media: isomage stream failed ({:#}); trying hadris-udf ...",
        stream.as_ref().unwrap_err()
    );
    let data = read_iso_tree_hadris(iso, &[], &[rel_path.to_string()], None)?
        .pop()
        .map(|(_, b)| b)
        .ok_or_else(|| anyhow::anyhow!("no {rel_path} in ISO"))?;
    std::fs::write(out, &data).with_context(|| format!("write {}", out.display()))?;
    Ok(())
}

/// NetKVM files out of virtio-win.iso (ISO9660 — hadris-udf rejects it;
/// isomage auto-detects). Returns `drivers/NetKVM/w11/<arch>/*` entries
/// ready for staging. Pure Rust, streamed, never whole in RAM.
pub fn extract_virtio_netkvm(
    iso: &Path,
    arch: crate::unattend::Architecture,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut file = File::open(iso)?;
    let root = isomage::detect_and_parse_filesystem(&mut file, crate::paths::VIRTIO_ISO)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", crate::paths::VIRTIO_ISO))?;
    fn find<'a>(node: &'a isomage::TreeNode, name: &str) -> Option<&'a isomage::TreeNode> {
        node.children
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }
    let mut node = &root;
    let arch_dir = arch.netkvm_iso_dir();
    for comp in ["NetKVM", "w11", arch_dir] {
        node = find(node, comp)
            .with_context(|| format!("{comp} missing in {}", crate::paths::VIRTIO_ISO))?;
    }
    let mut out = Vec::new();
    for f in [
        "netkvm.inf",
        "netkvm.sys",
        "netkvm.cat",
        "netkvmco.exe",
        "netkvmp.exe",
    ] {
        let child = find(node, f)
            .with_context(|| format!("{f} missing in {}", crate::paths::VIRTIO_ISO))?;
        let mut data = Vec::new();
        isomage::cat_node(&mut file, child, &mut data)
            .map_err(|e| anyhow::anyhow!("read {f}: {e}"))?;
        out.push((
            format!(
                "{}/NetKVM/w11/{arch_dir}/{f}",
                crate::paths::DRIVERS_STAGE_DIR
            ),
            data,
        ));
    }
    Ok(out)
}

/// Run `wimlib-imagex split <wim> <out-prefix> 4000` (vendored binary).
/// Produces `<prefix>` (`install.swm`) + `<stem>2.swm`.
pub fn split_wim(wimlib_bin: &Path, wim: &Path, out_prefix: &Path) -> Result<Vec<PathBuf>> {
    let st = std::process::Command::new(wimlib_bin)
        .args([
            "split",
            &wim.to_string_lossy(),
            &out_prefix.to_string_lossy(),
            "4000",
        ])
        .status()
        .with_context(|| format!("run {}", wimlib_bin.display()))?;
    if !st.success() {
        anyhow::bail!("wimlib-imagex split failed");
    }
    let stem = out_prefix
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("install");
    let parent = out_prefix.parent().unwrap_or(Path::new("."));
    let ext = out_prefix
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("swm");
    let mut parts = vec![out_prefix.to_path_buf()];
    let second = parent.join(format!("{stem}2.{ext}"));
    if second.is_file() {
        parts.push(second);
    }
    Ok(parts)
}

type SyncUdf = UdfVolume<std::io::BufReader<File>>;

fn find_file(dir: &UdfDir, name: &str) -> Result<Option<hadris_udf::dir::UdfDirEntry>> {
    for e in dir.all_entries() {
        if e.is_parent() || !e.is_file() || !e.name().eq_ignore_ascii_case(name) {
            continue;
        }
        return Ok(Some(e.clone()));
    }
    Ok(None)
}

fn open_udf_path(udf: &SyncUdf, comps: &[&str]) -> Result<Option<UdfDir>> {
    let mut cur = udf.root_dir()?;
    for c in comps {
        let Some(e) = find_dir(&cur, c)? else {
            return Ok(None);
        };
        cur = udf.read_directory(&e.icb)?;
    }
    Ok(Some(cur))
}

fn find_dir(dir: &UdfDir, name: &str) -> Result<Option<hadris_udf::dir::UdfDirEntry>> {
    for e in dir.all_entries() {
        if e.is_parent() || !e.is_dir() || !e.name().eq_ignore_ascii_case(name) {
            continue;
        }
        return Ok(Some(e.clone()));
    }
    Ok(None)
}

fn collect(
    udf: &SyncUdf,
    dir: &UdfDir,
    prefix: &str,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    for e in dir.all_entries() {
        if e.is_parent() {
            continue;
        }
        let p = format!("{prefix}/{}", e.name());
        if e.is_dir() {
            let sub = udf.read_directory(&e.icb)?;
            collect(udf, &sub, &p, out)?;
        } else {
            out.push((p, udf.read_file(e)?));
        }
    }
    Ok(())
}

fn collect_except(
    udf: &SyncUdf,
    dir: &UdfDir,
    prefix: &str,
    out: &mut Vec<(String, Vec<u8>)>,
    skip: &[String],
) -> Result<()> {
    for e in dir.all_entries() {
        if e.is_parent() {
            continue;
        }
        let p = format!("{prefix}/{}", e.name());
        if e.is_dir() {
            let sub = udf.read_directory(&e.icb)?;
            collect_except(udf, &sub, &p, out, skip)?;
        } else if !skip.iter().any(|s| e.name().eq_ignore_ascii_case(s)) {
            out.push((p, udf.read_file(e)?));
        }
    }
    Ok(())
}

fn mkdir_p<D: hadris_fat::Read + hadris_fat::Seek + hadris_fat::Write>(
    volume: &FatVolume<D>,
    path: &str,
    created: &mut HashSet<String>,
) -> Result<()> {
    let mut cur = String::new();
    for comp in path.split('/') {
        if comp.is_empty() {
            continue;
        }
        let parent_path = cur.clone();
        cur = if cur.is_empty() {
            comp.to_string()
        } else {
            format!("{cur}/{comp}")
        };
        if created.contains(&cur) {
            continue;
        }
        let parent = open_path(volume, &parent_path)?;
        volume.create_dir(&parent, comp)?;
        created.insert(cur.clone());
    }
    Ok(())
}

fn open_path<'a, D: hadris_fat::Read + hadris_fat::Seek>(
    volume: &'a FatVolume<D>,
    path: &str,
) -> Result<FatDir<'a, D>> {
    let root = volume.root_dir();
    let mut stack: Vec<FatDir<D>> = vec![root];
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        let top = stack.last().unwrap();
        let mut it = top.entries();
        let mut next = None;
        while let Some(entry) = it.next_entry() {
            let entry = entry?;
            if entry.name().eq_ignore_ascii_case(comp) {
                let e = entry
                    .as_entry()
                    .ok_or_else(|| anyhow::anyhow!("no entry"))?
                    .clone();
                next = Some(top.open_entry(&e)?);
                break;
            }
        }
        stack.push(next.ok_or_else(|| anyhow::anyhow!("{comp} missing in {path}"))?);
    }
    Ok(stack.pop().unwrap())
}

/// File view at a byte offset (partition access without a loop device).
struct PartitionView {
    f: File,
    base: u64,
    pos: u64,
}

impl PartitionView {
    fn new(f: File, base: u64) -> Self {
        Self { f, base, pos: 0 }
    }
}

impl Read for PartitionView {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.f.seek(SeekFrom::Start(self.base + self.pos))?;
        let n = self.f.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for PartitionView {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.f.seek(SeekFrom::Start(self.base + self.pos))?;
        let n = self.f.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.f.flush()
    }
}

impl Seek for PartitionView {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        let base = self.pos as i64;
        let n = match p {
            SeekFrom::Start(o) => o as i64,
            SeekFrom::End(o) => self.f.metadata()?.len() as i64 - self.base as i64 + o,
            SeekFrom::Current(o) => base + o,
        };
        if n < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = n as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical (RFC-4122) text form of a GPT GUID stored in the on-disk
    /// wire layout: Data1/Data2/Data3 little-endian, Data4 in file order.
    fn guid_text(bytes: [u8; 16]) -> String {
        let d1 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let d2 = u16::from_le_bytes([bytes[4], bytes[5]]);
        let d3 = u16::from_le_bytes([bytes[6], bytes[7]]);
        let tail: String = bytes[8..].iter().map(|b| format!("{b:02X}")).collect();
        format!(
            "{d1:08X}-{d2:04X}-{d3:04X}-{}-{}",
            &tail[0..4],
            &tail[4..16]
        )
    }

    /// Windows mounts an ESP hidden with no drive letter, and refuses to
    /// mount a GUID it does not know at all — either way Setup never sees
    /// `sources/install.swm` as media ("Install driver to show hardware").
    /// A swapped byte pair here (c087 vs 87c0) produced exactly that.
    #[test]
    fn basic_data_type_guid_is_microsoft_basic_data() {
        assert_eq!(
            guid_text(BASIC_DATA_TYPE_GUID),
            "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
        );
    }

    #[test]
    fn gpt_is_uefi_bootable_basic_data_that_fits_the_partition() {
        const SIZE: u64 = 64 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let mut img = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.path().join("t.img"))
            .unwrap();
        img.set_len(SIZE).unwrap();

        let (part_off, part_size) = write_gpt_partition_table(&mut img, SIZE, [0x11; 16]).unwrap();

        let mut buf = vec![0u8; 4096];
        // Protective MBR: not bootable, type 0xEE, 0x55AA signature.
        img.seek(SeekFrom::Start(0)).unwrap();
        img.read_exact(&mut buf[..512]).unwrap();
        assert_eq!(buf[446], 0x00, "protective MBR must not be bootable");
        assert_eq!(buf[450], 0xEE, "protective MBR type");
        assert_eq!(&buf[510..512], &[0x55, 0xAA]);

        // Partition entry 0.
        img.seek(SeekFrom::Start(2 * 512)).unwrap();
        img.read_exact(&mut buf[..128]).unwrap();
        assert_eq!(
            guid_text(buf[0..16].try_into().unwrap()),
            guid_text(BASIC_DATA_TYPE_GUID)
        );
        assert_eq!(
            &buf[48..56],
            &0x5u64.to_le_bytes(),
            "platform-required + legacy-bios-bootable"
        );
        let name = String::from_utf16_lossy(
            &buf[56..128]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect::<Vec<_>>(),
        );
        assert_eq!(name.trim_end_matches('\0'), "Basic data partition");
        let start = u64::from_le_bytes(buf[32..40].try_into().unwrap());
        let end = u64::from_le_bytes(buf[40..48].try_into().unwrap());
        assert_eq!(part_off, start * 512);
        assert_eq!(part_size, (end - start + 1) * 512);
        assert_eq!(
            part_size,
            SIZE - part_off - 33 * 512,
            "ends before backup GPT"
        );

        // Backup GPT header lives at the last sector.
        img.seek(SeekFrom::Start(SIZE - 512)).unwrap();
        img.read_exact(&mut buf[..8]).unwrap();
        assert_eq!(&buf[..8], b"EFI PART");
    }

    /// Requires the real virtio-win ISO in the dev cache (877 MB, not
    /// hermetic): `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn extracts_netkvm_from_virtio_iso() {
        let iso = Path::new(".cache/iso/virtio-win.iso");
        if !iso.is_file() {
            eprintln!("skipping: {iso:?} absent");
            return;
        }
        let files = extract_virtio_netkvm(iso, crate::unattend::Architecture::Arm64).unwrap();
        assert_eq!(files.len(), 5);
        for (p, data) in &files {
            assert!(p.starts_with("drivers/NetKVM/w11/arm64/"), "{p}");
            assert!(!data.is_empty(), "{p} empty");
        }
    }
}
