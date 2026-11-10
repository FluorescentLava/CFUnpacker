//! Attach the installed Windows App Runtime through Microsoft's native bootstrapper.
//!
//! The small adjacent bootstrap DLL is an installer/loader, not a bundled runtime.
//! Call this before initializing COM or activating any WinUI class, and keep the
//! returned guard alive until all WinUI objects have been released.

use std::ffi::{c_char, c_void};
use std::fmt;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

// Microsoft.WindowsAppSDK.Runtime 2.3.1 / include/WindowsAppSDK-VersionInfo.h.
pub const RELEASE_MAJOR_MINOR: u32 = 0x0002_0003;
pub const MIN_RUNTIME_VERSION: u64 = 0x0002_0003_0001_0000;
const ON_NO_MATCH_SHOW_UI: u32 = 0x0008;
const BOOTSTRAP_FILE: &str = "Microsoft.WindowsAppRuntime.Bootstrap.dll";

type Initialize = unsafe extern "system" fn(u32, *const u16, u64, u32) -> i32;
type Shutdown = unsafe extern "system" fn();

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(path: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(window: *mut c_void, text: *const u16, caption: *const u16, flags: u32) -> i32;
}

#[derive(Debug)]
pub struct BootstrapError {
    message: String,
    /// The official bootstrapper owns any missing-runtime UI after this point.
    /// Avoid displaying a second application dialog after the user closes it.
    pub official_prompt_requested: bool,
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BootstrapError {}

impl BootstrapError {
    fn before_initialization(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            official_prompt_requested: false,
        }
    }

    /// Shows a friendly error for a missing/corrupt loader. The native bootstrapper
    /// already owns the missing-runtime installation prompt, so do not duplicate it.
    pub fn show_dialog(&self) {
        if !self.official_prompt_requested {
            show_message(&self.message);
        }
    }
}

pub struct Bootstrap {
    module: *mut c_void,
    shutdown: Shutdown,
    // Runtime initialization and teardown stay on the application thread.
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for Bootstrap {
    fn drop(&mut self) {
        unsafe {
            (self.shutdown)();
            FreeLibrary(self.module);
        }
    }
}

pub fn initialize() -> Result<Bootstrap, BootstrapError> {
    let executable = std::env::current_exe().map_err(|error| {
        BootstrapError::before_initialization(format!("无法确定程序目录：{error}"))
    })?;
    let directory = executable.parent().ok_or_else(|| {
        BootstrapError::before_initialization("无法确定程序目录。请重新解压完整发行包。")
    })?;
    initialize_from(&directory.join(BOOTSTRAP_FILE))
}

fn initialize_from(path: &Path) -> Result<Bootstrap, BootstrapError> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // Restrict dependent DLL lookup to the loader's directory and normal system
    // locations; never search the current working directory or an arbitrary PATH.
    const SEARCH_DLL_LOAD_DIR: u32 = 0x0000_0100;
    const SEARCH_DEFAULT_DIRS: u32 = 0x0000_1000;
    let module = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            ptr::null_mut(),
            SEARCH_DLL_LOAD_DIR | SEARCH_DEFAULT_DIRS,
        )
    };
    if module.is_null() {
        return Err(BootstrapError::before_initialization(format!(
            "无法加载 Windows App Runtime 引导文件。\n\n请将完整发行包解压到同一个文件夹，保留 {BOOTSTRAP_FILE}。\n\n{}\n{}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    let initialize = unsafe { GetProcAddress(module, c"MddBootstrapInitialize2".as_ptr()) };
    let shutdown = unsafe { GetProcAddress(module, c"MddBootstrapShutdown".as_ptr()) };
    if initialize.is_null() || shutdown.is_null() {
        unsafe {
            FreeLibrary(module);
        }
        return Err(BootstrapError::before_initialization(
            "Windows App Runtime 引导文件不完整或版本不匹配。请重新解压完整发行包。",
        ));
    }
    let initialize: Initialize = unsafe { std::mem::transmute(initialize) };
    let shutdown: Shutdown = unsafe { std::mem::transmute(shutdown) };
    // An empty/null tag selects the stable release. OnNoMatch_ShowUI invokes the
    // official Windows App Runtime download/install prompt if no version matches.
    let result = unsafe {
        initialize(
            RELEASE_MAJOR_MINOR,
            ptr::null(),
            MIN_RUNTIME_VERSION,
            ON_NO_MATCH_SHOW_UI,
        )
    };
    if result < 0 {
        unsafe {
            FreeLibrary(module);
        }
        return Err(BootstrapError {
            message: format!(
                "Windows App Runtime 初始化失败 (0x{:08X})。需要 x64 Windows App Runtime 2.3.1 或更新的 2.x 版本；请完成系统安装提示后重新运行。",
                result as u32
            ),
            // Microsoft's MddBootstrap.cpp invokes OnNoMatch_ShowUI only for
            // STATEREPOSITORY_E_DEPENDENCY_NOT_RESOLVED (winerror.h). Other
            // failures still need our friendly startup error dialog.
            official_prompt_requested: result as u32 == 0x8067_0016,
        });
    }
    Ok(Bootstrap {
        module,
        shutdown,
        _thread: std::marker::PhantomData,
    })
}

fn show_message(message: &str) {
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "CFUnpacker 无法启动"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        MessageBoxW(ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x10);
    }
}
