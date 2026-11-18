//! 进程快照：用来判断某个 agent 是否还在运行。只取 pid 和可执行文件名，不读命令行。

use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub struct Processes {
    /// pid → 小写的可执行文件名
    by_pid: HashMap<u32, String>,
    available: bool,
}

impl Processes {
    pub fn snapshot() -> Processes {
        match list() {
            Some(by_pid) => Processes { by_pid, available: true },
            None => Processes::default(),
        }
    }

    /// 取不到进程列表时为 false，此时其他判断都返回“不确定”。
    pub fn available(&self) -> bool {
        self.available
    }

    pub fn len(&self) -> usize {
        self.by_pid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_pid.is_empty()
    }

    /// pid 是否存活；进程列表不可用时返回 `None`。
    pub fn alive(&self, pid: u32) -> Option<bool> {
        self.available.then(|| self.by_pid.contains_key(&pid))
    }

    /// 文件名里包含任一关键字的进程数；进程列表不可用时返回 `None`。
    pub fn count_named(&self, needles: &[&str]) -> Option<usize> {
        self.available.then(|| self.by_pid.values().filter(|name| needles.iter().any(|needle| name.contains(needle))).count())
    }

    pub fn any_named(&self, needles: &[&str]) -> Option<bool> {
        self.count_named(needles).map(|count| count > 0)
    }

    /// 匹配到的进程文件名（去重），调试用。
    pub fn names_matching(&self, needles: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = self.by_pid.values().filter(|name| needles.iter().any(|needle| name.contains(needle))).cloned().collect();
        names.sort();
        names.dedup();
        names
    }

    #[cfg(test)]
    pub(crate) fn fake(items: &[(u32, &str)]) -> Processes {
        Processes { by_pid: items.iter().map(|(pid, name)| (*pid, name.to_lowercase())).collect(), available: true }
    }
}

#[cfg(windows)]
fn list() -> Option<HashMap<u32, String>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };

    // SAFETY: 标准的 ToolHelp 遍历；`entry` 是按结构体大小初始化的纯数据，句柄在返回前关闭。
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut by_pid = HashMap::new();
        let mut more = Process32FirstW(snapshot, &mut entry);
        while more != 0 {
            let len = entry.szExeFile.iter().position(|unit| *unit == 0).unwrap_or(entry.szExeFile.len());
            by_pid.insert(entry.th32ProcessID, String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase());
            more = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        Some(by_pid)
    }
}

#[cfg(not(windows))]
fn list() -> Option<HashMap<u32, String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_names_and_pids() {
        let processes = Processes::fake(&[(10, "Codex.exe"), (11, "node.exe")]);
        assert_eq!(processes.alive(10), Some(true));
        assert_eq!(processes.alive(12), Some(false));
        assert_eq!(processes.any_named(&["codex"]), Some(true));
        assert_eq!(processes.any_named(&["claude"]), Some(false));
        assert_eq!(Processes::default().alive(10), None);
    }

    #[cfg(windows)]
    #[test]
    fn snapshot_contains_this_process() {
        let processes = Processes::snapshot();
        assert!(processes.available());
        assert_eq!(processes.alive(std::process::id()), Some(true));
    }
}
