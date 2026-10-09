//! 平台分发:编译期选择后端实现。

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(not(target_os = "windows"))]
pub mod linux;

pub fn create_backend() -> Result<Box<dyn crate::traits::PlatformBackend>, String> {
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsBackend::new()))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(Box::new(linux::LinuxBackend::new()))
    }
}
