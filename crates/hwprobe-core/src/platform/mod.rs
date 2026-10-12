//! 平台分发:编译期选择后端实现。

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub mod linux;

pub fn create_backend() -> Result<Box<dyn crate::traits::PlatformBackend>, String> {
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsBackend::new()))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacOSBackend::new()))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Ok(Box::new(linux::LinuxBackend::new()))
    }
}
