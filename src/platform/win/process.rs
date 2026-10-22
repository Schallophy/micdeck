//! Process facts that keep a source alive across a restart of the app it points at. A protected or
//! 64-bit process may refuse its module path, so identity can degrade to the image name.

use windows::core::PWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, HANDLE, STILL_ACTIVE,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Threading::GetProcessTimes;

pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    /// Empty when the OS will not tell us: a protected process, or one of another bitness.
    pub path: String,
    /// Creation time in 100 ns intervals since 1601, used only to break ties between same-named processes.
    pub created_100ns: u64,
}

pub fn self_pid() -> u32 {
    std::process::id()
}

fn open(pid: u32) -> Result<HANDLE, windows::core::Error> {
    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
}

/// Image name without the extension, the weaker half of a source's identity.
pub fn name(pid: u32) -> String {
    let text = path_or_none(pid);
    if let Some(full) = text {
        if let Some((_, file)) = full.rsplit_once('\\') {
            return trim_extension(file).to_string();
        }
    }
    super::devices::process_name(pid)
}

/// Full image path, or empty when the process will not disclose it.
pub fn path(pid: u32) -> String {
    path_or_none(pid).unwrap_or_default()
}

fn path_or_none(pid: u32) -> Option<String> {
    let handle = open(pid).ok()?;
    let mut buffer = [0u16; 4096];
    let mut size = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if result.is_err() || size == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..size as usize]))
}

/// Whether the process still exists. `GetExitCodeProcess` answers this without any rights beyond
/// `PROCESS_QUERY_LIMITED_INFORMATION`, which even a protected process grants; when the open itself is
/// refused the process is there but hidden from us, so it counts as alive.
pub fn alive(pid: u32) -> bool {
    match open(pid) {
        Ok(handle) => {
            let mut code = 0u32;
            let got = unsafe { GetExitCodeProcess(handle, &mut code) };
            unsafe {
                let _ = CloseHandle(handle);
            }
            if got.is_err() {
                return true;
            }
            code == STILL_ACTIVE.0 as u32
        }
        Err(err) => {
            if err.code() == ERROR_ACCESS_DENIED.to_hresult() {
                return true;
            }
            // Anything else: fall back to the process list, which does not need rights.
            super::devices::process_name(pid).len() > 0
        }
    }
}

fn created_100ns(pid: u32) -> u64 {
    let handle = match open(pid) {
        Ok(handle) => handle,
        Err(_) => return 0,
    };
    let mut creation = windows::Win32::Foundation::FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    let got = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if got.is_err() {
        return 0;
    }
    ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64
}

fn trim_extension(text: &str) -> &str {
    let dot = text.rfind('.');
    match dot {
        Some(index) if text[index..].eq_ignore_ascii_case(".exe") => &text[..index],
        _ => text,
    }
}

/// Every process whose image name matches, with the facts needed to pick the right one.
pub fn find_by_name(wanted: &str) -> Vec<ProcessInfo> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut out = Vec::new();
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(handle) => handle,
        Err(_) => return out,
    };
    let mut entry = PROCESSENTRY32W::default();
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    unsafe {
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let file = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if trim_extension(&file).eq_ignore_ascii_case(wanted) && entry.th32ProcessID != self_pid() {
                    let pid = entry.th32ProcessID;
                    out.push(ProcessInfo {
                        pid,
                        name: trim_extension(&file).to_string(),
                        path: path(pid),
                        created_100ns: created_100ns(pid),
                    });
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    out.sort_by_key(|p| p.created_100ns);
    out
}

/// The engine-facing shape: a process the user might route, with the two facts that make an identity.
#[derive(Clone, Debug)]
pub struct ProcessFacts {
    pub pid: u32,
    pub name: String,
    pub path: String,
}

pub fn processes_named(wanted: &str) -> Vec<ProcessFacts> {
    find_by_name(wanted)
        .into_iter()
        .map(|info| ProcessFacts {
            pid: info.pid,
            name: info.name,
            path: info.path,
        })
        .collect()
}

/// Processes that currently hold an audio render session. Several processes share one image name
/// (browser children, launchers); the one making the sound is the one with a session.
pub fn session_pids() -> Vec<u32> {
    super::devices::render_sessions(0.2)
        .unwrap_or_default()
        .into_iter()
        .map(|session| session.pid)
        .filter(|pid| *pid != 0)
        .collect()
}
