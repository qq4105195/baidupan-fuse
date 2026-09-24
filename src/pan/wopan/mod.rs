//! 联通云盘(WoPan)客户端:逆向的 Web 端协议(pan.wo.cn),
//! 参考 wopan-sdk-go / wopan-cf-worker / wopan-open 三家实现交叉验证。
//!
//! 协议要点:
//! - 两个 dispatcher 网关(api-user / wohome),方法名放 body.header.key,param AES 加密
//! - 全按目录 id 寻址(根="0"),path→dir_id 映射在这层自己维护,fs 层无感
//! - 双文件标识:条目 id(改名/移动/删除用)+ 内容 fid(下载用)
//! - 上传是 8MB 分片 multipart 直传,没有三段式没有秒传探测;上传后列表有秒级延迟
//! - token 失效信号是 RSP_CODE 9999(SDK 登录)/1001(Web token),refresh token 轮换
//!
//! 详见 docs/wopan-integration.md 的调研记录。

pub mod api;
pub mod crypto;

use super::{NetFile, PanClient, PanError, PanKind};
use crate::progress::Progress;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::os::unix::fs::FileExt;
use std::thread::sleep;
use std::time::Duration;

use api::{Channel, ParamMode};

/// 上传分片大小(参考实现 8MB)
const PART_SIZE: u64 = 8 << 20;
/// 每片上传失败重试次数(300ms 递增退避,对齐 wopan-cf-worker)
const PART_RETRIES: u32 = 3;
/// 上传后等列表可见的重试(实测最长 ~30s,这里 best-effort 拿条目 id)
const VISIBLE_TRIES: u32 = 3;
const VISIBLE_WAIT: Duration = Duration::from_secs(2);

// ---------- token 持久化 ----------

/// wopan 凭据:~/.config/baidupan-fuse/wopan.json
#[derive(Serialize, Deserialize, Clone)]
pub struct WopanToken {
    pub access_token: String,
    /// 可能没有(Web cookie 抓的 token);有才能自动续期
    #[serde(default)]
    pub refresh_token: String,
    /// access 过期时间,unix 秒(拿不到 expires_in 时按 7 天估)
    pub expires_at: u64,
    /// AppQueryUser 返回的 userId(掩码手机号),查容量要用
    #[serde(default)]
    pub user_id: String,
}

impl WopanToken {
    fn path() -> std::path::PathBuf {
        super::config_dir().join("wopan.json")
    }

    fn load() -> Result<Self> {
        super::load_json(&Self::path())
            .with_context(|| "读 wopan 凭据失败,请先登录(bdfs --backend wopan login)")
    }

    fn save(&self) -> Result<()> {
        super::save_json_atomic(&Self::path(), self)
    }
}

/// 凭据文件是否已保存(菜单状态行用)
pub fn token_saved() -> bool {
    WopanToken::path().is_file()
}

// ---------- 登录(独立函数:登录时还没有 client) ----------

fn plain_http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?)
}

/// 第一步:给手机号发短信验证码。
/// 这个端点不走 dispatcher:body 顶层是 func/clientId,没有 header/sign
pub fn send_sms_code(phone: &str) -> Result<()> {
    if !phone.chars().all(|c| c.is_ascii_digit()) || phone.len() != 11 {
        bail!("手机号格式不对:{phone}");
    }
    let http = plain_http()?;
    let param = crypto::encrypt_b64(
        crypto::LOGIN_CLIENT_SECRET,
        json!({ "operateType": "1", "phone": phone }).to_string().as_bytes(),
    );
    let body = json!({ "func": "app_send", "clientId": api::LOGIN_CLIENT_ID, "param": param });
    let resp = http
        .post(format!("{}/api-user/sendMessageCodeBase", api::BASE_URL))
        .headers(common_header_map())
        .json(&body)
        .send()?
        .error_for_status()?;
    let v: Value = resp.json()?;
    let status = v.get("STATUS").and_then(|s| s.as_str()).unwrap_or("");
    if status != "200" {
        bail!("发短信失败:STATUS={status} {}", v.get("MSG").and_then(|m| m.as_str()).unwrap_or(""));
    }
    if let Some(code) = v.pointer("/RSP/RSP_CODE").and_then(|c| c.as_str()) {
        if code != "0000" {
            bail!(
                "发短信失败:code={code} {}",
                v.pointer("/RSP/RSP_DESC").and_then(|d| d.as_str()).unwrap_or("")
            );
        }
    }
    Ok(())
}

/// 第二步:短信验证码换 token(LoginByMobileV2,DATA 明文返回)。
/// 成功后顺手 AppQueryUser 验证并记下 userId
pub fn sms_login(phone: &str, code: &str) -> Result<()> {
    let http = plain_http()?;
    let param = crypto::encrypt_b64(
        crypto::LOGIN_CLIENT_SECRET,
        json!({
            "clientSecret": String::from_utf8_lossy(crypto::LOGIN_CLIENT_SECRET),
            "phone": phone,
            "smsCode": code,
        })
        .to_string()
        .as_bytes(),
    );
    // header 和常规 dispatcher 一样;body 是登录专属形态:
    // clientId 用登录专用的那对,且没有 secret 字段
    let body = json!({
        "clientId": api::LOGIN_CLIENT_ID,
        "param": param,
    });
    let resp = http
        .post(format!("{}/api-user/dispatcher", api::BASE_URL))
        .headers(common_header_map())
        .json(&json!({
            "header": api::make_header(Channel::ApiUser, "LoginByMobileV2"),
            "body": body,
        }))
        .send()?
        .error_for_status()?;
    let v: Value = resp.json()?;
    let rsp = api::parse_envelope(&v)?;
    if rsp.code != "0000" {
        bail!("登录失败:code={} {}", rsp.code, rsp.desc);
    }
    // DATA 明文;键名兼容蛇形/驼峰(参考实现见过多种)
    let data = rsp.data;
    let pick = |keys: &[&str]| -> String {
        keys.iter()
            .find_map(|k| data.get(*k).and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string()
    };
    let access = pick(&["accessToken", "access_token", "token", "Token"]);
    if access.is_empty() {
        bail!("登录响应里没有 accessToken:{data}");
    }
    let refresh = pick(&["refreshToken", "refresh_token"]);
    let expires_in = data.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(7 * 86400);
    let token = WopanToken {
        access_token: access,
        refresh_token: refresh,
        expires_at: api::now_secs() + expires_in.saturating_sub(120),
        user_id: String::new(),
    };
    token.save()?;
    println!("登录成功,凭据已存到 {:?}", WopanToken::path());

    // 验证 + 预热 userId
    if let Ok(mut c) = WopanClient::from_config() {
        match c.account() {
            Ok(acc) => println!("账号:{acc}"),
            Err(e) => println!("警告:登录成功但验证账号失败:{e:#}"),
        }
    }
    Ok(())
}

/// 手动粘 token(Web 抓包兜底):没有 refresh,失效就要重弄
pub fn token_login(access_token: &str, refresh_token: &str) -> Result<()> {
    if access_token.len() < 16 {
        bail!("accessToken 至少 16 个字符(前 16 位是加密密钥)");
    }
    let token = WopanToken {
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_at: api::now_secs() + 7 * 86400,
        user_id: String::new(),
    };
    token.save()?;
    println!("凭据已存到 {:?}", WopanToken::path());
    Ok(())
}

// ---------- HTTP 公共头 ----------

/// 模拟 pan.wo.cn 网页端的头:风控吃 Referer/Origin/UA 这一套,
/// 下载直链也认这套(第三方 Referer 会被拒,pan.wo.cn 自己的放行)
fn common_header_map() -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(reqwest::header::ORIGIN, "https://pan.wo.cn".parse().unwrap());
    h.insert(reqwest::header::REFERER, "https://pan.wo.cn/".parse().unwrap());
    h.insert(reqwest::header::USER_AGENT, api::UA.parse().unwrap());
    h
}

// ---------- 客户端 ----------

pub struct WopanClient {
    http: reqwest::blocking::Client,
    /// 上传专用:8MB 分片 + 慢上行,放宽超时
    up_http: reqwest::blocking::Client,
    token: WopanToken,
    /// 上传节点(GetZoneInfo 发现,进程内缓存;失败兜底天津节点)
    zone: Option<String>,
    /// 远端目录路径 → 目录 id。列目录时自愈式重建,删目录时整前缀清除
    dir_ids: HashMap<String, String>,
}

impl WopanClient {
    pub fn from_config() -> Result<Self> {
        let mut c = Self {
            http: plain_http()?,
            up_http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()?,
            token: WopanToken::load()?,
            zone: None,
            dir_ids: HashMap::new(),
        };
        c.ensure_token()?;
        Ok(c)
    }

    /// token 快过期且有 refresh_token 就提前刷(没有 refresh 的只能等 9999/1001 被动处理)
    fn ensure_token(&mut self) -> Result<()> {
        if !self.token.refresh_token.is_empty() && self.token.expires_at <= api::now_secs() + 120 {
            self.refresh()?;
        }
        Ok(())
    }

    /// 刷新 token。**refresh token 是轮换的**,新值必须立刻落盘
    fn refresh(&mut self) -> Result<()> {
        if self.token.refresh_token.is_empty() {
            bail!("没有 refresh_token,无法自动续期,请重新登录");
        }
        let rsp = self.dispatch_raw(
            Channel::ApiUser,
            "AppRefreshToken",
            &json!({
                "refreshToken": self.token.refresh_token,
                "clientSecret": String::from_utf8_lossy(crypto::CLIENT_SECRET),
            }),
            ParamMode::ClientSecret,
        )?;
        let access = rsp
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("刷新响应缺 access_token:{rsp}"))?;
        let refresh = rsp
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.token.refresh_token)
            .to_string();
        self.token.access_token = access.to_string();
        self.token.refresh_token = refresh;
        self.token.expires_at =
            api::now_secs() + rsp.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(7 * 86400) - 120;
        self.token.save()?;
        tracing::info!("wopan token 已刷新,新过期时间 {}", self.token.expires_at);
        Ok(())
    }

    /// 单次 dispatcher 调用(不带刷新重试)
    fn dispatch_raw(
        &self,
        channel: Channel,
        method: &str,
        param: &Value,
        mode: ParamMode,
    ) -> Result<Value> {
        let body = api::build_body(channel, method, param, mode, &self.token.access_token)?;
        let mut req = self
            .http
            .post(format!("{}/{}/dispatcher", api::BASE_URL, channel.as_str()))
            .headers(common_header_map())
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        // Accesstoken 头只有 wohome 通道带(注意这个拼写,协议就这么定义的)
        if channel == Channel::Wohome {
            req = req.header("Accesstoken", &self.token.access_token);
        }
        let resp = req.json(&body).send()?.error_for_status()?;
        let v: Value = resp.json()?;
        let rsp = api::parse_envelope(&v)?;
        if rsp.code != "0000" {
            bail!("wopan {} 失败:code={} {}", method, rsp.code, rsp.desc);
        }
        // DATA 解密密钥按通道:wohome → token 前 16;api-user → CLIENT_SECRET
        let key = match channel {
            Channel::Wohome => crypto::token_key(&self.token.access_token)?,
            Channel::ApiUser => crypto::CLIENT_SECRET,
        };
        api::unwrap_data(rsp.data, key)
    }

    /// 带鉴权的 dispatcher 调用:9999/1001(token 失效)自动刷新重试一次;
    /// 刷新后仍失效 → PanError::AuthExpired(fs 层会归成 EIO,菜单提示重新登录)
    fn dispatch(
        &mut self,
        channel: Channel,
        method: &str,
        param: &Value,
        mode: ParamMode,
    ) -> Result<Value> {
        let is_expired = |e: &anyhow::Error| {
            e.to_string().contains("code=9999") || e.to_string().contains("code=1001")
        };
        match self.dispatch_raw(channel, method, param, mode) {
            Ok(v) => Ok(v),
            Err(e) => {
                if is_expired(&e) && channel == Channel::Wohome {
                    tracing::warn!("wopan token 失效,刷新后重试 {method}");
                    self.refresh()?;
                    match self.dispatch_raw(channel, method, param, mode) {
                        Ok(v) => Ok(v),
                        Err(e2) if is_expired(&e2) => {
                            Err(anyhow::Error::new(PanError::AuthExpired).context(e2))
                        }
                        Err(e2) => Err(e2),
                    }
                } else {
                    Err(e)
                }
            }
        }
    }

    // ---------- 用户/容量 ----------

    /// AppQueryUser(api-user 通道,DATA 加密)
    fn query_user(&mut self) -> Result<Value> {
        self.dispatch_raw(
            Channel::ApiUser,
            "AppQueryUser",
            &json!({ "accessToken": self.token.access_token }),
            ParamMode::ClientSecret,
        )
    }

    fn ensure_user_id(&mut self) -> Result<String> {
        if self.token.user_id.is_empty() {
            let u = self.query_user()?;
            self.token.user_id = u
                .get("userId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("AppQueryUser 响应缺 userId:{u}"))?
                .to_string();
            self.token.save().ok();
        }
        Ok(self.token.user_id.clone())
    }

    // ---------- 目录 id 寻址 ----------

    /// 上传节点:GetZoneInfo 发现(param 明文、body key:true),失败兜底天津节点
    fn zone_url(&mut self) -> Result<String> {
        if let Some(z) = &self.zone {
            return Ok(z.clone());
        }
        let url = match self.dispatch(
            Channel::Wohome,
            "GetZoneInfo",
            &json!({ "appId": api::ZONE_APP_ID }),
            ParamMode::PlainKeyTrue,
        ) {
            Ok(v) => v
                .get("url")
                .and_then(|u| u.as_str())
                .map(|s| s.trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty()),
            Err(e) => {
                tracing::warn!("GetZoneInfo 失败({e:#}),用兜底上传节点");
                None
            }
        };
        let z = url.unwrap_or_else(|| api::FALLBACK_ZONE_URL.to_string());
        self.zone = Some(z.clone());
        Ok(z)
    }

    /// QueryAllFiles 一页(systemDirs + files 合并;两个数组都要遍历)
    fn query_page(&mut self, dir_id: &str, page: u32) -> Result<Vec<Value>> {
        let data = self.dispatch(
            Channel::Wohome,
            "QueryAllFiles",
            &json!({
                "spaceType": "0",
                "parentDirectoryId": dir_id,
                "pageNum": page,
                "pageSize": api::PAGE_SIZE,
                "sortRule": 1, // 名称升序,顺序稳定
                "clientId": api::CLIENT_ID,
            }),
            ParamMode::Secret,
        )?;
        let mut out = Vec::new();
        for key in ["systemDirs", "files"] {
            if let Some(arr) = data.get(key).and_then(|v| v.as_array()) {
                out.extend(arr.iter().cloned());
            }
        }
        Ok(out)
    }

    /// 翻页拉全一个目录(响应没有总数字段,返回不满一页判停)
    fn list_entries(&mut self, dir_id: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut page = 0u32;
        loop {
            let entries = self.query_page(dir_id, page)?;
            let n = entries.len() as u32;
            out.extend(entries);
            if n < api::PAGE_SIZE {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    /// 目录路径 → 目录 id。缓存命中直接回;未命中从根逐段走(每次列目录都会自愈缓存)
    fn resolve_dir(&mut self, path: &str) -> Result<String> {
        let norm = crate::fs::normalize_dir(path);
        if let Some(id) = self.dir_ids.get(&norm) {
            return Ok(id.clone());
        }
        if norm == "/" {
            self.dir_ids.insert(norm, "0".into());
            return Ok("0".into());
        }
        let mut cur_id = "0".to_string();
        let mut cur_path = String::new();
        for seg in norm.trim_start_matches('/').split('/') {
            let parent = if cur_path.is_empty() {
                "/".to_string()
            } else {
                cur_path.clone()
            };
            cur_path = crate::fs::join_path(&parent, seg);
            // 在父目录里找这一段(目录条目)
            let found = self
                .list_entries(&cur_id)?
                .into_iter()
                .find(|e| {
                    e.get("name").and_then(|v| v.as_str()) == Some(seg)
                        && entry_type(e) == Some(false)
                })
                .ok_or_else(|| {
                    anyhow::Error::new(PanError::NotFound)
                        .context(format!("目录 {cur_path} 不存在(或不是目录)"))
                })?;
            let id = found
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("目录条目缺 id:{found}"))?
                .to_string();
            self.dir_ids.insert(cur_path.clone(), id.clone());
            cur_id = id;
        }
        Ok(cur_id)
    }

    /// 删除 path 及其子树在 dir_ids 里的映射(目录删除/改名后旧路径全部作废)
    fn purge_dir_ids(&mut self, path: &str) {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        self.dir_ids.retain(|k, _| !k.starts_with(&prefix));
        self.dir_ids.remove(path);
    }

    // ---------- 上传 ----------

    /// 8MB 分片直传 upload2C。没有三段式:最后一片 code=0000 服务端自动合并,
    /// 秒传由服务端自行判断(响应直接给 fid)
    fn upload_raw(
        &mut self,
        path: &str,
        src: &mut std::fs::File,
        size: u64,
        prog: &Progress,
    ) -> Result<NetFile> {
        let name = path.rsplit('/').next().unwrap_or("").to_string();
        let parent = crate::fs::parent_of(path);
        let dir_id = self.resolve_dir(&parent)?;
        let zone = self.zone_url()?;

        let file_info = json!({
            "spaceType": "0",
            "directoryId": dir_id,
            "batchNo": api::batch_no_now(),
            "fileName": name,
            "fileSize": size,
            "fileType": api::file_type_code(&name),
        });
        let file_info_enc = crypto::encrypt_b64(
            crypto::token_key(&self.token.access_token)?,
            file_info.to_string().as_bytes(),
        );
        let unique_id = format!("{}_{}", api::now_ms(), api::rand_chars(6));
        let url = format!("{zone}/openapi/client/upload2C");
        let total = size.div_ceil(PART_SIZE).max(1);

        let mut fid = String::new();
        for idx in 1..=total {
            let off = (idx - 1) * PART_SIZE;
            let want = PART_SIZE.min(size.saturating_sub(off)) as usize;
            let mut data = vec![0u8; want];
            src.read_exact_at(&mut data, off)?;

            let mut done = false;
            for attempt in 0..PART_RETRIES {
                let pid = prog.begin(path, &format!("上传分片#{idx}"), off, want as u64);
                // Part 的 reader 要求 'static,每轮克隆一份(和百度端同款处理)
                let part = reqwest::blocking::multipart::Part::reader_with_length(
                    ProgReader {
                        inner: Cursor::new(data.clone()),
                        prog: prog.clone(),
                        id: pid,
                    },
                    want as u64,
                )
                .file_name(name.clone())
                .mime_str("application/octet-stream")?;
                let form = reqwest::blocking::multipart::Form::new()
                    .text("uniqueId", unique_id.clone())
                    .text("accessToken", self.token.access_token.clone())
                    .text("fileName", name.clone())
                    // Web 前端的遗留字面量,别改成空串
                    .text("psToken", "undefined")
                    .text("fileSize", size.to_string())
                    .text("totalPart", total.to_string())
                    .text("channel", "wocloud")
                    .text("directoryId", dir_id.clone())
                    .text("fileInfo", file_info_enc.clone())
                    .text("partSize", want.to_string())
                    .text("partIndex", idx.to_string())
                    .part("file", part);
                let r = self
                    .up_http
                    .post(&url)
                    .headers(common_header_map())
                    .multipart(form)
                    .send()
                    .and_then(|resp| resp.error_for_status());
                match r {
                    Ok(resp) => {
                        let v: Value = resp.json()?;
                        let ok = v.get("code").and_then(|c| c.as_str()) == Some("0000");
                        prog.end(pid, ok);
                        if ok {
                            if let Some(f) = v.pointer("/data/fid").and_then(|f| f.as_str()) {
                                fid = f.to_string();
                            }
                            done = true;
                            break;
                        }
                        tracing::warn!("分片#{idx} 响应异常:{v}");
                    }
                    Err(e) => {
                        prog.end(pid, false);
                        tracing::warn!("分片#{idx} 请求失败:{e}");
                    }
                }
                if attempt + 1 < PART_RETRIES {
                    sleep(Duration::from_millis(300 * (attempt as u64 + 1)));
                }
            }
            if !done {
                bail!("分片#{idx}/{total} 上传失败(重试 {PART_RETRIES} 次),暂存文件已保留");
            }
        }

        // 上传后列表有最终一致延迟(实测最长 ~30s):等一会让新条目可见,
        // 顺带拿条目 id(响应只给 fid;万一响应没给 fid,也顺这里补上)
        let mut entry_id = String::new();
        for _ in 0..VISIBLE_TRIES {
            let entries = self.list_entries(&dir_id)?;
            if let Some(e) = entries.into_iter().find(|e| {
                e.get("name").and_then(|v| v.as_str()) == Some(name.as_str())
                    && entry_type(e) == Some(false)
            }) {
                entry_id = e.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if fid.is_empty() {
                    fid = e.get("fid").and_then(|v| v.as_str()).unwrap_or("").to_string();
                }
                break;
            }
            sleep(VISIBLE_WAIT);
        }
        if entry_id.is_empty() {
            tracing::warn!("上传完成但列表暂未见到 {name}(秒级延迟),条目 id 下次列表自愈");
        }

        Ok(NetFile {
            id: entry_id,
            fid,
            path: path.to_string(),
            name,
            is_dir: false,
            size,
            mtime: api::now_secs() as i64,
        })
    }
}

// ---------- 条目解析 ----------

/// type 字段:0=目录 1=文件;实测出现过 "7"/"9"/空串等未知值 → None(调用方跳过)
fn entry_type(e: &Value) -> Option<bool> {
    let t = e.get("type").and_then(|v| {
        v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    })?;
    match t {
        0 => Some(true),
        1 => Some(false),
        _ => None,
    }
}

/// 列表条目 → NetFile(path 由调用方拼)
fn netfile_from_entry(e: &Value, dir: &str) -> Option<NetFile> {
    let id = e.get("id").and_then(|v| v.as_str())?.to_string();
    let name = e.get("name").and_then(|v| v.as_str())?.to_string();
    let is_dir = entry_type(e)?;
    // size 数字/字符串两种都出现
    let size = e
        .get("size")
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(0);
    // 时间字段优先级 updateTime → modifyTime → createTime;坏数据宽容为 0
    let mtime = ["updateTime", "modifyTime", "createTime"]
        .iter()
        .find_map(|k| e.get(*k).and_then(|v| v.as_str()))
        .map(api::parse_wopan_time)
        .unwrap_or(0);
    Some(NetFile {
        path: crate::fs::join_path(dir, &name),
        fid: e.get("fid").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        id,
        name,
        is_dir,
        size,
        mtime,
    })
}

/// 包在 multipart 分片外面的读适配器:上传体边发边记进度(和百度端同款)
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

// ---------- PanClient 实现 ----------

impl PanClient for WopanClient {
    fn list_dir(&mut self, dir: &str) -> Result<Vec<NetFile>> {
        let norm = crate::fs::normalize_dir(dir);
        let dir_id = self.resolve_dir(&norm)?;
        let entries = self.list_entries(&dir_id)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in &entries {
            match netfile_from_entry(e, &norm) {
                Some(f) => {
                    if f.is_dir {
                        self.dir_ids.insert(f.path.clone(), f.id.clone());
                    }
                    out.push(f);
                }
                None => {
                    tracing::warn!(
                        "跳过未知类型条目 {}(type={:?},wopan 新增了条目形态?)",
                        e.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                        e.get("type")
                    );
                }
            }
        }
        self.dir_ids.insert(norm, dir_id);
        Ok(out)
    }

    fn get_dlink(&mut self, f: &NetFile) -> Result<String> {
        if f.fid.is_empty() {
            return Err(anyhow::Error::new(PanError::Api("条目缺 fid,无法取下载链接".into())));
        }
        let data = self.dispatch(
            Channel::Wohome,
            "GetDownloadUrlV2",
            &json!({ "type": "1", "fidList": [f.fid], "clientId": api::CLIENT_ID }),
            ParamMode::Secret,
        )?;
        let list = data
            .get("list")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("GetDownloadUrlV2 响应缺 list:{data}"))?;
        for item in list {
            if item.get("fid").and_then(|v| v.as_str()) == Some(f.fid.as_str()) {
                if let Some(u) = item.get("downloadUrl").and_then(|v| v.as_str()) {
                    return Ok(u.to_string());
                }
            }
        }
        Err(anyhow!("下载链接响应里没有 {} 的条目:{data}", f.fid))
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
        if parts <= 1 || len < (1 << 20) {
            return Self::fetch_part(&self.http, url, offset, len, prog, id);
        }
        let chunk = len.div_ceil(parts as u64);
        let starts: Vec<u64> = (0..parts as u64)
            .map(|i| offset + i * chunk)
            .take_while(|&off| off < offset + len)
            .collect();
        let results = std::thread::scope(|s| {
            let handles: Vec<_> = starts
                .into_iter()
                .map(|off| {
                    let l = chunk.min(offset + len - off);
                    let prog = prog.clone();
                    let url = url.to_string();
                    s.spawn(move || Self::fetch_part(&self.http, &url, off, l, &prog, id))
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
        let norm = crate::fs::normalize_dir(path);
        let parent = crate::fs::parent_of(&norm);
        let name = norm.trim_start_matches('/').rsplit('/').next().unwrap_or("").to_string();
        if name.is_empty() {
            return Err(anyhow::Error::new(PanError::Api("目录名为空".into())));
        }
        let parent_id = self.resolve_dir(&parent)?;
        let data = self.dispatch(
            Channel::Wohome,
            "CreateDirectory",
            &json!({
                "spaceType": "0",
                "parentDirectoryId": parent_id,
                "directoryName": name,
                "clientId": api::CLIENT_ID,
            }),
            ParamMode::Secret,
        )?;
        let id = data
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("CreateDirectory 响应缺 id:{data}"))?
            .to_string();
        self.dir_ids.insert(norm.clone(), id.clone());
        Ok(NetFile {
            id,
            fid: String::new(),
            path: norm,
            name,
            is_dir: true,
            size: 0,
            mtime: api::now_secs() as i64,
        })
    }

    fn delete(&mut self, f: &NetFile) -> Result<()> {
        self.dispatch(
            Channel::Wohome,
            "DeleteFile",
            &json!({
                "spaceType": "0",
                "vipLevel": "0",
                "dirList": if f.is_dir { vec![f.id.clone()] } else { vec![] },
                "fileList": if f.is_dir { vec![] } else { vec![f.id.clone()] },
                "clientId": api::CLIENT_ID,
            }),
            ParamMode::Secret,
        )?;
        // 目录:整棵子树的路径映射都作废
        self.purge_dir_ids(&f.path);
        Ok(())
    }

    fn mv(&mut self, f: &NetFile, dest_dir: &str, newname: &str) -> Result<()> {
        let dest = crate::fs::normalize_dir(dest_dir);
        let same_dir = crate::fs::parent_of(&f.path) == dest;
        // 移动(同目录改名跳过)。param 里有个恒 false 的 secret 字段,协议如此
        if !same_dir {
            let target = self.resolve_dir(&dest)?;
            self.dispatch(
                Channel::Wohome,
                "MoveFile",
                &json!({
                    "targetDirId": target,
                    "sourceType": "0",
                    "targetType": "0",
                    "dirList": if f.is_dir { vec![f.id.clone()] } else { vec![] },
                    "fileList": if f.is_dir { vec![] } else { vec![f.id.clone()] },
                    "secret": false,
                    "clientId": api::CLIENT_ID,
                }),
                ParamMode::Secret,
            )?;
        }
        // 改名(名字没变就跳过)。注意 type 是 int,与列表里的字符串 type 不同体系
        if newname != f.name {
            self.dispatch(
                Channel::Wohome,
                "RenameFileOrDirectory",
                &json!({
                    "spaceType": "0",
                    "type": if f.is_dir { 0 } else { 1 },
                    "fileType": if f.is_dir { "0".to_string() } else { api::file_type_code(newname).to_string() },
                    "id": f.id,
                    "name": newname,
                    "clientId": api::CLIENT_ID,
                }),
                ParamMode::Secret,
            )?;
        }
        let new_path = crate::fs::join_path(&dest, newname);
        if f.is_dir {
            self.purge_dir_ids(&f.path);
            self.dir_ids.insert(new_path, f.id.clone());
        }
        Ok(())
    }

    fn quota(&mut self) -> Result<(u64, u64)> {
        let uid = self.ensure_user_id()?;
        let data = self.dispatch(
            Channel::Wohome,
            "QueryCloudUsageInfo",
            &json!({ "phoneNum": uid, "clientId": api::CLIENT_ID }),
            ParamMode::Secret,
        )?;
        // 用 byte 开头的字段(字节);不带的是 KB,别用错量纲。
        // 参考实现里容量字段有的包在 usageInfo 里、有的直接在 DATA 顶层,两个都试
        let num = |v: &Value, k: &str| -> u64 {
            v.get(k)
                .and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
                .unwrap_or(0)
        };
        let usage = data.get("usageInfo").cloned().unwrap_or_else(|| data.clone());
        Ok((num(&usage, "byteTotalSize"), num(&usage, "byteUsedSize")))
    }

    fn account(&mut self) -> Result<String> {
        let u = self.query_user()?;
        let name = u.get("userName").and_then(|v| v.as_str()).unwrap_or("");
        let id = u.get("userId").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            bail!("AppQueryUser 响应缺 userId:{u}");
        }
        if self.token.user_id != id {
            self.token.user_id = id.to_string();
            self.token.save().ok();
        }
        Ok(format!("联通账号:{name}({id})"))
    }

    fn kind(&self) -> PanKind {
        PanKind::Wopan
    }
}

impl WopanClient {
    /// 单段 Range 拉取:
    /// 直链对 Referer 敏感(空或 pan.wo.cn 放行,第三方拒绝),带浏览器 UA,
    /// 重定向跟随;403 = 链接过期 → PanError::LinkExpired(fs 层弃缓存重取)
    fn fetch_part(
        http: &reqwest::blocking::Client,
        url: &str,
        offset: u64,
        len: u64,
        prog: &Progress,
        id: u64,
    ) -> Result<Vec<u8>> {
        let end = offset + len.saturating_sub(1);
        let resp = http
            .get(url)
            .headers(common_header_map())
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
            // 服务器忽略 Range 回了 200 全量:自己切窗口(兜底,仅小文件可行)
            let s = offset as usize;
            let e = (offset + len) as usize;
            if s >= buf.len() {
                Ok(Vec::new())
            } else {
                Ok(buf[s..e.min(buf.len())].to_vec())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 条目解析() {
        let e = json!({
            "id": "abc123", "fid": "FID1", "name": "电影.mkv", "type": "1",
            "size": "2048", "updateTime": "20220221185658"
        });
        let f = netfile_from_entry(&e, "/video").unwrap();
        assert_eq!(f.id, "abc123");
        assert_eq!(f.fid, "FID1");
        assert!(!f.is_dir);
        assert_eq!(f.size, 2048);
        assert_eq!(f.path, "/video/电影.mkv");
        assert_eq!(f.mtime, 1645441018);

        // 目录没有 fid;未知 type 跳过
        let d = json!({ "id": "d1", "name": "dir", "type": "0" });
        assert!(netfile_from_entry(&d, "/").unwrap().is_dir);
        assert!(netfile_from_entry(&json!({ "id": "x", "name": "?", "type": "7" }), "/").is_none());
    }
}
