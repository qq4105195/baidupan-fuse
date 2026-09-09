//! 交互控制台的持久化设置:~/.config/baidupan-fuse/settings.json。
//! 「7. 设置」改、「3. 挂载」用、装 systemd 服务时生成 ExecStart 也用。
//! 注意:CLI 子命令 mount 的参数优先级高于这里的值(脚本场景显式优先),
//! 菜单场景则完全以这份设置为准。

use crate::baidu::config_dir;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    /// 本地挂载点
    pub mountpoint: String,
    /// 挂载的远端根目录(挂全盘填 /)
    pub root: String,
    /// 顺序读块大小 MB(越大吞吐越高,16 是实测甜点)
    pub block_mb: u64,
    /// 每块并发连接数(SVIP 账号保持 1,并发波浪会被 CDN 限速)
    pub parallel: u64,
    /// 块缓存总上限 MB(FIFO 淘汰)
    pub cache_mb: u64,
    /// 目录列表缓存秒数(省 API 配额)
    pub dir_ttl: u64,
    /// 下载直链缓存秒数(官方 8 小时有效,保守 30 分钟)
    pub dlink_ttl: u64,
    /// 允许其他用户访问挂载点(需 /etc/fuse.conf 放开 user_allow_other)
    pub allow_other: bool,
    /// 只读挂载(默认 false:写支持已上线,要挡写用这个)
    pub readonly: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mountpoint: "/mnt/pan".into(),
            root: "/".into(),
            block_mb: 16,
            parallel: 1,
            cache_mb: 128,
            dir_ttl: 60,
            dlink_ttl: 1800,
            allow_other: false,
            readonly: false,
        }
    }
}

impl Settings {
    fn path() -> std::path::PathBuf {
        config_dir().join("settings.json")
    }

    /// 读设置:文件不存在/损坏都静默回默认值,别让控制台起不来
    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                tracing::warn!("settings.json 解析失败({e}),用默认值");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(config_dir())?;
        let p = Self::path();
        std::fs::write(&p, serde_json::to_string_pretty(self)?)?;
        println!("已保存到 {}", p.display());
        Ok(())
    }
}
