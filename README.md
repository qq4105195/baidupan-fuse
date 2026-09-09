# baidupan-fuse

百度网盘 FUSE 挂载工具:把网盘目录树挂载成本地文件系统,支持读 + 写(Linux/macOS 通用,同一份代码)。

- 走 [百度网盘开放平台](https://pan.baidu.com/union/doc/) REST API(基础网盘服务)
- `fuser` 实现用户态文件系统:Linux 纯 Rust 直连 `/dev/fuse`,无需 libfuse
- 读:目录浏览、`cat`/`cp` 拉取、`df` 查容量
- 写:改动先落本地暂存,close 时按官方三段式(precreate → 4MB 分片 superfile2 → create)
  传回网盘;mkdir/改名/删除/截断都支持。要挡写挂载时加 `--read-only`(内核层 EROFS)
- 目录列表 / attr / dlink 三级内存缓存,应对 FUSE 的调用风暴和 API 配额限制

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
├─ baidu.rs      API 客户端:OAuth 设备码登录/token 刷新、list、filemetas(dlink)、
│                Range 下载、quota;三段式上传(precreate/superfile2/create)、
│                mkdir/filemanager(delete/move);errno → ApiError,fs 层 downcast 决定内核 errno
├─ fs.rs         PanFs:fuser 的 Filesystem trait 实现。ino↔path 双向表、
│                目录/dlink TTL 缓存、块缓存+预读梯度;
│                写路径 = WriteSession 本地暂存 + close 时三段式上传
│                (写洞回填旧数据、覆盖/改名/删除/截断、秒传)
├─ settings.rs   控制台的持久化设置(~/.config/baidupan-fuse/settings.json),
│                挂载和 systemd 服务生成都以它为准
├─ progress.rs   传输进度:挂载进程实时写 progress.json(原子写+节流),
│                下载分块和上传分片都走这套,控制台「8. 传输进度」读同一个文件
├─ menu.rs       交互控制台:裸跑进入。登录/信息/挂载/卸载/自启/设置/进度,
│                卸载靠扫 /proc 找挂载进程(比 pkill -f 安全)
└─ main.rs       clap CLI:裸跑 → 控制台;子命令 login / info / ls / mount
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
4. ⬜ macOS 打包验证(macFUSE / FUSE-T)
