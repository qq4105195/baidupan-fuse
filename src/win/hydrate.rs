//! 水合管线:fetch_data 回调里把远端文件按块搬进占位符。
//!
//! 策略(见 plan):
//! - 服务 [required_file_range, 文件尾) —— HydrationType::Full 下 required 即全文件;
//!   断点续传时 required 是缺失尾部,起点向下对齐 4KiB 多拉一小截也无所谓
//! - 4MiB 一块顺序单流(块大小 4KiB 对齐 → write_at 合法;CDN 单长流才是高速通道)
//! - 每块:下载(fetch_range 带 403 换链重试)→ write_at(同时重置平台 60s 计时)
//!   → report_progress(Explorer 内联进度/进度弹窗)
//! - cancel_fetch_data 置取消标志,块间检查,取消即安静退出(已写数据保留,
//!   平台下次按缺失区间重试,天然续传)
//! - 并发限 3:Explorer 批量 pin 时别把 CDN 和配额打爆。排队等待是安全的——
//!   任何一次 write_at(CfExecute)都会重置本进程全部挂起回调的计时器
//!
//! 错误处理铁律(见 vendor/cloud-filter/PATCHES.md):回调绝不能返回 Err 去让
//! proxy 同步发失败应答(全 0 区间会被平台拒绝)——自己带 required range 发,
//! 然后返回 Ok。

use crate::core;
use crate::win::provider::WinProvider;
use cloud_filter::error::{CloudErrorKind, CResult};
use cloud_filter::filter::{info, ticket};
use cloud_filter::utility::WriteAt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// 单块字节数:4MiB(4KiB 对齐;FUSE 侧实测块越大吞吐越高,4MiB 兼顾进度粒度)
const BLOCK: u64 = 4 * 1024 * 1024;

/// write_at 缓冲的对齐粒度(cldapi 硬约束,末块终于 EOF 可豁免)
const ALIGN: u64 = 4096;

/// 最大同时水合数
const MAX_CONCURRENT: usize = 3;

/// 水合协调器:并发闸门 + 取消标志表
pub(crate) struct Hydrator {
    /// 并发闸门(信号量)
    gate: Gate,
    /// 取消标志:本地路径 → 标志(同路径并发水合共享一个)
    cancels: Mutex<HashMap<PathBuf, Arc<AtomicBool>>>,
}

impl Hydrator {
    pub(crate) fn new() -> Self {
        Self {
            gate: Gate::new(MAX_CONCURRENT),
            cancels: Mutex::new(HashMap::new()),
        }
    }

    /// 水合一个文件:required 起点对齐后按 4MiB 块搬到文件尾。
    /// 任何失败都通过 ticket 带区间上报,然后返回 Ok——绝不走 trait 的 Err 路径
    pub(crate) fn fetch(
        &self,
        prov: &WinProvider,
        remote: &str,
        fs_id: u64,
        size: u64,
        required: std::ops::Range<u64>,
        ticket: ticket::FetchData,
    ) -> CResult<()> {
        let local = core::remote_to_local(remote, &prov.root, &prov.sync_root);
        let cancel = {
            let mut m = self.cancels.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(m.entry(local.clone()).or_insert_with(|| Arc::new(AtomicBool::new(false))))
        };
        let _gate = self.gate.acquire();

        // 起点向下对齐到 4KiB(多覆盖 ≤4095 字节,保证后续所有块 4KiB 对齐);
        // 服务到文件尾:Full 水合 required 本就是全文件,恢复场景是缺失尾部
        let mut pos = required.start & !(ALIGN - 1);
        let end = size.max(required.end);
        tracing::info!("水合 {remote}:{pos}..{end}(required {:?},fs_id {fs_id})", required);

        if fs_id == 0 {
            // 身份解析失败(blob 坏 + 父目录也没查到),报错走失败应答
            let e = CloudErrorKind::InvalidRequest;
            if let Err(del) = ticket.fail(e, required.clone()) {
                tracing::warn!("fs_id 未知且失败应答未送达:{del}");
            }
            return Ok(());
        }

        let _ = ticket.report_progress(end, pos); // 先亮个进度框
        while pos < end {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("水合被取消,停在 {pos}/{end}:{remote}");
                return Ok(()); // 取消不是错误:已写数据保留,平台按缺失区间重试
            }
            let len = BLOCK.min(end - pos);
            match prov.client.fetch_range(
                &prov.dlink_cache,
                fs_id,
                remote,
                "hydrate",
                pos,
                len,
                1,
                &prov.progress,
            ) {
                Ok(chunk) => {
                    // 4KiB 对齐检查:块大小对齐或终于 EOF(末块)
                    debug_assert!(chunk.len() as u64 == len);
                    if let Err(e) = ticket.write_at(&chunk, pos) {
                        tracing::error!("write_at {remote}@{pos} 失败:{e}");
                        let _ = ticket.fail(CloudErrorKind::Unsuccessful, required.clone());
                        return Ok(());
                    }
                    pos += chunk.len() as u64;
                    if let Err(e) = ticket.report_progress(end, pos) {
                        tracing::warn!("进度上报失败(继续):{e}");
                    }
                }
                Err(e) => {
                    // 网络/配额:失败应答带区间,应用侧立刻拿到错误而不是挂 60s
                    tracing::warn!("下载 {remote}@{pos}+{len} 失败:{e:#}");
                    let kind = if crate::baidu::is_forbidden(&e) {
                        CloudErrorKind::AccessDenied
                    } else {
                        CloudErrorKind::NetworkUnavailable
                    };
                    if let Err(del) = ticket.fail(kind, pos..end) {
                        tracing::warn!("失败应答未送达(平台按超时处理):{del}");
                    }
                    return Ok(());
                }
            }
        }
        tracing::info!("水合完成:{remote}({end} 字节)");
        Ok(())
    }

    /// fetch_data 的取消回调:置位对应路径的取消标志
    pub(crate) fn cancel(&self, path: &Path, info: &info::CancelFetchData) {
        tracing::debug!(
            "取消水合:{}(timeout={},user={},range {:?})",
            path.display(),
            info.timeout(),
            info.user_cancelled(),
            info.file_range()
        );
        let m = self.cancels.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(flag) = m.get(path) {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// 简单计数信号量(标准库实现,无额外依赖)
struct Gate {
    slots: Mutex<usize>,
    cv: Condvar,
}

impl Gate {
    fn new(max: usize) -> Self {
        Self {
            slots: Mutex::new(max),
            cv: Condvar::new(),
        }
    }

    /// 拿一个槽位;满了就等(等待安全:活跃水合的 write_at 会重置全部计时器)
    fn acquire(&self) -> GateGuard<'_> {
        let mut n = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        while *n == 0 {
            n = self.cv.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n -= 1;
        GateGuard { gate: self }
    }
}

struct GateGuard<'a> {
    gate: &'a Gate,
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        let mut n = self.gate.slots.lock().unwrap_or_else(|e| e.into_inner());
        *n += 1;
        self.gate.cv.notify_one();
    }
}
