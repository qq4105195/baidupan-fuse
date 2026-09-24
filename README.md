# baidupan-fuse

多网盘 FUSE 挂载工具:把网盘目录树挂载成本地文件系统,支持读 + 写(Linux/macOS 通用,同一份代码)。

当前后端:**百度网盘**、**联通云盘(wopan)**,CLI 加 `--backend baidu|wopan` 切换,
交互控制台用「1. 切换网盘」。

- 百度走 [百度网盘开放平台](https://pan.baidu.com/union/doc/) REST API(基础网盘服务);
  联通走 Web 端(panservice.mail.wo.cn)dispatcher 协议,由
  [wopan-sdk-go](https://github.com/xhofe/wopan-sdk-go) 等逆向实现交叉验证而来
- `fuser` 实现用户态文件系统:Linux 纯 Rust 直连 `/dev/fuse`,无需 libfuse
- 读:目录浏览、`cat`/`cp` 拉取、`df` 查容量
- 写:改动先落本地暂存,close 时整文件传回网盘(百度官方三段式
  precreate → 4MB 分片 superfile2 → create,支持秒传;联通 8MB 分片直传);
  mkdir/改名/删除/截断都支持。要挡写挂载时加 `--read-only`(内核层 EROFS)
- 目录列表 / attr / dlink 三级内存缓存,应对 FUSE 的调用风暴和 API 配额限制

## 准备

- **百度**:到 [pan.baidu.com/union](https://pan.baidu.com/union/) 创建应用,拿到 **AppKey / SecretKey**。
  **重要限制**(见官方[权限与配额](https://pan.baidu.com/union/doc/%E4%BD%BF%E7%94%A8%E5%85%A5%E9%97%A8/%E6%9D%83%E9%99%90%E4%B8%8E%E9%85%8D%E9%A2%9D/)):
  未过审应用**接口频率 10 次/小时**、最多 10 个授权用户,且默认只能访问
  `/apps/<应用名>/` 目录;放开要提交上线审核
- **联通**:只要一个能收短信的手机号,无需申请应用

## 使用

二进制叫 `bdfs`。**新机器最简路径:直接裸跑 `bdfs`(不带子命令)进交互控制台**,
按菜单走完 切换网盘 → 登录 → 账号信息 → 挂载 → 开机自动挂载,参数都有提示和默认值:

```
════════════ bdfs ════════════
当前网盘:百度网盘  凭据:已保存  挂载:已挂载(/mnt/pan)  自启:百度✓ 联通✗
──────────────────────────────
  1. 切换网盘    当前:百度网盘,可在百度网盘/联通云盘之间切换
  2. 登录        填 AppKey/SecretKey,浏览器拿授权码
  3. 账号信息    验证登录、看容量
  4. 挂载        后台挂载,挂上就回来(启动控制台时也会自动挂载)
  5. 卸载
  6. 开机自动挂载 一处管理所有网盘的自启(不必切网盘)
  7. 取消自动挂载 全部网盘关闭并清理服务
  8. 设置        挂载点/根目录/块大小/并发/缓存/只读
  9. 传输进度    挂载进程正在/最近的下载与上传
  0. 退出
```

设置存 `~/.config/baidupan-fuse/settings.json`(总配置:当前网盘 + 各网盘各自的
挂载点/参数;联通默认挂 `/mnt/wopan`)。挂载进程实时把下载/上传进度写
`~/.config/baidupan-fuse/progress-{baidu,wopan}.json`(原子写、150ms 节流),
菜单「9. 传输进度」读它展示。脚本/自动化走子命令,`--backend` 选网盘:

```bash
# 百度登录:打印授权网址 → 浏览器登录百度并同意 → 页面显示授权码 → 粘贴回终端。
# 应用没开通设备码授权时要加 --code-mode(oob 授权码,10 分钟内有效、一次性)
bdfs login --app-key <AK> --app-secret <SK> --code-mode

# 联通登录:手机号收短信验证码,终端输入验证码
bdfs --backend wopan login --phone <手机号>
# 或手动粘 pan.wo.cn 抓包的 token(会没有自动续期,尽量用短信登录)
bdfs --backend wopan login --token <accessToken> --refresh-token <refreshToken>

bdfs info              # 验证 token(账号、会员类型、容量)
bdfs --backend wopan info
bdfs ls /              # 不挂载直接列目录(冒烟)
bdfs --backend wopan ls /

# 挂载(默认读写:写改动 close 时上传;16MB 块 / 单连接 / 缓存 128MB)。
# 参数全可省:省略的从 settings.json 里该网盘的设置读,显式给出的优先;要只读加 --read-only
bdfs mount --daemon          # 后台挂载:挂上就返回,日志 ~/.config/baidupan-fuse/log-baidu.txt
bdfs --backend wopan mount --daemon   # 联通同理;不加 --daemon 则前台跑(看实时日志用)

df -h /mnt/wopan          # 显示网盘容量即成功
cat /mnt/wopan/某文件.txt  # 触发 dlink + HTTP Range 下载
cp 本地文件 /mnt/wopan/    # close 时分片上传(进度也进 progress-wopan.json)
```

凭据都在 `~/.config/baidupan-fuse/` 下按网盘区分:百度 `config.json` + `token.json`,
联通 `wopan.json`。换新机器把对应文件 scp 过去免重新登录;但两台机器长期共用会
互相顶掉 refresh token(联通的 refresh token 每次续期都会轮换,尤其如此),
建议只在一处挂载。token 自动续期:百度 access 30 天 / refresh 10 年;
联通 access 约 7 天,失效时按 RSP_CODE 9999/1001 自动刷新重试一次。

常驻推荐控制台「6. 开机自动挂载」:**一处管理所有网盘**的自启,不必切网盘。
底层是一份 systemd 模板单元 + 按网盘启实例(`bdfs@baidu` / `bdfs@wopan`),
这是 systemd 管理"同一程序多实例"的标准形态(wg-quick@、openvpn-client@ 同款);
单个挂载崩了 systemd 单独拉起,互不影响。老版的 `bdfs.service` / `bdfs-wopan.service`
检测到会自动迁移。实例的 ExecStart 只有 `bdfs --backend <网盘> mount`——挂载参数
**不烤进服务**,启动时从 settings.json 实时读,「8. 设置」改完
`systemctl restart bdfs@<网盘>` 即生效。手写等效:

```ini
# /etc/systemd/system/bdfs@.service(%i = 网盘 id:baidu / wopan)
[Unit]
Description=bdfs FUSE 挂载(%i)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/bdfs --backend %i mount
Environment=RUST_LOG=info
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload && systemctl enable --now bdfs@wopan
```

sudo 安装时还会给实例写个 drop-in(`bdfs@<盘>.service.d/10-home.conf`)把 `HOME`
指回安装者——systemd 服务没有 `HOME` 环境变量(rclone 官方文档明说的坑),
不指回去开机时进程读不到 `~/.config` 里的凭据。

卸载:`bdfs` 裸跑选 5(只杀当前网盘的挂载进程),或 `fusermount -u /mnt/wopan`
(进程退出时 AutoUnmount 也会自动摘掉挂载点)。覆盖升级二进制前先 `pkill -x bdfs`
(**别用 `pkill -f`**,会误杀带同样参数的 ssh 会话)。容器里挂载需要
`--device /dev/fuse --cap-add SYS_ADMIN`。

日志:`RUST_LOG=debug bdfs mount ...` 看每次 API 拉取,排查限频很有用。

## 编译

产物是静态二进制 `target/release/bdfs`(musl,目标机器零依赖)。

- Linux(推荐 musl 静态产物,部署无需依赖):
  ```bash
  cargo build --release --target aarch64-unknown-linux-musl   # 或 x86_64
  ```
- macOS:需要先装 [macFUSE](https://github.com/macos-fuse-t/macfuse)(kext)或 [FUSE-T](https://www.fuse-t.org/)(免 kext)
- 想用 Docker 一把梭(开发机无 Rust 环境时):
  ```bash
  docker run --rm -v $PWD:/work -v baidupan-cargo-cache:/usr/local/cargo/registry \
    -w /work rust:alpine cargo build --release
  # 产物 target/release/bdfs(x86_64-linux-musl),scp 到目标机即可
  ```

## 设计

```
src/
├─ pan/           多网盘抽象层
│  ├─ mod.rs      PanClient trait(list_dir/get_dlink/read_range/upload/mkdir/
│  │              delete/mv/quota/account)+ NetFile/PanError/PanKind 统一模型;
│  │              各网盘凭据/暂存/进度文件都在 ~/.config/baidupan-fuse/ 下按名字区分
│  ├─ baidu.rs    百度客户端:OAuth 设备码登录/token 刷新、list、filemetas(dlink)、
│  │              Range 下载、quota;三段式上传(precreate/superfile2/create,秒传)、
│  │              mkdir/filemanager(delete/move);errno → PanError
│  └─ wopan/      联通客户端(逆向 Web 协议)
│     ├─ crypto.rs  AES-128-CBC + MD5 签名(密钥/IV 是逆向的固定值,有 openssl 对拍测试)
│     ├─ api.rs     dispatcher 信封构造/解析、时间/随机、文件类型码
│     └─ mod.rs     WopanClient:短信登录、token 轮换刷新、path→目录id 自愈映射、
│                  分页列表、GetDownloadUrlV2、8MB 分片直传、mkdir/move+rename/quota
├─ fs.rs         PanFs:fuser 的 Filesystem trait 实现,后端无关。ino↔path 双向表、
│                目录/dlink TTL 缓存、块缓存+预读梯度;
│                写路径 = WriteSession 本地暂存 + close 时整文件上传
│                (写洞回填旧数据、覆盖/改名/删除/截断)
├─ settings.rs   总设置(~/.config/baidupan-fuse/settings.json):当前网盘 + 各网盘
│                各自的挂载参数;兼容读取老版单网盘扁平格式
├─ progress.rs   传输进度:挂载进程实时写 progress-<网盘>.json(原子写+节流),
│                下载分块和上传分片都走这套,控制台「9. 传输进度」读同一个文件
├─ menu.rs       交互控制台:裸跑进入。启动时有凭据未挂载会自动后台挂载;
│                切换网盘/登录/信息/挂载/卸载/设置/进度,各项作用于当前网盘;
│                「开机自动挂载」一处管理所有网盘(systemd 模板 bdfs@.service + 实例,
│                实例启动时实时读设置);卸载靠扫 /proc(比 pkill -f 安全)
├─ daemon.rs     后台挂载:fork + setsid 出守护进程跑 mount2,父进程等挂载点
│                出现即返回(菜单 4 / 启动自动挂载 / mount --daemon 三处共用),
│                日志追加写 config 目录 log-<网盘>.txt
└─ main.rs       clap CLI:全局 --backend 选网盘;裸跑 → 控制台;
                 子命令 login / info / ls / mount
```

关键取舍:

- **路径协议 ↔ inode 协议**:网盘 API 是路径/各自 id,内核全是 inode,`fs.rs` 维护
  ino↔path 双向映射,inode 单调递增不复用。后端内部差异(百度按路径、联通按目录 id
  + 双 id 条目/内容标识)全部封进 `pan/` 的实现里,trait 表面只有路径 + 字符串 id
- **缓存是生命线**:内核一次 `ls -l` 触发几十个 lookup/getattr,不打缓存百度
  10 次/小时的配额秒光。目录列表默认缓存 60s,dlink 百度官方 8h 有效默认缓存
  30 分钟,联通时效更短默认 10 分钟
- **单线程串行**:mount2 默认单线程分发,天然限流(代价是大目录 readdir 阻塞其他操作,v1 接受)
- **读即 HTTP Range**:内核单次 read 上限 128KB,直接打 API 每次一个 HTTP 往返
  只有 ~160KB/s → 块缓存(16MB)+ 预读梯度,顺序读吞吐 ≈ CDN 单流速度
- **token 自愈**:快过期主动刷新;百度接口回 errno -6/111、联回 RSP_CODE
  9999/1001 时刷新重试一次;联通 refresh token 每次续期轮换,新值立即落盘
- **写 = 暂存 + close 时上传**:FUSE 的 write 是零散小包,直接打 API 不现实;
  改动全落本地暂存文件(config 目录 uploads-<网盘>/),close(flush)时整文件上传
  (百度分片 md5 三段式 + 秒传,联通 8MB 分片直传)。没有本地写缓存上限约束
  (暂存=文件大小),大文件会先占等量磁盘

## 已知边界(v1)

- 目录列表整目录翻页拉全,超大目录(万级文件)会连续多次 API 调用
- `find`(getattr/read 找元数据)依赖父目录缓存,远端文件在缓存期内被删仍会显示
- 错误映射只特判了 -9(ENOENT)/-7(EACCES),其余一律 EIO
- **顺序读吞吐 ~4-7MB/s**(SVIP 账号,随 CDN 节点波动;单流 curl 峰值见过 10-15MB/s):
  读策略三级——首读/seek 后直接拉请求窗口(秒回,不整块拉);连续顺序读升级成
  16MB 块缓存;确认流式(≥2MB)后放开预读 2 块。块大小 `--block-mb` 越大吞吐越高
  (实测 8MB≈4MB/s、16MB≈7MB/s、32MB 持平,连接爬坡摊薄)
- **并发是减速带**:实测"单长流"才是 SVIP 的高速通道,8 连接并发分块反而从
  7MB/s 掉到 2.3MB/s(CDN 对短促并发波浪限速)。`--parallel` 默认 1,留给实验
- CDN 实测怪癖(代码已处理):
  - **并发波浪会 403,顺序复用同一 dlink 没问题** → dlink 缓存 30 分钟,
    403 时弃缓存换新链自动重试
  - 下载请求不复用连接(`Connection: close`,同一 keep-alive 连接发第二个
    Range 会被 403)
  - 块数据短了会被内核当 EOF(文件看似截断)→ 拉取后校验长度,不等长报错重试
- API 实测与文档的差异(代码已兼容):filemetas 的返回数组字段名是 `info` 不是
  `list`;容量接口用 `/api/quota`(`xpan/nas?method=quota` 报 Param error);
  部分应用未开通设备码授权,`login --code-mode` 走 oob 授权码模式;
  **superfile2 只在 `d.pcs.baidu.com` 上有,`pan.baidu.com` 上是 404 页**;
  **空文件的 block_list 必须填 `[空串md5]`,填 `[]` 会报 errno=2**
- 写路径的边界:v1 是"整文件上传"不是增量——flush 时哪怕只改 1 字节也会
  把整个文件重传(百度 md5 对不上的分片才会真传,靠 precreate 的秒传/缺片机制
  省流量;联通无秒传,总是全量分片);上传失败的暂存文件保留在 uploads-<网盘>/
  供手动抢救;正在写的文件在 readdir 里要等上传完成才出现(lookup 能看到)
- 联通特有的边界(协议逆向所得,详见 docs/wopan-integration.md):
  - 上传后列表可见性有秒级延迟(实测最长 ~30s),上传完会等列表出现新条目,
    等不到不报错(条目 id 下次列目录自愈)
  - 下载直链对 Referer 敏感:空或 pan.wo.cn 放行,第三方拒绝;403 时弃链重取
  - 列表条目 type 出现过 "7"/"9" 等未知值,按"跳过 + 日志"处理
  - 短信登录用的 client(1001000035)和文件操作(1001000021)是两对凭据,别混用

## 路线

1. ✅ 只读挂载
2. ✅ 块级读缓存 + 预读梯度(160KB/s → 4-7MB/s,单流策略对齐 SVIP 高速通道)
3. ✅ 写支持:本地暂存 + close 时整文件上传(百度 precreate → superfile2 → create,
   覆盖/改名/删除/截断齐全,上传进度复用 progress 文件)
4. ✅ 多网盘:PanClient 抽象 + 联通云盘(短信登录/分片直传/容量账号)
5. ⬜ macOS 打包验证(macFUSE / FUSE-T)
6. ⬜ 多网盘规模化(调研见 docs/multi-drive-research.md):PanClient 拆成
   核心 + 可选能力 trait(只读盘只实现 List/Link,学 AList 的最小驱动);
   配置注册表化(加盘不改 Config 结构);combine 式聚合挂载
   `bdfs mount-all`(一个守护进程一棵树,每盘一个顶层子目录)
