//! 设置持久化:~/.config/baidupan-fuse/settings.json。
//! 多网盘之后是一个总配置:当前选中的网盘 + 每个网盘各自的挂载参数。
//! 「设置」菜单改当前网盘那份、装 systemd 服务时生成 ExecStart 也用它。
//! 注意:CLI 子命令 mount 的参数优先级高于这里的值(脚本场景显式优先),
//! 菜单场景则完全以这份设置为准。
//!
//! 兼容:老版本是单网盘扁平格式(mountpoint/root/… 直接在顶层),
//! 读取时识别出来就迁移成新格式(当百度网盘的设置),下次保存落盘新格式。

use crate::pan::{config_dir, PanKind};
use serde::{Deserialize, Serialize};

/// 单个网盘的挂载参数
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
    /// 下载直链缓存秒数(百度官方 8 小时有效取保守 30 分钟;
    /// 联通直链时效不明且对 Referer 敏感,默认更短)
    pub dlink_ttl: u64,
    /// 允许其他用户访问挂载点(需 /etc/fuse.conf 放开 user_allow_other)
    pub allow_other: bool,
    /// 只读挂载(默认 false:写支持已上线,要挡写用这个)
    pub readonly: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self::for_kind(PanKind::Baidu)
    }
}

impl Settings {
    /// 各后端的默认值:挂载点/直链缓存有别,其余共用
    pub fn for_kind(kind: PanKind) -> Self {
        match kind {
            PanKind::Baidu => Self {
                mountpoint: "/mnt/pan".into(),
                root: "/".into(),
                block_mb: 16,
                parallel: 1,
                cache_mb: 128,
                dir_ttl: 60,
                dlink_ttl: 1800,
                allow_other: false,
                readonly: false,
            },
            PanKind::Wopan => Self {
                mountpoint: "/mnt/wopan".into(),
                root: "/".into(),
                block_mb: 16,
                parallel: 1,
                cache_mb: 128,
                dir_ttl: 60,
                dlink_ttl: 600,
                allow_other: false,
                readonly: false,
            },
        }
    }
}

/// 总配置:当前网盘 + 各网盘设置
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// 当前操作的网盘(菜单/裸 mount 默认用它),存 PanKind::id()
    pub backend: String,
    pub baidu: Settings,
    pub wopan: Settings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: PanKind::Baidu.id().into(),
            baidu: Settings::for_kind(PanKind::Baidu),
            wopan: Settings::for_kind(PanKind::Wopan),
        }
    }
}

impl Config {
    fn path() -> std::path::PathBuf {
        config_dir().join("settings.json")
    }

    /// 当前选中的网盘;存了不认识的值就回百度(别让控制台起不来)
    pub fn current_kind(&self) -> PanKind {
        PanKind::parse(&self.backend).unwrap_or(PanKind::Baidu)
    }

    pub fn settings_for(&self, kind: PanKind) -> &Settings {
        match kind {
            PanKind::Baidu => &self.baidu,
            PanKind::Wopan => &self.wopan,
        }
    }

    pub fn settings_for_mut(&mut self, kind: PanKind) -> &mut Settings {
        match kind {
            PanKind::Baidu => &mut self.baidu,
            PanKind::Wopan => &mut self.wopan,
        }
    }

    /// 读设置:文件不存在/损坏都静默回默认值;
    /// 老版扁平格式(顶层直接是 mountpoint/…)迁移成百度那份
    pub fn load() -> Self {
        let raw = match std::fs::read_to_string(Self::path()) {
            Ok(r) => r,
            Err(_) => return Self::default(),
        };
        // 先看形状再决定按哪种格式解析,坏文件静默回默认
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            tracing::warn!("settings.json 解析失败,用默认值");
            return Self::default();
        };
        if v.get("mountpoint").is_some() && v.get("backend").is_none() {
            let baidu: Settings = serde_json::from_value(v).unwrap_or_default();
            tracing::info!("检测到旧版单网盘设置,已按百度网盘设置迁移(保存后转新格式)");
            return Self {
                backend: PanKind::Baidu.id().into(),
                baidu,
                ..Self::default()
            };
        }
        serde_json::from_value(v).unwrap_or_else(|e| {
            tracing::warn!("settings.json 解析失败({e}),用默认值");
            Self::default()
        })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(config_dir())?;
        let p = Self::path();
        std::fs::write(&p, serde_json::to_string_pretty(self)?)?;
        println!("已保存到 {}", p.display());
        Ok(())
    }
}
