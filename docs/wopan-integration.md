# 联通云盘(WoPan)支持:调研与集成方案

> **实施状态(2026-09-24)**:阶段 0~4 已全部落地——`src/pan/` 抽象层 + 百度平移、
> `src/pan/wopan/` 完整客户端(crypto 有 openssl 对拍单测)、fs 层多后端化、
> CLI 全局 `--backend` + 短信/token 登录、控制台多网盘菜单(切换网盘/分网盘凭据、
> 挂载点、systemd 模板 bdfs@.service + 实例 bdfs@baidu / bdfs@wopan、进度文件)。
> 离线可验的都已验证
> (11 个单测、设置迁移、菜单/CLI 冒烟);**联通真实账号的在线验证待做**,
> 上表"首跑验证清单"里的未知项(totalPart/直链 TTL/分片大小/吞吐)首跑时回填。

调研对象(2026-09,三份逆向实现交叉验证):

| 仓库 | 语言 | 定位 | 最后活跃 |
|---|---|---|---|
| [xhofe/wopan-sdk-go](https://github.com/xhofe/wopan-sdk-go) | Go | alist/OpenList 官方驱动用的 SDK | 2024-08 |
| [crmmc/wopan-open](https://github.com/crmmc/wopan-open) | Python | PySide6 桌面客户端(Web Cookie 登录) | 活跃 |
| [qhappyc/wopan-cf-worker](https://github.com/qhappyc/wopan-cf-worker) | TS | Cloudflare Worker 网关(短信登录+上传下载) | 活跃 |

三者协议同源(wopan-open 明说以 OpenList 为事实参考),细节互相印证,分歧点在文末列出。

---

## 1. 协议核心(三家一致,可直接信)

**不是 REST**。两个 dispatcher 网关 + 一个直传端点,方法名放 body:

```
POST https://panservice.mail.wo.cn/api-user/dispatcher   # 登录/用户信息/刷新
POST https://panservice.mail.wo.cn/wohome/dispatcher     # 全部文件操作
POST {zoneURL}/openapi/client/upload2C                   # 上传分片(multipart 直传)
```

### 1.1 请求信封

```json
{
  "header": {"key": "QueryAllFiles", "resTime": 1690000000000,
             "reqSeq": 178232, "channel": "wohome",
             "sign": "<md5hex>", "version": ""},
  "body": {"secret": true, "param": "<AES 加密后 base64 的 param JSON>"}
}
```

- `sign = md5( key + resTime + reqSeq + channel + "" )` 十六进制;resTime 毫秒时间戳;reqSeq ∈ [100000, 108998] 随机
- param 为空的方法(GetZoneInfo 等)body 顶层换成 `"key": true`,不加密

### 1.2 加密(Rust 移植的主要新依赖)

- **AES-128-CBC + PKCS7 + 标准 base64**
- IV 固定:`wNSOYIB1k1DjY5lA`
- 密钥按通道:
  - `api-user` 通道 → 硬编码 `XFmi9GS2hzk98jGX`
  - `wohome` 通道 → **accessToken 的前 16 个字符**
- 响应里的 `RSP.DATA` 三态都要处理:明文对象 / 加密 base64 字符串(同密钥解密)/ 空串

### 1.3 必带 HTTP header

```
Origin:  https://pan.wo.cn
Referer: https://pan.wo.cn/
User-Agent: Chrome/114 桌面版(三家都硬编码这个 UA)
Accesstoken: <token>        # 注意拼写;仅 wohome 通道带
```

无 deviceId、无设备指纹、无 cookie(API 请求不带 cookie)。风控面就是 UA+Origin/Referer+固定 IV 的 AES。

### 1.4 响应信封与错误码

```json
{"STATUS": "200", "MSG": "ok", "RSP": {"RSP_CODE": "0000", "RSP_DESC": "success", "DATA": ...}}
```

| 信号 | 含义 | 处理 |
|---|---|---|
| `RSP_CODE = 0000` | 成功 | |
| `RSP_CODE = 9999` | access token 失效(SDK 登录的 token) | 刷新 token 重试一次 |
| `RSP_CODE = 1001` | token 失效(Web cookie token) | 同上 |
| CDN 直链 HTTP 403 | **下载链接过期**(不是权限拒绝) | 弃缓存重新 GetDownloadUrl,≤3 次 |
| HTTP 429 / 503 | 限流 | 退避 + 降并发 |
| 上传响应 `code != "0000"` | 分片失败 | 重试该片 |
| `STATUS != "200"` | 协议层错误 | 报错 |

---

## 2. 登录(三条路径,推荐短信)

| 路径 | 出处 | 输入 | 可否刷新 |
|---|---|---|---|
| **A. 纯短信登录** | cf-worker | 手机号 + 短信验证码 | ✅ 有 refresh_token |
| B. 密码+短信两段式 | sdk-go | 手机号 + 密码 + 短信 | ✅ 有 refresh_token |
| C. Web Cookie | wopan-open | 浏览器抓 `WoCloud-Web-Token` | ❌ 无刷新,失效重抓 |

**推荐 A**(交互最轻,与 bdfs login 现有的"浏览器授权码"体验同级):

1. `POST /api-user/sendMessageCodeBase`,body **不走 dispatcher**:`{"func":"app_send","clientId":"1001000035","param":AES(登录密钥, {operateType:"1", phone})}`
   - 登录专用凭据:`clientId=1001000035`,`clientSecret=iELf0UL07o6I8eRK`(与主凭据 1001000021/XFmi9GS2hzk98jGX 不同,勿混)
2. dispatcher `LoginByMobileV2`(api-user 通道):param `{clientSecret, phone, smsCode}`,响应 DATA **明文**含 `accessToken/refreshToken`
3. access token 有效期 ≈ 7 天(`expires_in: 604799`);**refresh token 是轮换的——每次刷新必须立刻持久化新的**

刷新:dispatcher `AppRefreshToken`(api-user):param `{refreshToken, clientSecret}`,响应含新 access+refresh。
策略照抄 baidu.rs 现有骨架:过期前主动刷 + 收到 9999/1001 被动刷一次,只是 refresh token 语义不同(轮换,必须保存)。

Cookie token(路径 C)可作为 `bdfs login --backend wopan --token <串>` 手动兜底:值要 URL-decode 再去双引号(`%22xxx%22`),无 refresh 能力,只适合救急。

---

## 3. 文件模型(与百度的本质差异)

```
QueryAllFiles 条目:
{ id: "32位hex条目id", fid: "内容实体id(仅文件)", name, type: "0"目录/"1"文件,
  fileType: "0"目录/"1"图/"2"视频/"3"音频/"4"文档/"5"其他,
  size: 数字或字符串(两种都出现!), createTime/updateTime: "YYYYMMDDHHMMSS",
  parentDirectoryId }
```

| | 百度 | wopan | 对 fs.rs 的影响 |
|---|---|---|---|
| 文件标识 | 数字 fs_id | **字符串 id(条目)+ fid(下载用)** | fs_id: u64 → String |
| 寻址 | 路径 | **目录 id**(根 = `"0"`) | 客户端层自己维护 path→dir_id |
| 列表 | start/limit 有总数 | pageNum 从 0,pageSize ≤100,**无总数**,`len>=pageSize` 续翻 | 翻页循环重写 |
| 列表附带 | — | `systemDirs` + `files` 两个数组都要遍历 | |
| 条目类型 | isdir 0/1 | type 字符串,**实测出现过 "7"/"9"/空串** | 未知 type 跳过+告警,别报错 |
| mtime | unix 秒 | `YYYYMMDDHHMMSS`(updateTime→modifyTime→createTime) | 解析+容错 |
| 秒传 | precreate + md5 | **无**(服务端自行判断,响应直接给 fid) | 上传流程不同 |
| 回收站 | — | DeleteFile 即进回收站,另有 EmptyRecycleData | 删除语义相同 |

---

## 4. 操作对照(wohome dispatcher)

| 操作 | key | param 要点(都含 clientId) |
|---|---|---|
| 列目录 | `QueryAllFiles` | `{spaceType:"0", parentDirectoryId, pageNum, pageSize, sortRule}` |
| 下载直链 | `GetDownloadUrlV2` | `{type:"1", fidList:[fid]}`(**无 spaceType**);V1 是 `{fidList, spaceType}` 响应为数组——先实现 V2,V1 备胎 |
| mkdir | `CreateDirectory` | `{spaceType, parentDirectoryId, directoryName}` → 响应 `{id}` |
| 重命名 | `RenameFileOrDirectory` | `{type: int 0/1, fileType, id, name}`(type 是 int,与列表的字符串 type 不同体系) |
| 移动 | `MoveFile` | `{targetDirId, sourceType, targetType, dirList, fileList, secret:false}` |
| 删除 | `DeleteFile` | `{vipLevel:"0", dirList, fileList}` |
| 配额 | `QueryCloudUsageInfo` | `{phoneNum}`——**phoneNum 填 AppQueryUser 返回的 userId**(掩码手机号),不是裸手机号;容量用 `usageInfo.byteTotalSize/byteUsedSize`(字节;别用 totalSize,那是 KB) |
| 用户信息 | `AppQueryUser`(api-user) | `{accessToken}` → `{userId, userName}` |
| 上传节点 | `GetZoneInfo` | param `{appId:"10000001"}`,body 顶层 `"key":true` 不加密 → `{url}`,兜底 `https://tjupload.pan.wo.cn` |

家庭云(`spaceType:"1"` + familyId,默认家庭 id 从 `FamilyUserCurrentEncode` 取)和私密空间(`"4"` + psToken)同一批端点加字段即可,v1 不做。

## 5. 下载(实测结论)

- 直链 = `https://…/openapi/download?fid=<签名令牌>`,**支持 Range(实测 206 + Content-Range)**,FUSE 随机读无障碍
- 直连必须带 `Referer: https://pan.wo.cn/` + Chrome UA(空 Referer 也放行,**第三方 Referer 拒绝**),重定向要跟随
- **URL 原样透传,不要重新编码**(签名对字符编码敏感)
- 403 = 链接过期 → 重取直链 ≤3 次——与 baidu.rs 现有的 is_forbidden → 弃 dlink 缓存重试完全同构,直接复用 fs.rs 的 fetch_with_retry
- 链接 TTL 未知(三家都没记录)→ dlink 缓存默认给短 TTL(建议 10 分钟起,实测后调)
- 吞吐未知。wopan-open 默认单线程下载、上传 16 并发。FUSE 读路径先 `--parallel 1` 对齐百度策略,实测再调

## 6. 上传(无三段式)

```
GetZoneInfo → zone(进程内缓存)
循环分片 POST {zone}/openapi/client/upload2C  (multipart,响应明文 JSON,不走 dispatcher)
```

表单字段(每片都带):`uniqueId`(毫秒时间戳[_6位随机],每文件一次)、`accessToken`、`fileName`、`fileSize`、`totalPart`、`partSize`、`partIndex`(**从 1 起**)、`channel:"wocloud"`、`directoryId`、`psToken`(字面量字符串 `"undefined"`)、`fileInfo`(AES(token前16) 加密的 `{spaceType, directoryId, batchNo: yyyyMMddHHmmss, fileName, fileSize:int, fileType}`)、`file`(分片二进制)。

- 分片 8MB(Go/CF)或 5-16MB 可配(Python);**totalPart 用 ceil**(Go SDK 整除截断是 bug:9MB 会算成 1 片)
- **没有 commit/完成接口**:最后一片 `code=="0000"` 即自动合并;分片乱序到达无影响(实测 SHA256 校验通过)
- 响应 `data.fid` = 新文件 fid,同时就是下载标识
- 无哈希、无秒传探测(服务端自行判断)
- **上传后列表有最终一致延迟(实测最长 ~30 秒才出现在列表里)** → 上传完成后的 readdir 可见性要做小重试;好在 FUSE 的 lookup 走写会话能看到文件

---

## 7. 集成设计

### 7.1 抽象层:trait PanClient

fs.rs 对 BaiduClient 的依赖面就 10 个方法(见调研),抽成:

```rust
// src/pan/mod.rs
pub struct NetFile {
    pub id: String,     // 后端文件标识(baidu: fs_id 十进制串;wopan: 条目 id)
    pub fid: String,    // 下载标识(baidu: 同 id;wopan: fid)
    pub path: String,   // 服务端绝对路径(baidu 原生;wopan 由客户端维护)
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,     // unix 秒,统一在这层归一
}

pub enum PanError {          // fs.rs downcast ApiError → 改成 match 这个
    NotFound,                // → ENOENT
    PermissionDenied,        // → EACCES
    AuthExpired,             // 客户端内部已刷新重试过仍失败才抛出
    LinkExpired,             // 下载直链 403,fs 层弃缓存重取(替代 is_forbidden)
    RateLimited,             // 429/503
    Api(String),             // 其余 → EIO
}

pub trait PanClient {
    fn list_dir(&mut self, dir: &str) -> Result<Vec<NetFile>>;
    fn get_dlink(&mut self, f: &NetFile) -> Result<String>;      // wopan 用 f.fid
    fn read_range(&self, url: &str, off: u64, len: u64, parts: usize,
                  prog: &Progress, id: u64) -> Result<Vec<u8>>;
    fn upload(&mut self, path: &str, src: &mut std::fs::File, size: u64,
              prog: &Progress) -> Result<()>;   // ★ 整体上传,后端内部自己分段
    fn mkdir(&mut self, path: &str) -> Result<NetFile>;
    fn delete(&mut self, f: &NetFile) -> Result<()>;
    fn mv(&mut self, f: &NetFile, dest_dir: &str, newname: &str) -> Result<()>;
    fn quota(&mut self) -> Result<(u64, u64)>;
    fn user_info(&mut self) -> Result<String>;   // 展示用账号名
}
```

**关键取舍:上传收进 trait 内部**。百度把 fs.rs 里现成的"算分片 md5 → precreate → 缺片 → create"整体搬进 BaiduClient::upload(秒传逻辑随之内聚);wopan 的 upload 就是 GetZoneInfo + 8MB 分片循环。fs.rs 只保留暂存/补洞/flush 触发——WriteSession 结构不动。

**path 寻址保留在 trait 表面**:wopan 是 id 寻址,由 WopanClient 内部维护 `path → dir_id` 映射(每次 list_dir 从条目的 id/parentDirectoryId 自愈式重建,根 "0" 种子;rename/delete 后失效对应前缀,查不到就用父目录列表反查)。这样 fs.rs 的 ino↔path、目录缓存、块缓存全部原样复用,是改动最小的路线。

### 7.2 模块布局

```
src/
├─ pan/mod.rs          # NetFile / PanError / trait PanClient
├─ pan/baidu.rs        # 现 baidu.rs 平移,impl PanClient;upload 吸收三段式
├─ pan/wopan/mod.rs    # WopanClient:impl PanClient + path→dir_id 映射
├─ pan/wopan/crypto.rs # AES-CBC/PKCS7 + md5 sign + base64(离线单测)
├─ pan/wopan/api.rs    # dispatcher 信封、双通道调用、DATA 三态解包、9999/1001 刷新重试
├─ pan/wopan/login.rs  # sendMessageCodeBase + LoginByMobileV2 + AppRefreshToken(轮换持久化)
└─ pan/wopan/upload.rs # GetZoneInfo 缓存 + upload2C 分片循环
```

新依赖:`aes = "0.8"`、`cbc = "0.1"(features=["alloc","block-padding"])`、`base64 = "0.22"`;md5/reqwest/serde 已有。reqSeq 用时间戳纳秒取模即可,不必引入 rand。

### 7.3 凭据与 CLI

```
~/.config/baidupan-fuse/
├─ config.json + token.json      # 百度,原样不动(兼容)
├─ wopan.json                    # {access_token, refresh_token, expires_at, user_id}
├─ settings.json                 # 加 backend 字段,默认 "baidu"
└─ uploads-wopan/                # wopan 写暂存(与百度分开,防互删)
```

- `bdfs --backend wopan login --phone 186…`:发短信 → 输码 → 存 token
- `bdfs --backend wopan info / ls / mount /mnt/wopan`,控制台菜单顶部加后端选择
- `MountOption::Subtype` 按后端给("baidupan"/"wopanfs")
- progress.json 每条传输记录加 backend 字段,菜单「8. 传输进度」两个后端都能看
- 挂载形态:**独立挂载点**(一个后端一个 mount),统一根挂 `/baidu` `/wopan` 需要虚拟分发层,v2 再说

### 7.4 fs.rs 改动清单(全部机械性)

1. `fs_id: u64` → `id/fid: String`(dlink 缓存键、WriteSession.fs_id、秒传返回 Option<u64> → 不再需要)
2. `ApiError` downcast / `is_forbidden` → `PanError`(fetch_with_retry 的 403 分支改 match LinkExpired)
3. `upload_session` 里 md5/precreate/upload_slice/create 段 → 一行 `client.upload(&path, &mut f, target, prog)`
4. wopan 上传后的父目录缓存失效保留,另加"列表可见性小重试"(2s×5 次,列表里出现同名才放行,超时仅告警)
5. `settings.rs`、menu、systemd 生成加 backend 维度

## 8. 三家分歧与未知(首跑验证清单)

| 项 | 分歧/未知 | 处置 |
|---|---|---|
| totalPart 计算 | Go 整除(9MB→1 片,bug)/ CF 用 ceil | **ceil** |
| 分片大小 | 8MB(Go/CF)/ 5MB 默认(Python,5-16 可配) | 8MB 起步,做参数 |
| 直链 TTL | 无人记录 | 缓存 10 分钟起,403 自愈兜底 |
| V1 vs V2 下载接口 | Python 用 V1,Go/CF 用 V2;Go 作者对 V1 clientId 标 `???` | V2 优先 |
| Rename 的 fileType | Go 用 ClassifyRule 表换算,Python 本地按扩展名猜 | 先按扩展名猜(1图2视频3音频4文档5其他),被拒再上 ClassifyRule |
| pageSize 上限 | 无文档,三家 50/100 | 100 起步实测 |
| 下载吞吐/并发行为 | 未知(Python 默认单线程) | parallel=1 起步实测 |
| 单文件大小上限 | 会员档位控制(FCloudProductPackage 里 uploadFileSize) | 不本地强制,服务端报错如实透传 |
| 风控 | 逆向 Web 协议,理论上有封号/改协议风险 | header 三件套严格对齐;限频退避;别高频轮询 |

## 9. 实施计划

| 阶段 | 内容 | 验收 |
|---|---|---|
| **0 重构** | 抽 trait PanClient/PanError,百度平移,fs.rs 改签名 | 行为零变化,现有挂载冒烟通过 |
| **1 wopan 客户端** | crypto+dispatcher+短信登录+刷新+list/quota/user | crypto 离线单测(对拍 Go 的 crypto_test 向量);`bdfs --backend wopan login/info/ls` 跑通 |
| **2 只读挂载** | get_dlink+read_range 接进 fs.rs 缓存/块缓存/预读 | `cat`/`cp`/`df` 通过,吞吐记录进 README |
| **3 写支持** | mkdir/rename/mv/delete/upload + 暂存复用 + 进度 | 覆盖/改名/删除/截断齐全,md5 校验上传 |
| **4 收尾** | 控制台菜单、systemd、README、实测数据 | 全功能对齐百度后端 |

阶段 0 单独成一个 commit(纯重构可回滚),阶段 1 的 crypto 单测优先写——加密对了后面全是顺水推舟。
