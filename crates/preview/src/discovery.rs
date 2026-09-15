//! Observe only this user's listeners. HTTP probes run only after project
//! ownership has been established from the process's actual working directory.
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct Listener {
    pub pid: u32,
    pub parent: u32,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub started_at: u64,
    pub address: SocketAddr,
    pub zeron_owned: bool,
}

impl Listener {
    pub fn belongs_to(&self, root: &Path) -> bool {
        self.cwd.starts_with(root)
    }
}

/// Preserve a useful framework label without advertising process arguments,
/// which may contain credentials. Rank frontends above generic HTTP services.
pub fn framework(args: &[String]) -> (&'static str, u8) {
    for (needle, name) in [
        ("vite", "Vite"),
        ("next", "Next.js"),
        ("astro", "Astro"),
        ("miniflare", "Miniflare"),
    ] {
        if args.iter().any(|arg| {
            let arg = arg.to_lowercase();
            Path::new(&arg).file_stem().and_then(|s| s.to_str()) == Some(needle)
                || arg.contains(&format!("/{needle}/"))
                || arg.starts_with(&format!("{needle}-server"))
        }) {
            return (name, if needle == "miniflare" { 80 } else { 100 });
        }
    }
    if args.first().is_some_and(|s| {
        // `file_stem` (not `file_name`) so a Windows `node.exe` matches the
        // same way `node` does on unix; identical to `file_name` there since
        // extensionless unix binaries have no stem/name difference.
        Path::new(s)
            .file_stem()
            .is_some_and(|n| n == "node" || n == "bun" || n == "deno")
    }) {
        ("Node HTTP server", 50)
    } else {
        ("HTTP server", 40)
    }
}

/// Port flags are operational settings, never part of a service identity.
pub fn command_identity(args: &[String]) -> String {
    let mut skip = false;
    let python_http = args.iter().any(|arg| arg == "http.server");
    args.iter()
        .filter_map(|arg| {
            if skip {
                skip = false;
                return None;
            }
            if matches!(arg.as_str(), "--port" | "-p" | "--inspect-port") {
                skip = true;
                return None;
            }
            if arg.starts_with("--port=")
                || arg.starts_with("PORT=")
                || arg.starts_with("--inspect-port=")
            {
                return None;
            }
            if python_http && arg.parse::<u16>().is_ok() {
                return None;
            }
            Some(arg.as_str())
        })
        .collect::<Vec<_>>()
        .join("\0")
}

pub async fn is_http(address: SocketAddr) -> bool {
    tokio::time::timeout(Duration::from_millis(800), async move {
        let mut socket = tokio::net::TcpStream::connect(address).await.ok()?;
        socket
            .write_all(
                format!(
                    "HEAD / HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
                    address.port()
                )
                .as_bytes(),
            )
            .await
            .ok()?;
        let mut prefix = [0; 12];
        socket.read_exact(&mut prefix).await.ok()?;
        let version = &prefix[..9];
        let status = std::str::from_utf8(&prefix[9..])
            .ok()?
            .parse::<u16>()
            .ok()?;
        ((version == b"HTTP/1.1 " || version == b"HTTP/1.0 ") && (100..600).contains(&status))
            .then_some(())
    })
    .await
    .ok()
    .flatten()
    .is_some()
}

fn mark_descendants(listeners: &mut [Listener], parents: &HashMap<u32, u32>, owner: u32) {
    for listener in listeners {
        let mut pid = listener.pid;
        for _ in 0..128 {
            if pid == owner {
                listener.zeron_owned = true;
                break;
            }
            let Some(&parent) = parents.get(&pid) else {
                break;
            };
            if parent == pid || parent == 0 {
                break;
            }
            pid = parent;
        }
    }
}

/// Process start in ms since the epoch, derived the same way for the scanner
/// and for [`same_process`] so the two agree exactly.
#[cfg(target_os = "linux")]
fn linux_clock() -> (u64, u64) {
    let boot = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|line| {
                line.strip_prefix("btime ")
                    .and_then(|v| v.parse::<u64>().ok())
            })
        })
        .unwrap_or(0);
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    (boot, ticks)
}
#[cfg(target_os = "linux")]
fn linux_started_at(stat_fields: &[&str], boot: u64, ticks: u64) -> u64 {
    boot * 1000
        + stat_fields
            .get(19)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            * 1000
            / ticks
}

/// Whether `pid` is still the process the scanner observed with `started_at`.
/// Cheap enough to run per proxied connection; a full [`listeners`] pass
/// walks every process (and spawns lsof/ps on macOS), which the proxy used
/// to do for every asset a remote page requested.
#[cfg(target_os = "linux")]
pub fn same_process(pid: u32, started_at: u64) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, tail)) = stat.rsplit_once(") ") else {
        return false;
    };
    let fields: Vec<_> = tail.split_whitespace().collect();
    let (boot, ticks) = linux_clock();
    linux_started_at(&fields, boot, ticks) == started_at
}

#[cfg(target_os = "linux")]
pub fn listeners() -> Vec<Listener> {
    use std::{fs, os::unix::fs::MetadataExt};
    let mut sockets = HashMap::new();
    for (file, ipv6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        if let Ok(text) = fs::read_to_string(file) {
            for line in text.lines().skip(1) {
                let parts: Vec<_> = line.split_whitespace().collect();
                if parts.len() <= 9 || parts[3] != "0A" {
                    continue;
                }
                if let Some(address) = proc_address(parts[1], ipv6) {
                    sockets.insert(parts[9].to_string(), address);
                }
            }
        }
    }
    let (boot, ticks) = linux_clock();
    let uid = unsafe { libc::geteuid() };
    let mut result = Vec::new();
    let mut parents = HashMap::new();
    let Ok(processes) = fs::read_dir("/proc") else {
        return result;
    };
    for process in processes.flatten() {
        let Ok(pid) = process.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let path = process.path();
        if !fs::metadata(&path).is_ok_and(|m| m.uid() == uid) {
            continue;
        }
        let Ok(stat) = fs::read_to_string(path.join("stat")) else {
            continue;
        };
        let Some((_, tail)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<_> = tail.split_whitespace().collect();
        let Some(parent) = fields.get(1).and_then(|v| v.parse().ok()) else {
            continue;
        };
        parents.insert(pid, parent);
        let Ok(cwd) = fs::read_link(path.join("cwd")) else {
            continue;
        };
        let started_at = linux_started_at(&fields, boot, ticks);
        let args = fs::read(path.join("cmdline"))
            .unwrap_or_default()
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>();
        let Ok(fds) = fs::read_dir(path.join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(link) = fs::read_link(fd.path()) else {
                continue;
            };
            let link = link.to_string_lossy();
            let Some(inode) = link
                .strip_prefix("socket:[")
                .and_then(|s| s.strip_suffix(']'))
            else {
                continue;
            };
            if let Some(&address) = sockets.get(inode) {
                result.push(Listener {
                    pid,
                    parent,
                    cwd: cwd.clone(),
                    args: args.clone(),
                    started_at,
                    address,
                    zeron_owned: false,
                });
            }
        }
    }
    mark_descendants(&mut result, &parents, std::process::id());
    result.sort_by_key(|l| (l.address, l.pid));
    result.dedup_by_key(|l| (l.address, l.pid));
    result
}

#[cfg(target_os = "linux")]
fn proc_address(value: &str, ipv6: bool) -> Option<SocketAddr> {
    let (host, port) = value.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip = if ipv6 {
        if host.len() != 32 {
            return None;
        }
        let mut bytes = [0; 16];
        for (i, chunk) in host.as_bytes().chunks_exact(8).enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(
                &u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16)
                    .ok()?
                    .to_le_bytes(),
            );
        }
        IpAddr::V6(Ipv6Addr::from(bytes))
    } else {
        IpAddr::V4(Ipv4Addr::from(
            u32::from_str_radix(host, 16).ok()?.to_le_bytes(),
        ))
    };
    if !ip.is_loopback() && !ip.is_unspecified() {
        return None;
    }
    Some(SocketAddr::new(
        if ip.is_unspecified() {
            if ipv6 {
                Ipv6Addr::LOCALHOST.into()
            } else {
                Ipv4Addr::LOCALHOST.into()
            }
        } else {
            ip
        },
        port,
    ))
}

/// `ps lstart=` tokens ("Thu Sep 11 16:14:42 2026") to ms since the epoch.
#[cfg(target_os = "macos")]
fn parse_lstart(parts: &[&str]) -> u64 {
    use chrono::TimeZone;
    chrono::NaiveDateTime::parse_from_str(&parts.join(" "), "%a %b %e %T %Y")
        .ok()
        .and_then(|d| chrono::Local.from_local_datetime(&d).earliest())
        .map(|d| d.timestamp_millis().max(0) as u64)
        .unwrap_or(0)
}

/// See the Linux variant. One `ps -p` for a single pid instead of two lsof
/// passes and a full `ps -ax` per proxied connection.
#[cfg(target_os = "macos")]
pub fn same_process(pid: u32, started_at: u64) -> bool {
    let Ok(output) = std::process::Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<_> = text.split_whitespace().collect();
    parts.len() >= 5 && parse_lstart(&parts[..5]) == started_at
}

#[cfg(target_os = "macos")]
pub fn listeners() -> Vec<Listener> {
    use std::process::Command;
    let uid = unsafe { libc::geteuid() }.to_string();
    let fields = |arguments: &[&str]| -> Vec<(u32, String)> {
        let Ok(output) = Command::new("/usr/sbin/lsof").args(arguments).output() else {
            return Vec::new();
        };
        let mut pid = 0;
        let mut values = Vec::new();
        for field in output.stdout.split(|b| *b == 0) {
            let field = String::from_utf8_lossy(field);
            let field = field.trim_start_matches('\n');
            if let Some(value) = field.strip_prefix('p') {
                pid = value.parse().unwrap_or(0);
            }
            if let Some(value) = field.strip_prefix('n') {
                values.push((pid, value.to_owned()));
            }
        }
        values
    };
    let cwds: HashMap<_, _> = fields(&["-nP", "-a", "-u", &uid, "-d", "cwd", "-F0pn"])
        .into_iter()
        .collect();
    let mut parents = HashMap::new();
    let mut processes = HashMap::new();
    if let Ok(output) = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,lstart=,command="])
        .env("LC_ALL", "C")
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() < 8 {
                continue;
            }
            let (Ok(pid), Ok(parent)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) else {
                continue;
            };
            parents.insert(pid, parent);
            let started_at = parse_lstart(&parts[2..7]);
            processes.insert(
                pid,
                (
                    parent,
                    started_at,
                    parts[7..].iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                ),
            );
        }
    }
    let mut result = Vec::new();
    for (pid, socket) in fields(&["-nP", "-a", "-u", &uid, "-iTCP", "-sTCP:LISTEN", "-F0pn"]) {
        let Some(cwd) = cwds.get(&pid).and_then(|s| std::fs::canonicalize(s).ok()) else {
            continue;
        };
        let Some((parent, started_at, args)) = processes.get(&pid) else {
            continue;
        };
        let addresses = if let Some(port) = socket
            .strip_prefix("*:")
            .and_then(|p| p.parse::<u16>().ok())
        {
            vec![
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
            ]
        } else {
            socket
                .parse::<SocketAddr>()
                .ok()
                .filter(|a| a.ip().is_loopback())
                .into_iter()
                .collect()
        };
        for address in addresses {
            result.push(Listener {
                pid,
                parent: *parent,
                cwd: cwd.clone(),
                args: args.clone(),
                started_at: *started_at,
                address,
                zeron_owned: false,
            });
        }
    }
    mark_descendants(&mut result, &parents, std::process::id());
    result
}

/// Windows: owning pid per listening socket comes from `GetExtendedTcpTable`
/// (iphlpapi), the parent chain from a Toolhelp32 process snapshot, and
/// process identity (start time, cwd, command line) from `OpenProcess` plus
/// a `NtQueryInformationProcess`/`ReadProcessMemory` walk of the target's PEB
/// (the same technique Task Manager and Process Explorer use — there is no
/// documented API for another process's command line or working directory).
/// `OpenProcess` with only `PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ`
/// fails for processes this user cannot access (elevated processes, other
/// users, most system processes) without requiring an explicit uid check as
/// Linux/macOS do — failure to open, or to read the PEB, is treated the same
/// as "not ours" and the listener is skipped.
#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use std::ffi::c_void;
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6TABLE_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
        MIB_TCP_STATE_LISTEN, TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    // `ntdll!NtQueryInformationProcess`, `ProcessBasicInformation` (class 0) —
    // documented by Microsoft (`winternl.h`) for exactly this purpose, and
    // stable across every NT release. Declared by hand rather than pulled
    // from a crate feature: the signature is small and the ABI has not moved
    // in decades.
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(
            process_handle: HANDLE,
            process_information_class: i32,
            process_information: *mut c_void,
            process_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    /// Mirrors `ntdll`'s `PROCESS_BASIC_INFORMATION` (documented layout).
    #[repr(C)]
    struct ProcessBasicInformation {
        exit_status: i32,
        peb_base_address: usize,
        affinity_mask: usize,
        base_priority: i32,
        unique_process_id: usize,
        inherited_from_unique_process_id: usize,
    }

    /// 100ns ticks between the Windows epoch (1601-01-01) and the Unix epoch.
    const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;

    fn filetime_to_unix_ms(time: FILETIME) -> u64 {
        let ticks = ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
        ticks.saturating_sub(EPOCH_DIFF_100NS) / 10_000
    }

    fn open_process(pid: u32) -> Option<HANDLE> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                0,
                pid,
            )
        };
        (!handle.is_null()).then_some(handle)
    }

    pub(super) fn process_started_at(process: HANDLE) -> Option<u64> {
        let mut creation: FILETIME = unsafe { std::mem::zeroed() };
        let mut exit: FILETIME = unsafe { std::mem::zeroed() };
        let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
        let mut user: FILETIME = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
        (ok != 0).then(|| filetime_to_unix_ms(creation))
    }

    unsafe fn read_value<T: Copy>(process: HANDLE, address: usize) -> Option<T> {
        let mut value = std::mem::MaybeUninit::<T>::uninit();
        let mut read = 0usize;
        let ok = unsafe {
            ReadProcessMemory(
                process,
                address as *const c_void,
                value.as_mut_ptr() as *mut c_void,
                size_of::<T>(),
                &mut read,
            )
        };
        (ok != 0 && read == size_of::<T>()).then(|| unsafe { value.assume_init() })
    }

    fn read_wide_string(process: HANDLE, address: usize, length_bytes: u16) -> Option<String> {
        if address == 0 || length_bytes == 0 {
            return Some(String::new());
        }
        let mut buffer = vec![0u16; length_bytes as usize / 2];
        let mut read = 0usize;
        let ok = unsafe {
            ReadProcessMemory(
                process,
                address as *const c_void,
                buffer.as_mut_ptr() as *mut c_void,
                length_bytes as usize,
                &mut read,
            )
        };
        (ok != 0 && read == length_bytes as usize).then(|| String::from_utf16_lossy(&buffer))
    }

    /// Best-effort split of a raw Windows command line into argv-like tokens
    /// (double-quote runs treated as one token). Not a full
    /// `CommandLineToArgvW`-compatible parser (no backslash-escape handling),
    /// but [`framework`]/[`command_identity`] only need approximate tokens.
    fn split_command_line(command_line: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        for c in command_line.chars() {
            match c {
                '"' => in_quotes = !in_quotes,
                c if c.is_whitespace() && !in_quotes => {
                    if !current.is_empty() {
                        args.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            args.push(current);
        }
        args
    }

    /// Only 64-bit target processes are supported: the `RTL_USER_PROCESS_PARAMETERS`
    /// offsets below are the x64 layout. This build only ever targets
    /// `x86_64-pc-windows-msvc`, and a foreign-bitness target process (rare,
    /// and unreadable without WOW64 gymnastics) simply yields `None` here,
    /// which callers treat as "not ours".
    fn process_cwd_and_args(process: HANDLE) -> Option<(PathBuf, Vec<String>)> {
        let mut info: ProcessBasicInformation = unsafe { std::mem::zeroed() };
        let mut returned = 0u32;
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                0, // ProcessBasicInformation
                &mut info as *mut _ as *mut c_void,
                size_of::<ProcessBasicInformation>() as u32,
                &mut returned,
            )
        };
        if status != 0 || info.peb_base_address == 0 {
            return None;
        }
        // PEB.ProcessParameters (offset 0x20 on x64).
        let params: usize = unsafe { read_value(process, info.peb_base_address + 0x20)? };
        if params == 0 {
            return None;
        }
        // RTL_USER_PROCESS_PARAMETERS.CurrentDirectory.DosPath (UNICODE_STRING at 0x38).
        let cwd_len: u16 = unsafe { read_value(process, params + 0x38)? };
        let cwd_buf: usize = unsafe { read_value(process, params + 0x40)? };
        // RTL_USER_PROCESS_PARAMETERS.CommandLine (UNICODE_STRING at 0x70).
        let cmd_len: u16 = unsafe { read_value(process, params + 0x70)? };
        let cmd_buf: usize = unsafe { read_value(process, params + 0x78)? };
        let cwd = read_wide_string(process, cwd_buf, cwd_len)?;
        if cwd.is_empty() {
            return None;
        }
        // The PEB's `CurrentDirectory` is a plain `C:\...` path; project roots
        // are matched via `Listener::belongs_to` (a `starts_with` check)
        // against canonicalized roots, which on Windows carry the `\\?\`
        // verbatim prefix. Canonicalize here too — exactly what the macOS
        // `listeners()` does with its `lsof`-reported cwd — so the two sides
        // of that comparison agree. A path that no longer canonicalizes
        // (deleted, or a stale/inaccessible mount) is treated as unreadable.
        let cwd = std::fs::canonicalize(cwd).ok()?;
        let command_line = read_wide_string(process, cmd_buf, cmd_len).unwrap_or_default();
        Some((cwd, split_command_line(&command_line)))
    }

    fn ipv4_listeners() -> Vec<(SocketAddr, u32)> {
        let mut size: u32 = 0;
        unsafe {
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &mut size,
                0,
                AF_INET as u32,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            );
        }
        if size == 0 {
            return Vec::new();
        }
        let mut buffer = vec![0u8; size as usize];
        let rc = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr() as *mut c_void,
                &mut size,
                0,
                AF_INET as u32,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if rc != 0 {
            return Vec::new();
        }
        let table = unsafe { &*(buffer.as_ptr() as *const MIB_TCPTABLE_OWNER_PID) };
        let rows = unsafe {
            std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize)
        };
        rows.iter()
            .filter(|row| row.dwState == MIB_TCP_STATE_LISTEN as u32)
            .map(|row| {
                let ip = Ipv4Addr::from(u32::from_be(row.dwLocalAddr));
                let ip = if ip.is_unspecified() { Ipv4Addr::LOCALHOST } else { ip };
                let port = u16::from_be(row.dwLocalPort as u16);
                (ip, port, row.dwOwningPid)
            })
            .filter(|(ip, _, _)| ip.is_loopback())
            .map(|(ip, port, pid)| (SocketAddr::new(IpAddr::V4(ip), port), pid))
            .collect()
    }

    fn ipv6_listeners() -> Vec<(SocketAddr, u32)> {
        let mut size: u32 = 0;
        unsafe {
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &mut size,
                0,
                AF_INET6 as u32,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            );
        }
        if size == 0 {
            return Vec::new();
        }
        let mut buffer = vec![0u8; size as usize];
        let rc = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr() as *mut c_void,
                &mut size,
                0,
                AF_INET6 as u32,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if rc != 0 {
            return Vec::new();
        }
        let table = unsafe { &*(buffer.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID) };
        let rows = unsafe {
            std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize)
        };
        rows.iter()
            .filter(|row| row.dwState == MIB_TCP_STATE_LISTEN as u32)
            .map(|row| {
                let ip = Ipv6Addr::from(row.ucLocalAddr);
                let ip = if ip.is_unspecified() { Ipv6Addr::LOCALHOST } else { ip };
                let port = u16::from_be(row.dwLocalPort as u16);
                (ip, port, row.dwOwningPid)
            })
            .filter(|(ip, _, _)| ip.is_loopback())
            .map(|(ip, port, pid)| (SocketAddr::new(IpAddr::V6(ip), port), pid))
            .collect()
    }

    /// pid → parent pid, via a Toolhelp32 process snapshot (the documented,
    /// no-admin-required way to enumerate every process's parent on Windows).
    pub(super) fn parent_snapshot() -> HashMap<u32, u32> {
        let mut parents = HashMap::new();
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return parents;
        }
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
            loop {
                parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                    break;
                }
            }
        }
        unsafe { CloseHandle(snapshot) };
        parents
    }

    pub(super) fn open_and_started_at(pid: u32) -> Option<(HANDLE, u64)> {
        let process = open_process(pid)?;
        match process_started_at(process) {
            Some(started_at) => Some((process, started_at)),
            None => {
                unsafe { CloseHandle(process) };
                None
            }
        }
    }

    pub(super) fn cwd_and_args(process: HANDLE) -> Option<(PathBuf, Vec<String>)> {
        process_cwd_and_args(process)
    }

    pub(super) fn close(process: HANDLE) {
        unsafe { CloseHandle(process) };
    }

    pub(super) fn tcp_listeners() -> Vec<(SocketAddr, u32)> {
        let mut sockets = ipv4_listeners();
        sockets.extend(ipv6_listeners());
        sockets
    }
}

#[cfg(target_os = "windows")]
pub fn same_process(pid: u32, started_at: u64) -> bool {
    let Some((process, actual)) = windows_impl::open_and_started_at(pid) else {
        return false;
    };
    windows_impl::close(process);
    actual == started_at
}

#[cfg(target_os = "windows")]
pub fn listeners() -> Vec<Listener> {
    let sockets = windows_impl::tcp_listeners();
    if sockets.is_empty() {
        return Vec::new();
    }
    let mut by_pid: HashMap<u32, Vec<SocketAddr>> = HashMap::new();
    for (address, pid) in sockets {
        by_pid.entry(pid).or_default().push(address);
    }
    let parents = windows_impl::parent_snapshot();
    let mut result = Vec::new();
    for (pid, addresses) in by_pid {
        // Failure to open the process, or to walk its PEB, means it belongs
        // to another user or is otherwise inaccessible without elevation —
        // treated the same as the uid check Linux/macOS use: not ours.
        let Some((process, started_at)) = windows_impl::open_and_started_at(pid) else {
            continue;
        };
        let identity = windows_impl::cwd_and_args(process);
        windows_impl::close(process);
        let Some((cwd, args)) = identity else {
            continue;
        };
        let parent = parents.get(&pid).copied().unwrap_or(0);
        for address in addresses {
            result.push(Listener {
                pid,
                parent,
                cwd: cwd.clone(),
                args: args.clone(),
                started_at,
                address,
                zeron_owned: false,
            });
        }
    }
    mark_descendants(&mut result, &parents, std::process::id());
    result.sort_by_key(|l| (l.address, l.pid));
    result.dedup_by_key(|l| (l.address, l.pid));
    result
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn listeners() -> Vec<Listener> {
    Vec::new()
}
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn same_process(_pid: u32, _started_at: u64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_identity_ignores_port_configuration() {
        let args = |port: &str| vec!["node".into(), "api.js".into(), "--port".into(), port.into()];
        assert_eq!(
            command_identity(&args("3000")),
            command_identity(&args("3001"))
        );
        assert_ne!(
            command_identity(&args("3000")),
            command_identity(&["node".into(), "docs.js".into()])
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn accepts_only_loopback_reachable_listeners() {
        assert_eq!(
            proc_address("0100007F:1435", false).unwrap(),
            "127.0.0.1:5173".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            proc_address("00000000000000000000000001000000:1435", true).unwrap(),
            "[::1]:5173".parse::<SocketAddr>().unwrap()
        );
        assert!(proc_address("0101A8C0:1435", false).is_none());
        assert!(proc_address("malformed:00", true).is_none());
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn same_process_matches_the_scanner_start_time() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        struct Kill(std::process::Child);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _guard = Kill(child);
        // `listeners()` only reports sockets; derive the scanner's start time
        // through the same platform path it uses.
        #[cfg(target_os = "linux")]
        let started_at = {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let (_, tail) = stat.rsplit_once(") ").unwrap();
            let fields: Vec<_> = tail.split_whitespace().collect();
            let (boot, ticks) = linux_clock();
            linux_started_at(&fields, boot, ticks)
        };
        #[cfg(target_os = "macos")]
        let started_at = {
            let output = std::process::Command::new("/bin/ps")
                .args(["-axo", "pid=,ppid=,lstart=,command="])
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines()
                .map(|line| line.split_whitespace().collect::<Vec<_>>())
                .find(|parts| parts.first().and_then(|p| p.parse::<u32>().ok()) == Some(pid))
                .map(|parts| parse_lstart(&parts[2..7]))
                .unwrap()
        };
        assert!(started_at > 0);
        assert!(same_process(pid, started_at));
        assert!(!same_process(pid, started_at + 1000));
        assert!(!same_process(pid, 0));
        drop(_guard);
        assert!(
            !same_process(pid, started_at),
            "a reaped pid no longer matches"
        );
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn same_process_matches_the_scanner_start_time() {
        // A plain, non-interactive sleep: deterministic (no console/stdin
        // quirks like `pause` or `timeout` have when their stdio is
        // redirected), and killed well before its 30s budget elapses.
        let child = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 30"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        struct Kill(std::process::Child);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _guard = Kill(child);
        let (process, started_at) =
            windows_impl::open_and_started_at(pid).expect("open freshly spawned child process");
        windows_impl::close(process);
        assert!(started_at > 0);
        assert!(same_process(pid, started_at));
        assert!(!same_process(pid, started_at + 1000));
        assert!(!same_process(pid, 0));
        drop(_guard);
        // Give the OS a moment to retire the process table entry.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !same_process(pid, started_at),
            "a reaped pid no longer matches"
        );
    }
    #[tokio::test]
    async fn requires_an_http_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for response in [
                b"SSH-2.0-test\r\n".as_slice(),
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
            ] {
                let (mut client, _) = listener.accept().await.unwrap();
                let mut input = [0; 512];
                let _ = client.read(&mut input).await;
                client.write_all(response).await.unwrap();
            }
        });
        assert!(!is_http(address).await);
        assert!(is_http(address).await);
        task.await.unwrap();
    }
}
