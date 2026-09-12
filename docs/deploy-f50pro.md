# F50 Pro 部署教程:百度网盘挂进随身路由的 Samba 共享

目标设备:中兴 F50 Pro 5G 随身路由(Android 15,已 root)。
效果:整机变成"网盘路由器"——局域网任何设备直接访问
`\\192.168.0.1\F50 Pro\百度网盘`(Windows)/ `smb://192.168.0.1`(macOS/Linux),
读写网盘文件,无需装任何客户端。

```
Windows/手机 ──SMB──> F50 Pro(厂商 Samba)──> /data/SAMBA_SHARE/百度网盘(FUSE)
                                                    │
                                              bdfs(百度开放平台 API)
                                                    │
                                              百度网盘(全盘/指定目录)
```

## 前提条件

- F50 Pro 已 root(Magisk 精简版即可,本教程**不依赖** service.d)
- SSH 可用:`ssh root@192.168.0.1`(免密最方便,公钥放进 `/data/ssh/.../authorized_keys`)
- 百度网盘开放平台应用:
  - **已过审** → 可挂全盘 `-r /`,接口频率不受限(推荐)
  - **未过审** → 只能访问 `/apps/<应用名>/`,接口 10 次/小时(列目录有缓存兜底,勉强能用)
- 一台能跑 WSL/Docker 的电脑(编译用;F50 Pro 本机没有 Rust 环境)

## 一、编译 aarch64-musl 静态二进制

**为什么是 musl 静态**:Android 用户态是 bionic,glibc 动态链接的二进制跑不了;
静态 musl 零依赖,且 musl 的 DNS 解析在缺 `/etc/resolv.conf` 时回落 127.0.0.1——
F50 Pro 上正好有 dnsmasq 监听 127.0.0.1:53,**DNS 不用任何处理**。

**为什么在 WSL/Linux 里编**:fuser 的 build.rs 判定的是**宿主** OS,
Windows/macOS 宿主上交叉到 Linux 会直接 panic
(`Building without libfuse is only supported on Linux`)。
WSL(或任意 Linux)里编 aarch64 目标即可绕开。

WSL Ubuntu 内一键脚本(rustup/crates 走 rsproxy 镜像,zig 从 ziglang.org 直连):

```bash
#!/usr/bin/env bash
set -ex
export RUSTUP_DIST_SERVER="https://rsproxy.cn" RUSTUP_UPDATE_ROOT="https://rsproxy.cn/rustup"
[ -x ~/.cargo/bin/cargo ] || curl -sSf https://rsproxy.cn/rustup-init.sh \
  | sh -s -- -y --default-toolchain stable --profile minimal --no-modify-path
. ~/.cargo/env
rustup target add aarch64-unknown-linux-musl
cat > ~/.cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = 'rsproxy-sparse'
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
EOF
# zig:注意官方命名是 架构在前(zig-x86_64-linux-...),国内镜像大多没同步 zig
[ -x ~/zig/zig ] || { mkdir -p ~/zig \
  && curl -fL -o /tmp/zig.tar.xz \
    https://ziglang.org/download/0.14.1/zig-x86_64-linux-0.14.1.tar.xz \
  && tar -xJf /tmp/zig.tar.xz -C ~/zig --strip-components=1; }
export PATH="$HOME/zig:$PATH"
cargo install cargo-zigbuild --locked
cd /path/to/baidupan-fuse
cargo zigbuild --release --target aarch64-unknown-linux-musl
file target/aarch64-unknown-linux-musl/release/bdfs
# 应显示: ELF 64-bit LSB executable, ARM aarch64, ... statically linked
```

> Windows 本机直连 rust-lang.org/ziglang.org 常被掐(TLS handshake eof)。
> 两条出路:走 rsproxy/ziglang(WSL 直连大多可达),或走 F50 Pro 上 Clash 的
> 局域网代理 `http://192.168.0.1:7890`(`https_proxy=... rustup ...`)。

## 二、部署到设备

```bash
ssh root@192.168.0.1 "mkdir -p /data/bdfs"
scp target/aarch64-unknown-linux-musl/release/bdfs root@192.168.0.1:/data/bdfs/
ssh root@192.168.0.1 "chmod +x /data/bdfs/bdfs && /data/bdfs/bdfs --version"
```

**关键环境变量**:bdfs 的配置/token/上传暂存都在 `$HOME/.config/baidupan-fuse/`
(`baidu.rs: config_dir()`)。Android 的 root shell 没有可靠的 HOME,
后续所有命令都带 `HOME=/data/bdfs`,统一放 `/data/bdfs/.config/baidupan-fuse/`。

## 三、登录授权

```bash
ssh root@192.168.0.1
# 设备码模式(自动轮询,浏览器确认后 token 自动落盘):
HOME=/data/bdfs /data/bdfs/bdfs login -k <AppKey> -s <SecretKey>
# 应用没开设备码授权就加 --code-mode(oob 授权码,按提示粘贴)
```

验证(同时验证 DNS/TLS/授权三件事):

```bash
HOME=/data/bdfs /data/bdfs/bdfs info   # 账号、会员类型、容量
HOME=/data/bdfs /data/bdfs/bdfs ls /   # 列根目录
```

## 四、手动挂载验证

F50 Pro 的厂商 Samba 有个现成共享 `[F50 Pro]` 指向 `/data/SAMBA_SHARE`,
把 FUSE 挂载点放进它的子目录,**不改 smb.conf**(App 开机会重建并
`chattr +i` 锁定 smb.conf,改了也白改):

```bash
HOME=/data/bdfs setsid nohup /data/bdfs/bdfs mount \
  /data/SAMBA_SHARE/百度网盘 -r / --allow-other \
  >/data/bdfs/mount.log 2>&1 &

df -h /data/SAMBA_SHARE/百度网盘   # 显示网盘容量即成功
ls /data/SAMBA_SHARE/百度网盘
```

- `--allow-other`:让 smbd 的子进程也能穿透挂载点。root 挂载无需
  `/etc/fuse.conf` 放开 user_allow_other。
- 写模式:默认读写(写改动 close 时整文件三段式上传);要只读加 `--read-only`。

Windows 侧:`\\192.168.0.1\F50 Pro\百度网盘` 直接打开。

## 五、开机自启(F50 Pro 专属姿势)

**坑:F50 Pro 的 magiskd 是精简版,不执行 `/data/adb/service.d/` 脚本。**
正确姿势是挂 `com.minikano.f50_sms` App 的两个钩子(设备上 sshd、Clash
就是这么自启的):

| 钩子 | 触发时机 | 用途 |
|---|---|---|
| `/sdcard/ufi_tools_boot.sh` | 开机(uptime<120s 窗口) | 首次拉起 |
| `/sdcard/ufi_tools_schedule.sh` | 每次 SMB 连接 | 看门狗,死了自动复活 |

两个钩子都调用同一个守护脚本(带 `pgrep` 防重复起两份)。

**1. 守护脚本** `/data/adb/service.d/99bdfs.sh`(放 service.d 只是图路径稳定,
实际由钩子调用;挂载不需要外网,FUSE 起来后首次访问才打 API,所以不等待网络):

```sh
#!/system/bin/sh
# bdfs:百度网盘 FUSE 挂载守护(开机/巡检时调用;已在跑则跳过)
export HOME=/data/bdfs
MP=/data/SAMBA_SHARE/百度网盘
if ! pgrep -x bdfs >/dev/null 2>&1; then
  # 上次进程被杀可能留下挂不上的残留挂载点,先摘掉
  umount "$MP" 2>/dev/null
  setsid nohup /data/bdfs/bdfs mount "$MP" -r / --allow-other \
    >>/data/bdfs/mount.log 2>&1 &
fi
```

**2. 追加到两个钩子**(已存在则追加,别覆盖别人的内容):

```bash
ssh root@192.168.0.1
grep -q 99bdfs /sdcard/ufi_tools_boot.sh || \
  printf '%s\n' '# bdfs 百度网盘挂载(开机)' \
  '/data/adb/service.d/99bdfs.sh' >> /sdcard/ufi_tools_boot.sh
grep -q 99bdfs /sdcard/ufi_tools_schedule.sh || \
  printf '%s\n' '# bdfs 百度网盘挂载(看门狗:进程死了自动拉起)' \
  '/data/adb/service.d/99bdfs.sh' >> /sdcard/ufi_tools_schedule.sh
```

**3. 重启验证**:

```bash
ssh root@192.168.0.1 reboot
# 等 2~3 分钟后:
ssh root@192.168.0.1 "pgrep -x bdfs && df -h /data/SAMBA_SHARE/百度网盘 | tail -1"
```

> 经验:开机后第一次打开共享若显示空,等两秒刷新——SMB 连接本身就会触发
> schedule 钩子把挂载拉起来,这正是看门狗兜底的意义。

## 升级 / 卸载

```bash
# 升级二进制(挂载点会闪断一下,smaba 无感)
ssh root@192.168.0.1 "pkill -x bdfs; sleep 1; umount /data/SAMBA_SHARE/百度网盘 2>/dev/null"
scp target/aarch64-unknown-linux-musl/release/bdfs root@192.168.0.1:/data/bdfs/
ssh root@192.168.0.1 "sh /data/adb/service.d/99bdfs.sh"   # 立即拉起

# 卸载:删钩子里的两行 + pkill + umount + rm -rf /data/bdfs
```

## 踩坑记录(按踩中顺序)

1. **Windows 宿主交叉编译直接 panic**:fuser build.rs 判宿主 OS → 在
   WSL/Linux 宿主编 aarch64 目标。
2. **rust-lang.org / ziglang.org 直连被掐**:rustup 走 rsproxy.cn;zig 走
   WSL 直连 ziglang.org(可达);都不行走设备 Clash 代理
   `http://192.168.0.1:7890`。
3. **挂载报 `No such file or directory`**:fuser 的 `AutoUnmount` 强制走
   `fusermount3` 外部二进制,Android 没有 → 已修(have_fusermount() 探测,
   没有就跳过 AutoUnmount 走 root 直连 mount(2),见 `src/fs.rs`)。
4. **service.d 不执行**:精简版 magiskd 不跑开机脚本 → 挂 f50_sms App 的
   `ufi_tools_*.sh` 钩子。
5. **smb.conf 别改**:App 开机 `rm` 后重建并 `chattr +i`;要加共享目录,
   放进 `[F50 Pro]` 的 `/data/SAMBA_SHARE` 下即可。
6. **DNS 免处理**:musl 静态二进制缺 resolv.conf 时回落 127.0.0.1,
   命中设备 dnsmasq。

## 性能与安全

- 顺序读约 4-7 MB/s(SVIP 单流;CDN 波动),SMB 不额外放大瓶颈
- 写 = 整文件重传 + 等量本地暂存(F50 Pro 的 /data 有 ~50G,够用)
- **厂商 Samba 是免密码 guest 可写**:挂读写模式 = 全局域网可写你的网盘,
  仅在可信内网使用;介意就在 smb.conf 重建后立刻改权限(会被下次开机还原,
  需要配合 unlock 脚本,本教程不展开)
