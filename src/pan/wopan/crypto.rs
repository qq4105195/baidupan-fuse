//! 联通云盘协议的加密/签名:AES-128-CBC(param 加解密)+ MD5(header 签名)。
//! 密钥/IV 都是 Web 端(wopan-sdk-go / wopan-cf-worker / wopan-open 三方一致)逆向的固定值。

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use anyhow::{bail, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// 全局固定 IV(协议就这么定的,安全性谈不上,但得照抄)
pub const IV: &[u8; 16] = b"wNSOYIB1k1DjY5lA";

/// api-user 通道的 AES 密钥(即 clientSecret,16 字节 ASCII)
pub const CLIENT_SECRET: &[u8; 16] = b"XFmi9GS2hzk98jGX";
/// H5 短信登录通道的密钥(另一对 client 凭据,勿与上面混用)
pub const LOGIN_CLIENT_SECRET: &[u8; 16] = b"iELf0UL07o6I8eRK";

/// wohome 通道的密钥:accessToken 前 16 个字符(借用入参)
pub fn token_key(access_token: &str) -> Result<&[u8; 16]> {
    // token 是 UUID 形如 91d4b946-xxxx-...,前 16 字符足够凑满 16 字节
    let bytes = access_token.as_bytes();
    if bytes.len() < 16 {
        bail!("accessToken 长度不足 16,无法派生 wohome 密钥(可能已失效)");
    }
    Ok(bytes[..16].try_into().expect("已校验长度"))
}

/// AES-128-CBC + PKCS7 加密后标准 base64(请求 param 用)
pub fn encrypt_b64(key: &[u8; 16], plain: &[u8]) -> String {
    let ct = Aes128CbcEnc::new(key.into(), IV.into()).encrypt_padded_vec_mut::<Pkcs7>(plain);
    B64.encode(ct)
}

/// base64 → AES-128-CBC 解密(响应 DATA 用);padding 不对说明密钥错了
pub fn decrypt_b64(key: &[u8; 16], b64: &str) -> Result<Vec<u8>> {
    let ct = B64
        .decode(b64.trim())
        .map_err(|e| anyhow::anyhow!("base64 解码失败:{e}"))?;
    let pt = Aes128CbcDec::new(key.into(), IV.into())
        .decrypt_padded_vec_mut::<Pkcs7>(&ct)
        .map_err(|_| anyhow::anyhow!("AES 解密失败(padding 不对,多半是密钥/响应形态不匹配)"))?;
    Ok(pt)
}

/// dispatcher header 签名:md5(key + resTime + reqSeq + channel + version),
/// version 恒为空串。十六进制小写
pub fn sign(method: &str, res_time: i64, req_seq: u32, channel: &str) -> String {
    format!("{:x}", md5::compute(format!("{method}{res_time}{req_seq}{channel}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 向量用 openssl enc -aes-128-cbc 生成,保证和参考实现(TS/Go/Python)对齐
    #[test]
    fn 加密向量_openssl对拍() {
        assert_eq!(
            encrypt_b64(CLIENT_SECRET, br#"{"a":1}"#),
            "fEhWhxcQMOQXzgHbQEA2jw=="
        );
        assert_eq!(
            encrypt_b64(
                CLIENT_SECRET,
                br#"{"operateType":"1","phone":"18612345678"}"#
            ),
            "im3icG1o4fcGtnS1uGsgCnxBc54jVh49ujUQJh5QcC11rYLXkcWsm0CXokkZLdDY"
        );
        // 中文(多字节 UTF-8)
        assert_eq!(
            encrypt_b64(CLIENT_SECRET, "中文名.txt".as_bytes()),
            "nIL/YlU9lfETgIcCFpNKQg=="
        );
    }

    #[test]
    fn 解密往返() {
        for pt in [
            &b"{}"[..],
            br#"{"files":[]}"#,
            "中文/路径/文件 名.tar.gz".as_bytes(),
            vec![0u8; 48].as_slice(), // 恰好整块:PKCS7 要补一整块
        ] {
            let key = b"0123456789abcdef";
            let ct = encrypt_b64(key, pt);
            assert_eq!(decrypt_b64(key, &ct).unwrap(), pt);
        }
    }

    #[test]
    fn token_key_取前16字符() {
        let k = token_key("91d4b946-1234-4abd-9e2f-a1b2c3d4e5f6").unwrap();
        assert_eq!(k, b"91d4b946-1234-4a");
        assert!(token_key("short").is_err());
    }

    #[test]
    fn 签名形状() {
        // 固定输入的 md5,防手滑改拼接顺序
        assert_eq!(sign("QueryAllFiles", 1690000000000, 178232, "wohome").len(), 32);
        assert_eq!(sign("A", 1, 2, "c"), format!("{:x}", md5::compute(b"A12c")));
    }
}
