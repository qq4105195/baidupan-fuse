//! 占位符身份 blob:随占位符一起存进 NTFS(cldapi 硬限制 ≤4KB),
//! 回调发生时直接从 [Request::file_blob] 拿,免打一次 API。
//!
//! 紧凑 JSON `{id,d,s,m}` = fs_id / is_dir / size / mtime。
//! 故意不存路径:路径由同步根相对位置推出,远端改名也不会让 blob 失效。
//! fs_id=0(秒传拿不到新 id)是合法值,水合/回传时按父目录列表回查。

use crate::baidu::NetFile;
use serde::{Deserialize, Serialize};

/// cldapi 身份 blob 上限(我们的紧凑 JSON 几十字节,余量巨大)
pub const MAX_BLOB: usize = 4096;

/// 占位符身份
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// 远端 fs_id;0 = 未知(秒传产物,用前按路径回查)
    pub id: u64,
    /// 是否目录
    pub d: u8,
    /// 字节大小(目录为 0)
    pub s: u64,
    /// mtime(epoch 秒)
    pub m: i64,
}

impl From<&NetFile> for Identity {
    fn from(f: &NetFile) -> Self {
        Self {
            id: f.fs_id,
            d: u8::from(f.is_dir),
            s: f.size,
            m: f.mtime,
        }
    }
}

/// 编码成 blob(JSON;纯数字字段,序列化不会失败,长度必在限制内)
pub fn encode(nf: &NetFile) -> Vec<u8> {
    let v = serde_json::to_vec(&Identity::from(nf)).expect("纯数字字段序列化不会失败");
    debug_assert!(v.len() <= MAX_BLOB);
    v
}

/// 解码;坏 blob(旧版本/被改过)当没有处理
pub fn decode(blob: &[u8]) -> Option<Identity> {
    serde_json::from_slice(blob).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nf() -> NetFile {
        NetFile {
            fs_id: 1234567890123,
            path: "/apps/x/a/b.bin".into(),
            name: "b.bin".into(),
            is_dir: false,
            size: 1 << 40,
            mtime: 1758300000,
        }
    }

    #[test]
    fn blob往返() {
        let blob = encode(&nf());
        assert!(blob.len() <= MAX_BLOB);
        assert_eq!(decode(&blob), Some(Identity::from(&nf())));
    }

    #[test]
    fn 坏blob当空() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(b"{oops"), None);
        // 半截 JSON 解不出来
        assert_eq!(decode(&encode(&nf())[..5]), None);
    }
}
