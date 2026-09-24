//! 百度网盘开放平台 API 客户端:blocking HTTP,供 fuser 的同步 Filesystem 直接调用。
//! 接口对齐官方文档 https://pan.baidu.com/union/doc/ (基础网盘服务),
//! 通过 pan::PanClient 暴露给 fs 层(路径寻址、三段式上传的细节都封在这)。

use super::{NetFile, PanClient, PanError, PanKind};
use crate::progress::Progress;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::os::unix::fs::FileExt;

const OPEN_HOST: &str = "https://openapi.baidu.com";
const PAN_HOST: &str = "https://pan.baidu.com";
/// 分片上传端点。注意:superfile2 只在 pcs 域名上有,pan.baidu.com 上是 404 页
/// (实测 403+HTML);BaiduPCS-Go 也用这个域名
const PCS_HOST: &str = "https://d.pcs.baidu.com";
/// dlink 下载必须带这个 User-Agent,否则命中防盗链(错误码 31326)
const DL_UA: &str = "pan.baidu.com";
/// 目录列表每页条数,官方建议不超过 1000
const PAGE_LIMIT: u32 = 1000;

// ---------- 上传分片 ----------

/// 上传分片基准大小:superfile2 的标准切片是 4MB
pub const UPLOAD_SLICE: u64 = 4 << 20;

/// 按文件大小算分片:4MB 起步;官方限制分片数 ≤1024,
/// 超过 4MB×1024=4GB 的文件自动放大分片(取 4MB 整倍数,切片边界规整)
fn upload_slice_size(file_size: u64) -> u64 {
    let min_slice = file_size.div_ceil(1024);
    if min_slice <= UPLOAD_SLICE {
        UPLOAD_SLICE
    } else {
        min_slice.div_ceil(UPLOAD_SLICE) * UPLOAD_SLICE
    }
}

/// 上传专用 client:4MB 分片 + 慢上行,60s 超时会在半路掐断,放宽到 10 分钟
fn upload_http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?)
}

/// 包在 multipart 分片外面的读适配器:上传体边发边记进度
struct ProgReader {
    inner: Cursor<Vec<u8>>,
    prog: Progress,
    id: u64,
}

impl Read for ProgReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.prog.add(self.id, n as u64);
        }
        Ok(n)
    }
}

// ---------- 配置与 token 持久化 ----------

/// 应用凭据,login 时保存,后续 mount 不用再传
#[derive(Serialize, Deserialize, Clone)]
pub struct AppConfig {
    pub app_key: String,
    pub app_secret: String,
}

/// OAuth token(access 30 天有效,refresh 10 年)
#[derive(Serialize, Deserialize, Clone)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    /// access_token 过期时间,unix 秒
    pub expires_at: u64,
}

impl AppConfig {
    fn path() -> std::path::PathBuf {
        super::config_dir().join("config.json")
    }

    pub fn load() -> Result<Self> {
        super::load_json(&Self::path())
            .with_context(|| format!("读配置失败 {:?},请先运行 bdfs 登录(裸跑进控制台或 bdfs login)", Self::path()))
    }

    pub fn save(&self) -> Result<()> {
        super::save_json_atomic(&Self::path(), self)
    }
}

impl Token {
    fn path() -> std::path::PathBuf {
        super::config_dir().join("token.json")
    }

    fn load() -> Result<Self> {
        super::load_json(&Self::path())
            .with_context(|| format!("读 token 失败 {:?},请先运行 bdfs 登录", Self::path()))
    }

    fn save(&self) -> Result<()> {
        super::save_json_atomic(&Self::path(), self)
    }
}

// ---------- OAuth 登录 ----------

#[derive(Deserialize)]
struct DeviceCodeResp {
    device_code: String,
    user_code: String,
    verification_url: String,
    #[serde(default)]
    qrcode_url: String,
    /// 轮询间隔秒数,一般 5
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default = "default_expires")]
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

fn default_expires() -> u64 {
    300
}

#[derive(Deserialize)]
struct TokenResp {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    /// 有效期秒数,设备码模式是 30 天
    #[serde(default)]
    expires_in: Option<u64>,
    /// 失败时百度的惯例:HTTP 200 + JSON 里的 error 字段
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

fn http_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?)
}

/// 设备码模式登录:打印验证网址让用户去浏览器授权,轮询直到成功。
/// 适合无图形界面的 Linux 服务器。
pub fn device_login(app_key: &str, app_secret: &str) -> Result<Token> {
    let http = http_client()?;
    let dev: DeviceCodeResp = http
        .get(format!("{OPEN_HOST}/oauth/2.0/device"))
        .query(&[
            ("response_type", "device_code"),
            ("client_id", app_key),
            ("scope", "basic,netdisk"),
        ])
        .send()?
        .error_for_status()?
        .json()?;

    println!("== 百度账号授权 ==");
    println!("1. 浏览器打开: {}", dev.verification_url);
    println!("2. 输入授权码: {}", dev.user_code);
    if !dev.qrcode_url.is_empty() {
        println!("   或扫码:     {}", dev.qrcode_url);
    }
    println!("等待授权中(超时 {} 秒)…", dev.expires_in);

    let deadline = Instant::now() + Duration::from_secs(dev.expires_in);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(dev.interval));
        let resp: TokenResp = http
            .get(format!("{OPEN_HOST}/oauth/2.0/token"))
            .query(&[
                ("grant_type", "device_token"),
                ("code", dev.device_code.as_str()),
                ("client_id", app_key),
                ("client_secret", app_secret),
            ])
            .send()?
            .json()?;

        if let Some(at) = resp.access_token {
            let token = Token {
                access_token: at,
                // 刷新 token 百度可能不回传,回传就更新
                refresh_token: resp.refresh_token.unwrap_or_default(),
                expires_at: now_secs() + resp.expires_in.unwrap_or(86400).saturating_sub(600),
            };
            token.save()?;
            println!("授权成功,token 已存到 {:?}", Token::path());
            return Ok(token);
        }

        match resp.error.as_deref() {
            // 用户还没确认授权,继续等
            Some("authorization_pending") => print!("."),
            Some("expired_token") => bail!("授权码已过期,请重新 login"),
            Some(e) => bail!(
                "授权失败: {e} ({})",
                resp.error_description.unwrap_or_default()
            ),
            None => bail!("授权响应异常: 缺 access_token"),
        }
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
    bail!("等待授权超时,请重新 login")
}

/// 授权码模式(oob)登录:应用没开通设备码授权时的替代方案。
/// 打印授权网址,用户浏览器登录并同意后,页面会显示授权码,
/// 把码从 stdin 传进来换 token。
pub fn authcode_login(app_key: &str, app_secret: &str) -> Result<Token> {
    let url = format!(
        "{OPEN_HOST}/oauth/2.0/authorize?response_type=code&client_id={app_key}\
         &redirect_uri=oob&scope=basic,netdisk"
    );
    println!("== 百度账号授权(授权码模式)==");
    println!("浏览器打开: {url}");
    println!("登录并同意授权后,页面会显示授权码,粘贴到这里回车:");

    let mut code = String::new();
    std::io::stdin()
        .read_line(&mut code)
        .context("读授权码失败")?;
    let code = code.trim();
    if code.is_empty() {
        bail!("授权码为空");
    }

    let http = http_client()?;
    let resp: TokenResp = http
        .get(format!("{OPEN_HOST}/oauth/2.0/token"))
        .query(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", app_key),
            ("client_secret", app_secret),
            ("redirect_uri", "oob"),
        ])
        .send()?
        .error_for_status()?
        .json()?;

    let at = resp
        .access_token
        .ok_or_else(|| anyhow!("换 token 失败: {} ({})", resp.error.unwrap_or_default(), resp.error_description.unwrap_or_default()))?;
    let token = Token {
        access_token: at,
        refresh_token: resp.refresh_token.unwrap_or_default(),
        expires_at: now_secs() + resp.expires_in.unwrap_or(86400).saturating_sub(600),
    };
    token.save()?;
    println!("授权成功,token 已存到 {:?}", Token::path());
    Ok(token)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------- 错误 ----------

/// 响应里 errno 非 0 就提取成 PanError(-9/-7 特判,其余归 Api)
fn as_api_err(v: &Value) -> Option<PanError> {
    let errno = v.get("errno")?.as_i64()?;
    if errno == 0 {
        return None;
    }
    let msg = v
        .get("show_msg")
        .or_else(|| v.get("errmsg"))
        .and_then(|m| m.as_str())
        .unwrap_or("未知错误")
        .to_string();
    Some(match errno {
        -9 => PanError::NotFound,
        -7 => PanError::PermissionDenied,
        _ => PanError::Api(format!("errno={errno} {msg}")),
    })
}

// ---------- 网盘文件模型 ----------

/// 从 list/filemetas 的条目 JSON 构建 NetFile
fn netfile_from_value(v: &Value) -> Option<NetFile> {
    let id = v.get("fs_id")?.as_u64()?.to_string();
    Some(NetFile {
        path: v.get("path")?.as_str()?.to_string(),
        name: v
            .get("server_filename")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        is_dir: v.get("isdir").and_then(|d| d.as_i64())? == 1,
        size: v.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
        mtime: v
            .get("server_mtime")
            .and_then(|s| s.as_i64())
            .unwrap_or(0),
        fid: id.clone(),
        id,
    })
}

/// 用户信息(uinfo)
#[derive(Deserialize, Default)]
pub struct UserInfo {
    #[serde(default)]
    pub baidu_name: String,
    #[serde(default)]
    pub netdisk_name: String,
    #[serde(default)]
    pub uk: u64,
    /// 0 普通 / 1 会员 / 2 超级会员
    #[serde(default)]
    pub vip_type: i32,
}

// ---------- 客户端 ----------

pub struct BaiduClient {
    http: reqwest::blocking::Client,
    /// 上传专用 client:超时放宽到 10 分钟,大分片慢上行不被掐断
    up_http: reqwest::blocking::Client,
    app_key: String,
    app_secret: String,
    token: Token,
}

impl BaiduClient {
    /// 从本地配置构建,token 快过期时自动刷新
    pub fn from_config() -> Result<Self> {
        let cfg = AppConfig::load()?;
        let token = Token::load()?;
        let mut c = Self {
            http: http_client()?,
            up_http: upload_http()?,
            app_key: cfg.app_key,
            app_secret: cfg.app_secret,
            token,
        };
        c.ensure_token()?;
        Ok(c)
    }

    /// token 剩余寿命不足 2 分钟就刷新(留点余量,避免挂载中途失效)
    fn ensure_token(&mut self) -> Result<()> {
        if self.token.expires_at <= now_secs() + 120 {
            self.refresh()?;
        }
        Ok(())
    }

    fn refresh(&mut self) -> Result<()> {
        let resp: TokenResp = self
            .http
            .get(format!("{OPEN_HOST}/oauth/2.0/token"))
            .query(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", self.token.refresh_token.as_str()),
                ("client_id", self.app_key.as_str()),
                ("client_secret", self.app_secret.as_str()),
            ])
            .send()?
            .error_for_status()?
            .json()?;

        let at = resp
            .access_token
            .ok_or_else(|| anyhow!("刷新 token 失败: {}", resp.error.unwrap_or_default()))?;
        self.token.access_token = at;
        if let Some(rt) = resp.refresh_token {
            if !rt.is_empty() {
                self.token.refresh_token = rt;
            }
        }
        self.token.expires_at = now_secs() + resp.expires_in.unwrap_or(86400).saturating_sub(600);
        self.token.save()?;
        tracing::info!("access_token 已刷新,新过期时间 {}", self.token.expires_at);
        Ok(())
    }

    /// 带鉴权 GET 一个 xpan 接口;token 失效(errno -6/111)自动刷新重试一次
    fn rest_get(&mut self, endpoint: &str, params: &[(String, String)]) -> Result<Value> {
        let v = self.do_rest(endpoint, params)?;
        if let Some(errno) = v.get("errno").and_then(|e| e.as_i64()) {
            if errno == -6 || errno == 111 {
                tracing::warn!("token 失效(errno={errno}),刷新后重试");
                self.refresh()?;
                return self.do_rest(endpoint, params);
            }
        }
        Ok(v)
    }

    fn do_rest(&mut self, endpoint: &str, params: &[(String, String)]) -> Result<Value> {
        self.ensure_token()?;
        let mut q: Vec<(String, String)> = params.to_vec();
        q.push(("access_token".into(), self.token.access_token.clone()));
        let resp = self
            .http
            .get(format!("{PAN_HOST}{endpoint}"))
            .query(&q)
            .send()?
            .error_for_status()?;
        Ok(resp.json()?)
    }

    /// 带鉴权 POST 一个 xpan 表单接口;token 失效(errno -6/111)自动刷新重试一次
    fn rest_post(
        &mut self,
        endpoint: &str,
        query: &[(String, String)],
        form: &[(String, String)],
    ) -> Result<Value> {
        let v = self.do_rest_post(endpoint, query, form)?;
        if let Some(errno) = v.get("errno").and_then(|e| e.as_i64()) {
            if errno == -6 || errno == 111 {
                tracing::warn!("token 失效(errno={errno}),刷新后重试");
                self.refresh()?;
                return self.do_rest_post(endpoint, query, form);
            }
        }
        Ok(v)
    }

    fn do_rest_post(
        &mut self,
        endpoint: &str,
        query: &[(String, String)],
        form: &[(String, String)],
    ) -> Result<Value> {
        self.ensure_token()?;
        let mut q: Vec<(String, String)> = query.to_vec();
        q.push(("access_token".into(), self.token.access_token.clone()));
        let resp = self
            .http
            .post(format!("{PAN_HOST}{endpoint}"))
            .query(&q)
            .form(form)
            .send()?
            .error_for_status()?;
        Ok(resp.json()?)
    }

    fn check_api(&self, v: &Value) -> Result<()> {
        if let Some(e) = as_api_err(v) {
            return Err(anyhow::Error::new(e));
        }
        Ok(())
    }

    /// 用户信息
    pub fn uinfo(&mut self) -> Result<UserInfo> {
        let v = self.rest_get(
            "/rest/2.0/xpan/nas",
            &[("method".into(), "uinfo".into())],
        )?;
        self.check_api(&v)?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    /// 容量,返回 (总字节, 已用字节)。
    /// 注意:xpan/nas?method=quota 对部分应用报 Param error,实测 /api/quota 通用
    pub fn quota_raw(&mut self) -> Result<(u64, u64)> {
        let v = self.rest_get(
            "/api/quota",
            &[
                ("checkfree".into(), "1".into()),
                ("checkexpire".into(), "1".into()),
            ],
        )?;
        self.check_api(&v)?;
        let total = v.get("total").and_then(|t| t.as_u64()).unwrap_or(0);
        let used = v.get("used").and_then(|t| t.as_u64()).unwrap_or(0);
        Ok((total, used))
    }

    /// 列目录(自动翻页拉全;大目录会多次调用,靠上层缓存兜着)
    pub fn list_dir_raw(&mut self, dir: &str) -> Result<Vec<NetFile>> {
        let mut out = Vec::new();
        let mut start: u32 = 0;
        loop {
            let v = self.rest_get(
                "/rest/2.0/xpan/file",
                &[
                    ("method".into(), "list".into()),
                    ("dir".into(), dir.to_string()),
                    ("order".into(), "name".into()),
                    ("start".into(), start.to_string()),
                    ("limit".into(), PAGE_LIMIT.to_string()),
                ],
            )?;
            self.check_api(&v)?;
            let list = v
                .get("list")
                .and_then(|l| l.as_array())
                .context("list 响应缺 list 字段")?;
            let n = list.len();
            for item in list {
                if let Some(f) = netfile_from_value(item) {
                    out.push(f);
                }
            }
            // 返回不满一页说明到底了
            if (n as u32) < PAGE_LIMIT {
                break;
            }
            start += PAGE_LIMIT;
        }
        Ok(out)
    }

    /// 查某个文件的下载直链(dlink,官方 8 小时有效,靠上层缓存)
    pub fn get_dlink_raw(&mut self, fs_id: u64) -> Result<String> {
        let v = self.rest_get(
            "/rest/2.0/xpan/file",
            &[
                ("method".into(), "filemetas".into()),
                ("fsids".into(), format!("[{fs_id}]")),
                ("dlink".into(), "1".into()),
            ],
        )?;
        self.check_api(&v)?;
        // 官方文档说返回 list,实测这个账号/接口返回 info——两个都认
        v.pointer("/list/0/dlink")
            .or_else(|| v.pointer("/info/0/dlink"))
            .and_then(|d| d.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("filemetas 响应缺 dlink"))
    }

    /// 拉一段数据:切成 parts 份并行 Range 请求,按序拼接(aria2 式多连接下载)。
    /// prog/id 用于实时进度:每个分片收到多少字节就 add 多少。
    pub fn read_range_raw(
        &self,
        dlink: &str,
        offset: u64,
        len: u64,
        parts: usize,
        prog: &Progress,
        id: u64,
    ) -> Result<Vec<u8>> {
        if parts <= 1 || len < (1 << 20) {
            return Self::fetch_part(&self.http, dlink, &self.token.access_token, offset, len, prog, id);
        }
        let chunk = len.div_ceil(parts as u64);
        // 各分片起点;末尾凑不满 parts 份就少开线程
        let starts: Vec<u64> = (0..parts as u64)
            .map(|i| offset + i * chunk)
            .take_while(|&off| off < offset + len)
            .collect();

        let results = std::thread::scope(|s| {
            let handles: Vec<_> = starts
                .into_iter()
                .map(|off| {
                    let l = chunk.min(offset + len - off);
                    // Progress 是 Arc 包裹,clone 给线程很便宜
                    let prog = prog.clone();
                    s.spawn(move || {
                        Self::fetch_part(&self.http, dlink, &self.token.access_token, off, l, &prog, id)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("下载线程 panic"))
                .collect::<Vec<_>>()
        });

        let mut out = Vec::with_capacity(len as usize);
        for r in results {
            out.extend(r?);
        }
        Ok(out)
    }

    /// 单段 Range 拉取(核心,串行/并发共用):
    /// UA 必须是 pan.baidu.com,302 自动跟随,支持断点续传。
    /// 响应体按 64KB 边收边记进度,而不是一次性 bytes()。
    fn fetch_part(
        http: &reqwest::blocking::Client,
        dlink: &str,
        token: &str,
        offset: u64,
        len: u64,
        prog: &Progress,
        id: u64,
    ) -> Result<Vec<u8>> {
        // dlink 本身带 query 参数,token 直接拼在后面
        let url = format!("{dlink}&access_token={token}");
        let end = offset + len - 1;
        let resp = http
            .get(&url)
            .header(reqwest::header::USER_AGENT, DL_UA)
            // 实测 CDN 会 403 掉"同一 keep-alive 连接上的第二个 Range 请求",
            // 每个分片用独立连接(curl 的行为),代价只是每片一次 TLS 握手
            .header(reqwest::header::CONNECTION, "close")
            .header(reqwest::header::RANGE, format!("bytes={offset}-{end}"))
            .send()?;
        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            return Err(anyhow::Error::new(PanError::LinkExpired));
        }
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
            || resp.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE
        {
            return Err(anyhow::Error::new(PanError::RateLimited));
        }
        let mut resp = resp.error_for_status()?;
        let status = resp.status();
        let mut buf = Vec::with_capacity(len as usize);
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = resp.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            prog.add(id, n as u64);
        }
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            Ok(buf)
        } else {
            // 服务器忽略 Range 返回了 200 全量:自己切窗口(仅当文件不大时可行,兜底逻辑)
            let s = offset as usize;
            let e = (offset + len) as usize;
            if s >= buf.len() {
                Ok(Vec::new())
            } else {
                Ok(buf[s..e.min(buf.len())].to_vec())
            }
        }
    }

    /// 三段式上传第一步 precreate。
    /// 返回 None = 秒传(网盘已有同 md5 内容,到此就完成了);
    /// Some((uploadid, 还要传的分片序号)):响应里 block_list 缺省时按"全都要传"处理
    fn precreate(
        &mut self,
        path: &str,
        size: u64,
        slice_md5s: &[String],
    ) -> Result<Option<(String, Vec<u64>)>> {
        let v = self.rest_post(
            "/rest/2.0/xpan/file",
            &[("method".into(), "precreate".into())],
            &[
                ("path".into(), path.to_string()),
                ("size".into(), size.to_string()),
                ("isdir".into(), "0".into()),
                ("autoinit".into(), "1".into()),
                ("block_list".into(), serde_json::to_string(slice_md5s)?),
                // rtype=3:目标已存在时覆盖,对齐 FUSE 的覆盖写语义
                ("rtype".into(), "3".into()),
            ],
        )?;
        self.check_api(&v)?;
        let rt = v.get("return_type").and_then(|t| t.as_i64()).unwrap_or(1);
        if rt == 2 {
            return Ok(None);
        }
        let uploadid = v
            .get("uploadid")
            .and_then(|u| u.as_str())
            .context("precreate 响应缺 uploadid")?
            .to_string();
        let need = v
            .get("block_list")
            .and_then(|b| b.as_array())
            .map(|arr| arr.iter().filter_map(|i| i.as_u64()).collect())
            .unwrap_or_default();
        Ok(Some((uploadid, need)))
    }

    /// 三段式上传第二步 superfile2:传一个分片,网盘按 md5 校验落临时区。
    /// token 失效时刷新后整片重传一次
    fn upload_slice(
        &mut self,
        uploadid: &str,
        path: &str,
        partseq: u64,
        data: &[u8],
        prog: &Progress,
    ) -> Result<()> {
        let partseq_s = partseq.to_string();
        for attempt in 0..2 {
            let id = prog.begin(
                path,
                &format!("上传分片#{partseq}"),
                partseq * UPLOAD_SLICE,
                data.len() as u64,
            );
            // Part 的 reader 要求 'static,只能每轮克隆一份 4MB
            let part = reqwest::blocking::multipart::Part::reader_with_length(
                ProgReader {
                    inner: Cursor::new(data.to_vec()),
                    prog: prog.clone(),
                    id,
                },
                data.len() as u64,
            )
            .file_name(format!("part{partseq}"))
            .mime_str("application/octet-stream")?;
            let form = reqwest::blocking::multipart::Form::new().part("file", part);
            let resp = self
                .up_http
                .post(format!("{PCS_HOST}/rest/2.0/pcs/superfile2"))
                .query(&[
                    ("method", "upload"),
                    ("type", "tmpfile"),
                    ("access_token", self.token.access_token.as_str()),
                    ("path", path),
                    ("uploadid", uploadid),
                    ("partseq", partseq_s.as_str()),
                ])
                .multipart(form)
                .send()?
                .error_for_status()?;
            let v: Value = resp.json()?;
            if let Some(errno) = v.get("errno").and_then(|e| e.as_i64()) {
                if (errno == -6 || errno == 111) && attempt == 0 {
                    prog.end(id, false);
                    tracing::warn!("token 失效(errno={errno}),刷新后重传分片#{partseq}");
                    self.refresh()?;
                    continue;
                }
            }
            self.check_api(&v)?;
            let ok = v.get("md5").and_then(|m| m.as_str()).is_some();
            prog.end(id, ok);
            if !ok {
                bail!("superfile2 响应缺 md5:{v}");
            }
            return Ok(());
        }
        bail!("分片#{partseq} 两次尝试都失败")
    }

    /// 三段式上传第三步 create:合并分片正式落地,返回新文件的 fs_id
    fn create_file(
        &mut self,
        path: &str,
        size: u64,
        uploadid: &str,
        slice_md5s: &[String],
    ) -> Result<u64> {
        let v = self.rest_post(
            "/rest/2.0/xpan/file",
            &[("method".into(), "create".into())],
            &[
                ("path".into(), path.to_string()),
                ("size".into(), size.to_string()),
                ("isdir".into(), "0".into()),
                ("uploadid".into(), uploadid.to_string()),
                ("block_list".into(), serde_json::to_string(slice_md5s)?),
                ("rtype".into(), "3".into()),
            ],
        )?;
        self.check_api(&v)?;
        v.get("fs_id")
            .and_then(|f| f.as_u64())
            .context("create 响应缺 fs_id")
    }

    /// 建目录(method=create 的 isdir=1 形态),返回目录 id
    fn mkdir_raw(&mut self, path: &str) -> Result<u64> {
        let v = self.rest_post(
            "/rest/2.0/xpan/file",
            &[("method".into(), "create".into())],
            &[
                ("path".into(), path.to_string()),
                ("isdir".into(), "1".into()),
            ],
        )?;
        self.check_api(&v)?;
        v.get("fs_id")
            .and_then(|f| f.as_u64())
            .context("mkdir 响应缺 fs_id")
    }

    /// 文件管理统一入口:copy/move/rename/delete。
    /// form 的 filelist 是 JSON 数组;async=1 自适应(小任务同步返回结果)
    fn filemanager(&mut self, opera: &str, filelist: String) -> Result<()> {
        let v = self.rest_post(
            "/rest/2.0/xpan/file",
            &[
                ("method".into(), "filemanager".into()),
                ("opera".into(), opera.to_string()),
            ],
            &[
                ("async".into(), "1".into()),
                ("filelist".into(), filelist),
            ],
        )?;
        self.check_api(&v)?;
        // 每个条目还有各自的 errno(-9 不存在等),全 0 才算成
        if let Some(arr) = v.get("info").and_then(|i| i.as_array()) {
            for item in arr {
                if let Some(errno) = item.get("errno").and_then(|e| e.as_i64()) {
                    if errno != 0 {
                        let msg = item
                            .get("path")
                            .and_then(|p| p.as_str())
                            .unwrap_or("?");
                        return Err(anyhow::Error::new(match errno {
                            -9 => PanError::NotFound,
                            -7 => PanError::PermissionDenied,
                            _ => PanError::Api(format!("errno={errno} {msg}")),
                        }));
                    }
                }
            }
        }
        Ok(())
    }

    /// 删除文件/目录(filemanager 的 delete,filelist 传路径数组)
    fn delete_raw(&mut self, path: &str) -> Result<()> {
        self.filemanager("delete", serde_json::to_string(&[path])?)
    }

    /// 移动/改名:path 是源,dest 是目标目录,newname 是目标目录下的新名字。
    /// 纯改名 = dest 取原父目录;统一走 move 一种格式,
    /// 避开 rename 接口 newname 格式的文档歧义(裸名还是全路径)
    fn mv_raw(&mut self, path: &str, dest: &str, newname: &str) -> Result<()> {
        let filelist = serde_json::to_string(&[serde_json::json!({
            "path": path,
            "dest": dest,
            "newname": newname,
        })])?;
        self.filemanager("move", filelist)
    }

    /// 整文件三段式上传(原 fs.rs 的逻辑收进来):
    /// 逐片算 md5 → precreate(秒传直接回)→ 传缺片 → create
    fn upload_raw(
        &mut self,
        path: &str,
        src: &mut std::fs::File,
        size: u64,
        prog: &Progress,
    ) -> Result<NetFile> {
        let name = path.rsplit('/').next().unwrap_or("").to_string();
        let mk = |id: String| NetFile {
            id: id.clone(),
            fid: id,
            path: path.to_string(),
            name: name.clone(),
            is_dir: false,
            size,
            mtime: now_secs() as i64,
        };

        // 分片规则:4MB 起步,>4GB 自动放大,保证 ≤1024 片(官方上限)
        let slice = upload_slice_size(size);
        let nblocks = size.div_ceil(slice);
        let mut buf = vec![0u8; slice as usize];
        let mut md5s: Vec<String> = Vec::with_capacity(nblocks as usize);
        if nblocks == 0 {
            // 空文件:block_list=[] 会被 precreate 拒(errno=2)。
            // 用空串 md5 当唯一分片,实测 precreate 直接回 need=[] 免传,create 收尾
            md5s.push(format!("{:x}", md5::compute([])));
        }
        for i in 0..nblocks {
            let want = slice.min(size - i * slice) as usize;
            src.read_exact_at(&mut buf[..want], i * slice)?;
            md5s.push(format!("{:x}", md5::compute(&buf[..want])));
        }
        src.seek(SeekFrom::Start(0)).ok();

        match self.precreate(path, size, &md5s)? {
            None => Ok(mk(String::new())), // 秒传:拿不到 fs_id,id 留空
            Some((uploadid, need)) => {
                for seq in need {
                    if seq >= nblocks {
                        bail!("precreate 要分片#{seq},本地只有 {nblocks} 片");
                    }
                    let off = seq * slice;
                    let want = slice.min(size - off) as usize;
                    let mut data = vec![0u8; want];
                    src.read_exact_at(&mut data, off)?;
                    self.upload_slice(&uploadid, path, seq, &data, prog)?;
                }
                let id = self.create_file(path, size, &uploadid, &md5s)?;
                Ok(mk(id.to_string()))
            }
        }
    }
}

// ---------- PanClient 实现:把原生接口适配成统一 trait ----------

impl PanClient for BaiduClient {
    fn list_dir(&mut self, dir: &str) -> Result<Vec<NetFile>> {
        self.list_dir_raw(dir)
    }

    fn get_dlink(&mut self, f: &NetFile) -> Result<String> {
        let fs_id: u64 = f.id.parse().unwrap_or(0);
        self.get_dlink_raw(fs_id)
    }

    fn read_range(
        &self,
        url: &str,
        offset: u64,
        len: u64,
        parts: usize,
        prog: &Progress,
        id: u64,
    ) -> Result<Vec<u8>> {
        self.read_range_raw(url, offset, len, parts, prog, id)
    }

    fn upload(
        &mut self,
        path: &str,
        src: &mut std::fs::File,
        size: u64,
        prog: &Progress,
    ) -> Result<NetFile> {
        self.upload_raw(path, src, size, prog)
    }

    fn mkdir(&mut self, path: &str) -> Result<NetFile> {
        let id = self.mkdir_raw(path)?;
        Ok(NetFile {
            id: id.to_string(),
            fid: id.to_string(),
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap_or("").to_string(),
            is_dir: true,
            size: 0,
            mtime: now_secs() as i64,
        })
    }

    fn delete(&mut self, f: &NetFile) -> Result<()> {
        self.delete_raw(&f.path)
    }

    fn mv(&mut self, f: &NetFile, dest_dir: &str, newname: &str) -> Result<()> {
        self.mv_raw(&f.path, dest_dir, newname)
    }

    fn quota(&mut self) -> Result<(u64, u64)> {
        self.quota_raw()
    }

    fn account(&mut self) -> Result<String> {
        let u = self.uinfo()?;
        Ok(format!(
            "百度账号:{}  网盘账号:{}  用户ID:{}  会员:{}",
            u.baidu_name,
            u.netdisk_name,
            u.uk,
            match u.vip_type {
                2 => "超级会员",
                1 => "会员",
                _ => "普通",
            }
        ))
    }

    fn kind(&self) -> PanKind {
        PanKind::Baidu
    }
}
