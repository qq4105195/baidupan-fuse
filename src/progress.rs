//! 下载进度:挂载进程把进行中/最近完成的拉取写进 progress.json(原子写),
//! 控制台「8. 下载进度」读同一个文件展示——最简单的文件型 IPC,零依赖。
//! 工具目前只读(只有下载);将来做写支持时,上传进度也走这套。

use crate::baidu::config_dir;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 最近完成记录保留条数
const RECENT_KEEP: usize = 20;
/// 进行中条目的落盘节流:再频繁只是浪费磁盘
const FLUSH_EVERY: Duration = Duration::from_millis(150);

/// 进度文件里的一条"进行中"记录
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Running {
    pub id: u64,
    /// 远端文件路径
    pub path: String,
    /// 类型标签:块#N / 随机读
    pub kind: String,
    pub offset: u64,
    pub total: u64,
    pub done: u64,
    /// 开始时刻,unix 毫秒
    pub started_ms: u64,
}

/// 一条"最近完成"记录
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Done {
    pub path: String,
    pub kind: String,
    pub bytes: u64,
    /// 耗时毫秒
    pub ms: u64,
    pub ok: bool,
}

/// progress.json 的整体结构(控制台读取用)
#[derive(Serialize, Deserialize, Debug)]
pub struct ProgressFile {
    pub updated_ms: u64,
    pub running: Vec<Running>,
    pub recent: Vec<Done>,
}

/// 进度记录器:Clone 便宜(内部 Arc),可以直接传进下载线程
#[derive(Clone)]
pub struct Progress {
    inner: Arc<Mutex<State>>,
    file: PathBuf,
}

struct State {
    next_id: u64,
    running: Vec<Running>,
    recent: VecDeque<Done>,
    last_flush: Instant,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Progress {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                next_id: 0,
                running: Vec::new(),
                recent: VecDeque::new(),
                last_flush: Instant::now(),
            })),
            file: config_dir().join("progress.json"),
        }
    }

    /// 登记一次拉取,返回 id
    pub fn begin(&self, path: &str, kind: &str, offset: u64, total: u64) -> u64 {
        let mut st = self.lock();
        st.next_id += 1;
        let id = st.next_id;
        st.running.push(Running {
            id,
            path: path.to_string(),
            kind: kind.to_string(),
            offset,
            total,
            done: 0,
            started_ms: now_ms(),
        });
        self.flush(&mut st);
        id
    }

    /// 累加已下载字节(节流落盘)
    pub fn add(&self, id: u64, n: u64) {
        let mut st = self.lock();
        if let Some(e) = st.running.iter_mut().find(|e| e.id == id) {
            e.done += n;
        }
        if st.last_flush.elapsed() >= FLUSH_EVERY {
            self.flush(&mut st);
        }
    }

    /// 结束一次拉取(成功与否都记进最近列表)
    pub fn end(&self, id: u64, ok: bool) {
        let mut st = self.lock();
        if let Some(pos) = st.running.iter().position(|e| e.id == id) {
            let e = st.running.remove(pos);
            st.recent.push_front(Done {
                path: e.path,
                kind: e.kind,
                bytes: e.done,
                ms: now_ms().saturating_sub(e.started_ms),
                ok,
            });
            while st.recent.len() > RECENT_KEEP {
                st.recent.pop_back();
            }
        }
        self.flush(&mut st);
    }

    /// 卸载时清掉进度文件,别留下"永远进行中"的假条目
    /// (只有 unix 的 mount/卸载路径用;Windows 同步常驻不卸载)
    #[cfg(unix)]
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.file);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.lock().expect("进度锁中毒")
    }

    /// 落盘(调用方持有锁):先写 .tmp 再改名,读方不会读到半截 JSON。
    /// 进度写失败不该影响下载,只记日志
    fn flush(&self, st: &mut State) {
        st.last_flush = Instant::now();
        let v = ProgressFile {
            updated_ms: now_ms(),
            running: st.running.clone(),
            recent: st.recent.iter().cloned().collect(),
        };
        let tmp = self.file.with_file_name("progress.json.tmp");
        let Ok(raw) = serde_json::to_string(&v) else {
            return;
        };
        if let Err(e) = std::fs::write(&tmp, raw).and_then(|_| std::fs::rename(&tmp, &self.file)) {
            tracing::debug!("写进度文件失败: {e}");
        }
    }
}
