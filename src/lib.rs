#[cfg(feature = "downloader")]
pub mod downloader;
pub mod qemu;
#[cfg(feature = "downloader")]
pub mod qemu_portable;
pub mod qmp;
pub mod ssh;
pub mod storage;
pub mod unattend;

pub use qemu::{HeadlessVm, HeadlessVmBuilder, WindowsVersion, qmp_alive};
#[cfg(feature = "downloader")]
pub use qemu_portable::PortableQemu;
pub use storage::DiskManager;
pub use unattend::{Architecture, VmConfig, auto_detect_platform};
