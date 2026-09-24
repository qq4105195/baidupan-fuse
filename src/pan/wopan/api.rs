//! wopan dispatcher 协议层:信封构造/解析、时间与随机数、文件类型码。
//! 协议不是 REST:所有方法 POST 到两个 dispatcher 网关,方法名放 body.header.key,
//! 参数整体 AES 加密放 body.param(见 crypto.rs)。

use super::crypto;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

pub const BASE_URL: &str = "https://panservice.mail.wo.cn";
/// 主 client(wohome / api-user / wocloud 三通道共用)
pub const CLIENT_ID: &str = "1001000021";
/// H5 短信登录专用 client(和主凭据是两对,勿混)
pub const LOGIN_CLIENT_ID: &str = "1001000035";
/// GetZoneInfo 的 appId
pub const ZONE_APP_ID: &str = "10000001";
/// 上传节点兜底(天津)
pub const FALLBACK_ZONE_URL: &str = "https://tjupload.pan.wo.cn";
/// 模拟 pan.wo.cn 网页端的 UA(三家参考实现都用 Chrome/114)
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/114.0.0.0 Safari/537.36";
/// 目录列表每页条数(参考实现 50~100,取 100;实测上限未知)
pub const PAGE_SIZE: u32 = 100;

// ---------- 通道与 body 形态 ----------

#[derive(Clone, Copy, PartialEq)]
pub enum Channel {
    /// 登录/用户信息/刷新:body 用 clientSecret 加密,不带 Accesstoken 头
    ApiUser,
    /// 文件操作:param 用 token 前 16 字符加密,带 Accesstoken 头
    Wohome,
}

impl Channel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Channel::ApiUser => "api-user",
            Channel::Wohome => "wohome",
        }
    }
}

/// body 里 param 的包装方式
#[derive(Clone, Copy)]
pub enum ParamMode {
    /// 常规:{secret:true, param:AES(token key)}——wohome 文件操作
    Secret,
    /// api-user 通道:{clientId, secret:true, param:AES(CLIENT_SECRET)}——用户信息/刷新
    ClientSecret,
    /// param 明文 JSON、body 顶层 key:true——GetZoneInfo 这类
    PlainKeyTrue,
}

/// 构造 header 段(登录流程 body 形态特殊,单独复用这一段)
pub fn make_header(channel: Channel, method: &str) -> Value {
    let res_time = now_ms() as i64;
    let req_seq = pseudo_req_seq();
    json!({
        "key": method,
        "resTime": res_time,
        "reqSeq": req_seq,
        "channel": channel.as_str(),
        "sign": crypto::sign(method, res_time, req_seq, channel.as_str()),
        "version": "",
    })
}

/// 构造 dispatcher 请求的完整 body
pub fn build_body(channel: Channel, method: &str, param: &Value, mode: ParamMode, token: &str) -> Result<Value> {
    let header = make_header(channel, method);
    let body = match mode {
        ParamMode::Secret => json!({
            "secret": true,
            "param": crypto::encrypt_b64(crypto::token_key(token)?, param.to_string().as_bytes()),
        }),
        ParamMode::ClientSecret => json!({
            "clientId": CLIENT_ID,
            "secret": true,
            "param": crypto::encrypt_b64(crypto::CLIENT_SECRET, param.to_string().as_bytes()),
        }),
        ParamMode::PlainKeyTrue => json!({ "key": true, "param": param }),
    };
    Ok(json!({ "header": header, "body": body }))
}

// ---------- 响应解析 ----------

/// dispatcher 响应外层 {STATUS, MSG, RSP:{RSP_CODE, RSP_DESC, DATA}} 的解析结果
pub struct Rsp {
    /// "0000" 成功;"9999"/"1001" token 失效;其余业务错误
    pub code: String,
    pub desc: String,
    /// 原始 DATA(可能是明文对象、加密 base64 字符串、空串)
    pub data: Value,
}

/// 解析外层信封(HTTP 200 + STATUS="200" 才到这里)
pub fn parse_envelope(v: &Value) -> Result<Rsp> {
    let status = v.get("STATUS").and_then(|s| s.as_str()).unwrap_or("");
    if status != "200" {
        let msg = v.get("MSG").and_then(|m| m.as_str()).unwrap_or("");
        bail!("wopan 网关 STATUS={status} {msg}");
    }
    let rsp = v.get("RSP").ok_or_else(|| anyhow!("wopan 响应缺 RSP:{v}"))?;
    Ok(Rsp {
        code: rsp.get("RSP_CODE").and_then(|c| c.as_str()).unwrap_or("").to_string(),
        desc: rsp.get("RSP_DESC").and_then(|c| c.as_str()).unwrap_or("").to_string(),
        data: rsp.get("DATA").cloned().unwrap_or(Value::Null),
    })
}

/// DATA 三态归一:空串 → Null;加密字符串 → 解密再解析 JSON;对象原样
pub fn unwrap_data(data: Value, key: &[u8; 16]) -> Result<Value> {
    match data {
        Value::String(s) if s.is_empty() => Ok(Value::Null),
        Value::String(s) => {
            let pt = crypto::decrypt_b64(key, &s)?;
            Ok(serde_json::from_slice(&pt)?)
        }
        other => Ok(other),
    }
}

// ---------- 时间/随机 ----------

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn now_secs() -> u64 {
    now_ms() / 1000
}

/// reqSeq:协议要求 [100000,108998] 的"随机"数;用纳秒熵够用(签名本身不校验随机性)
fn pseudo_req_seq() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    100000 + (nanos % 8999) as u32
}

/// 上传 uniqueId 的尾巴:6 位随机字母(xorshift,不追求密码学安全)
pub fn rand_chars(n: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut state = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e3779b97f4a7c15)
        | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            CHARS[(state as usize) % CHARS.len()] as char
        })
        .collect()
}

/// 天数 → 公历(Howard Hinnant 算法),给 batchNo 格式化用
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 公历 → 天数(同上,逆向),给 mtime 解析用
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 上传 batchNo:yyyyMMddHHmmss(服务端只当批次标签,UTC 即可)
pub fn batch_no_now() -> String {
    let secs = now_secs() as i64;
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}{mo:02}{d:02}{:02}{:02}{:02}", tod / 3600, tod % 3600 / 60, tod % 60)
}

/// 列表条目时间 "YYYYMMDDHHMMSS" → unix 秒。
/// wopan 用的是北京时间(UTC+8);解析失败宽容返回 0,别让一个坏条目炸掉整个目录
pub fn parse_wopan_time(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.len() != 14 || !b.iter().all(u8::is_ascii_digit) {
        return 0;
    }
    let num = |r: std::ops::Range<usize>| -> i64 { s[r].parse().unwrap_or(0) };
    let days = days_from_civil(num(0..4), num(4..6) as u32, num(6..8) as u32);
    days * 86400 + num(8..10) * 3600 + num(10..12) * 60 + num(12..14) - 8 * 3600
}

// ---------- 文件类型码 ----------

/// 扩展名 → wopan fileType 码(对齐 wopan-cf-worker 的映射表):
/// 1 图 2 视频 3 音频 4 文档 5 其他;目录用 "0"
pub fn file_type_code(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if ext.is_empty() || name.ends_with('.') {
        return "5";
    }
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tiff" | "tif" | "psd" | "raw"
        | "tga" | "livp" | "heic" | "ico" => "1",
        "asf" | "avi" | "dv" | "flv" | "mkv" | "mov" | "mp4" | "rm" | "swf" | "ts" | "vob"
        | "wmv" | "rmvb" | "mpg" | "webm" | "ogv" | "m4v" | "f4v" | "3gp" | "m3u8" | "dat" => "2",
        "ac3" | "flac" | "m4a" | "mp2" | "mp3" | "wav" | "wma" | "ape" | "mpc" | "tta"
        | "ogg" | "amr" | "aac" | "aiff" | "au" | "mka" | "wv" => "3",
        "txt" | "rtf" | "doc" | "docx" | "ppt" | "pptx" | "xls" | "xlsx" | "md" | "pdf"
        | "hlp" | "csv" => "4",
        _ => "5",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 信封形状() {
        let body = build_body(
            Channel::Wohome,
            "QueryAllFiles",
            &json!({"a": 1}),
            ParamMode::Secret,
            "91d4b946-1234-4abd-9e2f-a1b2c3d4e5f6",
        )
        .unwrap();
        let h = body.pointer("/header").unwrap();
        assert_eq!(h["key"], "QueryAllFiles");
        assert_eq!(h["channel"], "wohome");
        assert_eq!(h["version"], "");
        assert_eq!(h["sign"].as_str().unwrap().len(), 32);
        // param 是 base64 密文
        let param = body.pointer("/body/param").unwrap().as_str().unwrap();
        assert!(!param.is_empty());
    }

    #[test]
    fn 时间往返() {
        // 2022-02-21 18:56:58 +08 = 2022-02-21 10:56:58 UTC = 1645441018
        assert_eq!(parse_wopan_time("20220221185658"), 1645441018);
        assert_eq!(parse_wopan_time(""), 0);
        assert_eq!(parse_wopan_time("bad"), 0);
        assert_eq!(batch_no_now().len(), 14);
    }

    #[test]
    fn 文件类型码() {
        assert_eq!(file_type_code("a.png"), "1");
        assert_eq!(file_type_code("电影.mkv"), "2");
        assert_eq!(file_type_code("x.MP3"), "3");
        assert_eq!(file_type_code("doc.pdf"), "4");
        assert_eq!(file_type_code("archive.tar.gz"), "5");
        assert_eq!(file_type_code("无扩展名"), "5");
    }
}
