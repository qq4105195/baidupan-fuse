# baidupan-fuse

百度网盘挂载工具,同一份代码两种形态:
**Linux/macOS** 是 FUSE 挂载(读写),**Windows** 是 OneDrive 式按需文件夹
(Cloud Files API:云图标占位、打开即下载、右键"始终保留在此设备"/"释放空间")。

- 走 [百度网盘开放平台](https://pan.baidu.com/union/doc/) REST API(基础网盘服务)
- Linux/macOS:`fuser` 实现用户态文件系统,Linux 纯 Rust 直连 `/dev/fuse`,无需 libfuse
- 读:目录浏览、`cat`/`cp` 拉取、`df` 查容量
- 写:改动先落本地暂存,close 时按官方三段式(precreate → 4MB 分片 superfile2 → create)
  传回网盘;mkdir/改名/删除/截断都支持。要挡写挂载时加 `--read-only`(内核层 EROFS)
- Windows:Explorer 侧边栏出现「百度网盘」,文件默认是云图标占位符,双击按需下载,
  本地增删改改名自动回传网盘(防抖 + 失败退避重试)
- 目录列表 / attr / dlink 三级内存缓存,应对调用风暴和 API 配额限制
- 可部署到 Android 设备(实测中兴 F50 Pro 随身路由,root):网盘直接变成局域网
  SMB 共享,教程见 [docs/deploy-f50pro.md](docs/deploy-f50pro.md)

## 准备

1. 到 [pan.baidu.com/union](https://pan.baidu.com/union/) 创建应用,拿到 **AppKey / SecretKey**
2. **重要限制**(见官方[权限与配额](https://pan.baidu.com/union/doc/%E4%BD%BF%E7%94%A8%E5%85%A5%E9%97%A8/%E6%9D%83%E9%99%90%E4%B8%8E%E9%85%8D%E9%A2%9D/)):
   - 未通过上线审核的应用:**接口频率 10 次/小时**、最多 10 个授权用户
   - 默认只能访问 `/apps/<应用名>/` 目录(网盘网页里显示为「我的应用数据」下)
   - 要挂载整个网盘、放开频率,需要提交应用上线审核

## 使用

二进制叫 `bdfs`。**新机器最简路径:直接裸跑 `bdfs`(不带子命令)进交互控制台**,
按菜单走完 登录授权 → 账号信息 → 挂载 → 开机自动挂载,参数都有提示和默认值:

```
════════════ bdfs ════════════
凭据:已保存  挂载:已挂载(/mnt/pan)  自启:已启用
──────────────────────────────
  1. 登录授权    填 AppKey/SecretKey,浏览器拿授权码
  2. 账号信息    验证登录、看容量
  3. 挂载        前台运行,Ctrl-C 退出并自动卸载
  4. 卸载
  5. 开机自动挂载(安装 systemd 服务)
  6. 取消自动挂载
  7. 设置        挂载点/根目录/块大小/并发/缓存/只读
  8. 传输进度    挂载进程正在/最近的下载与上传
  0. 退出
```

菜单里的设置存 `~/.config/baidupan-fuse/settings.json`,挂载和生成的 systemd
服务都用它。挂载进程实时把下载/上传进度写 `~/.config/baidupan-fuse/progress.json`
(原子写、150ms 节流),菜单「8. 传输进度」读它展示;进度停更 5 秒会提示
"挂载可能没在跑"。脚本/自动化走子命令:

```bash
# 登录:打印授权网址 → 浏览器登录百度并同意 → 页面显示授权码 → 粘贴回终端。
# 应用没开通设备码授权时要加 --code-mode(oob 授权码,10 分钟内有效、一次性)
bdfs login --app-key <AK> --app-secret <SK> --code-mode

bdfs info    # 验证 token(账号、会员类型、容量)
bdfs ls /    # 不挂载直接列目录(冒烟)

# 挂载(默认读写:写改动 close 时上传;16MB 块 / 单连接 / 缓存 128MB)
# 要只读加 --read-only
mkdir -p /mnt/pan
setsid nohup env RUST_LOG=info bdfs mount /mnt/pan -r / \
  >/var/log/panfuse.log 2>&1 < /dev/null &

df -h /mnt/pan            # 显示网盘容量即成功
cat /mnt/pan/某文件.txt    # 触发 dlink + HTTP Range 下载
cp 本地文件 /mnt/pan/      # close 时三段式上传(上传进度也进 progress.json)
```

凭据在 `~/.config/baidupan-fuse/`(config.json + token.json)。换新机器可以直接把这两个文件
scp 过去免重新授权;但两台机器长期共用会互相顶掉 refresh token,建议只在一处挂载。
token 自动续期(access 30 天,refresh 10 年),重启进程无感。

常驻推荐控制台「5. 开机自动挂载」一键装 systemd 服务(用当前设置生成
`/etc/systemd/system/bdfs.service`);手写等效:

```ini
[Unit]
Description=Baidu Netdisk FUSE mount
After=network-online.target

[Service]
ExecStart=/usr/local/bin/bdfs mount /mnt/pan -r /
Environment=RUST_LOG=info
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload && systemctl enable --now bdfs
```

卸载:`bdfs` 裸跑选 4,或 `fusermount -u /mnt/pan`(进程退出时 AutoUnmount
也会自动摘掉挂载点)。覆盖升级二进制前先 `pkill -x bdfs`(**别用 `pkill -f`**,
会误杀带同样参数的 ssh 会话)。容器里挂载需要 `--device /dev/fuse --cap-add SYS_ADMIN`。

日志:`RUST_LOG=debug bdfs mount ...` 看每次 API 拉取,排查限频很有用。

## Windows:OneDrive 式按需文件夹

Windows 不走 FUSE,用系统的 **Cloud Files API(cldapi)**——和 OneDrive 同一套机制。
要求:Windows 10 1709+、同步根在 NTFS 盘、**无需管理员**(同步根注册进当前用户)。

新机器同样是裸跑 `bdfs` 进控制台,菜单按平台自动切换:

```
════════════ bdfs ════════════
凭据:已保存  同步:已注册(C:\Users\you\BaiduNetdisk)  自启:未配置
──────────────────────────────
  1. 登录授权    填 AppKey/SecretKey,浏览器拿授权码
  2. 账号信息    验证登录、看容量
  3. 同步        启动按需文件夹,前台运行,Ctrl-C 退出
  4. 停止同步
  5. 开机自动同步(注册表 Run 键)
  6. 取消自动同步
  7. 设置        同步根/远端根目录/缓存
  8. 传输进度    挂载/同步进程正在/最近的下载与上传
  0. 退出
```

「3. 同步」= `bdfs sync`(可带参数指定同步根):注册同步根(默认
`%USERPROFILE%\BaiduNetdisk`,Explorer 侧边栏出现「百度网盘」)后前台常驻。
之后:

- 文件夹里的文件都是**云图标占位符**(不占磁盘),双击 = 整文件下载后打开,
  进度显示在资源管理器弹窗;`bdfs` 控制台「8. 传输进度」也能看
- 右键 **始终保留在此设备** = 后台全量下载(绿勾);右键 **释放空间** =
  删本地内容回云图标(没回传完的改动会被拒绝,防止丢数据)
- 本地新建/修改/改名/删除自动回传网盘(改动后 ~2s 防抖起传;失败按
  30s→1m→10m→30m→1h 退避重试,重启进程会启动脏扫补传)
- 单实例保护:第二个 `bdfs sync` 直接提示退出;菜单「4. 停止同步」或 Ctrl-C
  优雅断开(**不注销**:进程不在时占位符仍可见,只是打不开)
- 「5. 开机自动同步」写 HKCU Run 键(指向当前 exe,把 exe 挪位置前先取消)

凭据在 `%APPDATA%\baidupan-fuse\`(和 Linux 的 `~/.config/baidupan-fuse/` 同构,
config.json + token.json 可以直接拷贝过去免重新授权)。

**Windows 专属边界**:

- **配额比 Linux 更敏感**:Explorer 浏览目录时元数据查询是连片的,目录列表缓存
  TTL 默认 **300s**(Linux 是 60s),别调小
- 已展开过的目录**不会自动刷新**(cldapi 枚举一次性语义):在别的设备上传到网盘的
  新文件,v1 里要重启同步进程才出现在已浏览过的目录里(没浏览过的目录浏览即拉新)
- 远端在别处被删/改名,v1 不回推本地(「本地为主」语义;双向对比回推列在路线里)
- `bdfs sync` 进程不在跑时:占位符可见、不能打开/水合;本地已全量下载的文件照常可用
- 同步根不能是磁盘根目录/系统目录;杀软实时扫描可能触发隐式下载(代价认了,
  拦它会把"始终保留在此设备"也误伤)
- 崩溃残留的注册可用菜单外命令清干净:`bdfs unregister`(本地文件原样保留)

## 编译

产物是静态二进制 `target/release/bdfs`(musl,目标机器零依赖)。

- Linux(推荐 musl 静态产物,部署无需依赖):
  ```bash
  cargo build --release --target aarch64-unknown-linux-musl   # 或 x86_64
  ```
- Windows:MSVC 工具链直接 `cargo build --release`(cldapi 绑定是纯 FFI,
  无系统 SDK 额外要求);产物 `target\release\bdfs.exe`。aarch64 Linux 交叉
  编译见 [docs/deploy-f50pro.md](docs/deploy-f50pro.md)(zigbuild)
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
├─ baidu.rs      API 客户端:OAuth 设备码登录/token 刷新、list、filemetas(dlink)、
│                Range 下载、quota;三段式上传(precreate/superfile2/create)、
│                mkdir/filemanager(delete/move);errno → ApiError,fs 层 downcast 决定内核 errno
├─ core.rs       平台无关内核(自 fs.rs 抽取):SharedClient 锁策略、Dir/DLink TTL 缓存、
│                Range 拉取重试、三段式上传编排、本地↔远端路径映射;FUSE 和 cldapi 共用
├─ fs.rs         PanFs:fuser 的 Filesystem trait 实现(unix)。ino↔path 双向表、
│                目录/dlink TTL 缓存、块缓存+预读梯度;
│                写路径 = WriteSession 本地暂存 + close 时三段式上传
│                (写洞回填旧数据、覆盖/改名/删除/截断、秒传)
├─ settings.rs   控制台的持久化设置(unix ~/.config、Windows %APPDATA%),挂载/同步/
│                systemd 服务/注册表自启都以它为准
├─ progress.rs   传输进度:常驻进程实时写 progress.json(原子写+节流),
│                下载分块和上传分片都走这套,控制台「8. 传输进度」读同一个文件
├─ menu/         交互控制台:裸跑进入。共享骨架(登录/信息/设置/进度)+ 按平台菜单项
│                (unix:挂载/卸载/systemd 自启;windows:同步/停止/注册表自启/注销)
├─ win/          Windows 形态(Cloud Files API,全部 cfg(windows)):
│                mod.rs 同步根注册/生命周期/单实例互斥/停止事件;identity.rs 占位符
│                blob 编码(fs_id/size/mtime);provider.rs SyncFilter 回调
│                (枚举/删除/改名/脱水);hydrate.rs 按需下载管线(并发限 3+取消);
│                syncback.rs 本地变更回传(watcher+防抖队列+退避重试+自触抑制)
└─ main.rs       clap CLI:裸跑 → 控制台;子命令 login / info / ls / mount(unix)、
                 sync / unregister(windows)
```

关键取舍:

- **路径协议 ↔ inode 协议**:百度 API 全是路径,内核全是 inode,`fs.rs` 维护双向映射,inode 单调递增不复用
- **缓存是生命线**:内核一次 `ls -l` 触发几十个 lookup/getattr,不打缓存 10 次/小时的配额秒光。目录列表默认缓存 60s(`--dir-tl` 调),dlink 官方 8h 有效默认缓存 30 分钟
- **单线程串行**:mount2 默认单线程分发,天然限流(代价是大目录 readdir 阻塞其他操作,v1 接受)
- **读即 HTTP Range**:内核单次 read 上限 128KB,直接打 API 每次一个 HTTP 往返
  只有 ~160KB/s → 块缓存(16MB)+ 预读梯度,顺序读吞吐 ≈ CDN 单流速度
- **token 自愈**:expires_at 前 2 分钟自动刷新;接口回 errno -6/111 时刷新重试一次
- **写 = 暂存 + close 时上传**:FUSE 的 write 是零散小包,直接打 API 不现实;
  改动全落本地暂存文件(config 目录 uploads/),close(flush)时算分片 md5 走
  三段式。没有本地写缓存上限约束(暂存=文件大小),大文件会先占等量磁盘

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
  把整个文件重传(md5 对不上的分片才会真传,靠 precreate 的秒传/缺片机制省流量);
  上传失败的暂存文件保留在 uploads/ 供手动抢救;正在写的文件在 readdir 里
  要等上传完成才出现(lookup 能看到)

## 路线

1. ✅ 只读挂载
2. ✅ 块级读缓存 + 预读梯度(160KB/s → 4-7MB/s,单流策略对齐 SVIP 高速通道)
3. ✅ 写支持:本地暂存 + close 时 precreate → superfile2(4MB 分片)→ create,
   覆盖/改名/删除/截断齐全,上传进度复用 progress.json(60MB 实测 md5 一致)
4. ✅ Windows 按需文件夹(Cloud Files API):云图标占位/双击水合(断点续传)/
   pin-unpin/本地增删改改名回传/脏扫兜底;FUSE 路径不受影响
5. ⬜ 远端改动回推(双向 fs_id/mtime 对比;Windows 已展开目录的重新枚举)
6. ⬜ macOS 打包验证(macFUSE / FUSE-T)
