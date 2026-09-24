//! 后台挂载:fork 出独立会话的子进程跑 mount2,父进程轮询 /proc/mounts,
//! 挂载点一出现就返回。控制台「4. 挂载」、控制台启动时的自动挂载、
//! `bdfs mount --daemon` 三处共用。
//!
//! 关键约束:**挂载用的客户端必须在 fork 之后、子进程里构造**。
//! reqwest::blocking::Client 创建时会给自己起一个内部 tokio 运行时线程,
//! 而 fork 不复制线程——父进程建好的客户端到了子进程里,运行时线程不在,
//! 所有请求都会空等到超时。所以这里收的是构造闭包,不是现成的 PanFs。
//!
//! 单次 fork + setsid 就够脱离终端(关终端/退出控制台都不会带走挂载进程),
//! 不做双 fork:父进程(控制台)活着时由它收尸,退出后子进程挂到 init 下。
//! 没选 systemd 托管是因为这个路径主要服务交互控制台——用户没装自启服务
//! 也能"启动即挂载";要开机自启仍然推荐「6. 开机自动挂载」的模板实例。

use crate::fs::PanFs;
use crate::pan::PanKind;
use anyhow::{bail, Result};
use fuser::MountOption;
use std::io::Write;
use std::time::Duration;

/// 后台挂载。成功 = 挂载点已在 /proc/mounts 里出现;失败 = 子进程退出或超时,
/// 报错会带上日志末尾的失败原因。日志追加写 config 目录 log-<网盘>.txt。
pub fn spawn(
    kind: PanKind,
    mountpoint: &str,
    opts: Vec<MountOption>,
    make_client_fs: impl FnOnce() -> Result<PanFs>,
) -> Result<()> {
    let log = crate::pan::config_dir().join(format!("log-{}.txt", kind.id()));
    let c_log = std::ffi::CString::new(log.to_string_lossy().as_bytes())
        .map_err(|e| anyhow::anyhow!("日志路径不合法:{e}"))?;
    // fork 前把父进程的缓冲刷掉,避免子进程继承后重复输出一遍
    let _ = std::io::stdout().flush();

    let pid = unsafe { libc::fork() };
    match pid {
        0 => unsafe {
            // 子进程:新会话(脱离控制终端),stdio 重定向到日志文件。
            // 客户端在这里构造(见模块注释:fork 后才建,reqwest 的内部线程才在)
            libc::setsid();
            let fd = libc::open(
                c_log.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
                0o600,
            );
            if fd >= 0 {
                libc::dup2(fd, 1);
                libc::dup2(fd, 2);
                if fd > 2 {
                    libc::close(fd);
                }
            }
            if let Ok(n) = std::ffi::CString::new("/dev/null") {
                let nullfd = libc::open(n.as_ptr(), libc::O_RDONLY);
                if nullfd >= 0 {
                    libc::dup2(nullfd, 0);
                }
            }
            let panfs = match make_client_fs() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("初始化失败:{e:#}");
                    std::process::exit(1);
                }
            };
            let prog = panfs.progress().clone();
            match fuser::mount2(panfs, mountpoint, &opts) {
                Ok(()) => {
                    prog.clear();
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("挂载失败:{e:#}");
                    std::process::exit(1);
                }
            }
        },
        pid if pid > 0 => {
            // 父进程:最多等 10s。子进程退出(WNOHANG 能收到僵尸)说明初始化/挂载失败,
            // 把日志末行带出来,省得用户再开一次文件
            for _ in 0..100 {
                if crate::fs::is_mounted(mountpoint) {
                    println!("已在后台挂载到 {mountpoint}(日志:{})", log.display());
                    return Ok(());
                }
                let mut status = 0;
                if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
                    bail!(
                        "后台挂载进程退出了:{}(日志:{})",
                        log_tail(&log).unwrap_or_else(|| "原因未知".into()),
                        log.display()
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            bail!("等后台挂载超时(10s),看 {}", log.display());
        }
        _ => bail!("fork 失败:{}", std::io::Error::last_os_error()),
    }
}

/// 日志末尾最后一行非空文本(子进程刚 eprintln 的失败原因)
fn log_tail(path: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    // 日志带 ANSI 颜色码,剥掉再给用户看
    let line = s
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default();
    Some(strip_ansi(line))
}

/// 剥 ANSI 转义序列(只处理常见的 ESC[…m,够用了)
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.next() == Some('[') {
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn 剥_ansi_颜色码() {
        assert_eq!(strip_ansi("\u{1b}[33m WARN\u{1b}[0m bdfs: 挂载失败:x"), " WARN bdfs: 挂载失败:x");
        assert_eq!(strip_ansi("普通文本"), "普通文本");
    }
}
