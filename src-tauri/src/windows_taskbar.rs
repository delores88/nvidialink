//! Windows shell identity and the large icon that Tao keeps separate from its title-bar icon.

use std::{ffi::c_void, io, ptr};

const IMAGE_ICON: u32 = 1;
const LR_DEFAULTSIZE: u32 = 0x0040;
const LR_SHARED: u32 = 0x8000;
const WM_SETICON: u32 = 0x0080;
const WM_GETICON: u32 = 0x007f;
const ICON_BIG: usize = 1;

#[link(name = "shell32")]
unsafe extern "system" {
    fn SetCurrentProcessExplicitAppUserModelID(app_id: *const u16) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(module_name: *const u16) -> *mut c_void;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn LoadImageW(
        instance: *mut c_void,
        name: *const u16,
        image_type: u32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> *mut c_void;
    fn SendMessageW(window: *mut c_void, message: u32, wparam: usize, lparam: isize) -> isize;
}

pub fn set_app_id(identifier: &str) -> io::Result<()> {
    if identifier.is_empty() || identifier.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows taskbar identity must be a nonempty string without nulls",
        ));
    }
    let wide: Vec<u16> = identifier.encode_utf16().chain(Some(0)).collect();
    // SAFETY: the terminated UTF-16 buffer remains live during this synchronous call.
    // main calls this before the Tauri builder creates or presents any windows.
    let result = unsafe { SetCurrentProcessExplicitAppUserModelID(wide.as_ptr()) };
    if result < 0 {
        return Err(io::Error::other(format!(
            "Windows taskbar identity failed (HRESULT 0x{:08x})",
            result as u32
        )));
    }
    Ok(())
}

pub fn set_taskbar_icon(window: &tauri::WebviewWindow) -> io::Result<()> {
    let hwnd = window.hwnd().map_err(io::Error::other)?.0;
    // SAFETY: null selects this process's executable module, which stays loaded for
    // the entire process lifetime. The resource ID is the one used by tauri-build.
    let module = unsafe { GetModuleHandleW(ptr::null()) };
    if module.is_null() {
        return Err(io::Error::last_os_error());
    }
    let resource = tauri::utils::platform::WINDOWS_APP_ICON_RESOURCE_ID as usize as *const u16;
    // SAFETY: resource is MAKEINTRESOURCE for the embedded multi-size ICO. Default
    // dimensions select the system icon size. LR_SHARED makes Windows own and retain
    // the handle; it must not be DestroyIcon'd when this function returns.
    let icon = unsafe {
        LoadImageW(
            module,
            resource,
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE | LR_SHARED,
        )
    };
    if icon.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: setup runs on the UI thread with the live main window. WM_SETICON's
    // return value is the previous icon, so verify success through WM_GETICON instead.
    // Only ICON_BIG changes; Tao continues owning its existing small title-bar icon.
    unsafe {
        SendMessageW(hwnd, WM_SETICON, ICON_BIG, icon as isize);
        if SendMessageW(hwnd, WM_GETICON, ICON_BIG, 0) != icon as isize {
            return Err(io::Error::other("Windows did not retain the taskbar icon"));
        }
    }
    Ok(())
}
