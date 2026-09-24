# 多网盘架构调研:成熟产品怎么管理"很多个网盘"

> 调研时间 2026-09。背景:bdfs 已支持百度网盘 + 联通云盘(每盘独立挂载进程、
> 独立凭据/设置、systemd 模板实例 `bdfs@<盘>` 自启),接下来要接更多网盘
> (夸克/123pan/阿里云盘/OneDrive…)。本文回答:**网盘多了之后,挂载/配置/自启/
> 菜单该怎么架构**,别人踩出来的路线是什么。

调研对象:rclone(及其生态的 systemd 用法)、AList/OpenList、CloudDrive2、RaiDrive,
以及 Air Explorer/Air Live Drive、ExpanDrive、Mountain Duck 等对照。
所有结论附来源 URL;推断部分明确标注。

## 0. 一句话结论

多网盘场景下,成熟产品几乎全部选 **B:单一守护进程管所有网盘**,差别只在对外呈现——
AList 是"一个 Web 服务 + 虚拟目录树,FUSE 外包给 rclone/davfs2";CloudDrive2/RaiDrive
是"一个核心服务 + 自带挂载层,每盘一个卷(或聚合成一棵树)"。
rclone 是唯一的 **A:每个 remote 一个挂载进程** 的活标本,靠 systemd 模板单元管多实例。
两条路线我们**短期用 A(已落地的模板实例),中期补一个 B 形态的聚合挂载**,
详见 §6。

## 1. AList / OpenList:单守护进程 + 虚拟路径,FUSE 整个外包

**形态**:单进程 Go 服务(Gin + SolidJS),SQLite 默认存储,约 90 个网盘驱动
**静态编译**进一个二进制;每个存储配一个"挂载路径"(如 `/quark`、`/baidu`),
拼成一棵虚拟目录树;对外只有 Web UI + WebDAV,**自己完全不做 FUSE**——
本地文件系统体验靠 rclone/davfs2/RaiDrive 挂它的 WebDAV(官方文档直接列出这些工具)。

关键设计(都值得抄):

- **驱动注册是编译期的**:每个驱动包 `init()` 里 `RegisterDriver`,在 `drivers/all.go`
  blank import 一行即接入;官方说法:"复制 drivers/template 目录,新建包 + all.go 加
  一行 import,不用改任何其他文件"。这就是 90 个驱动的由来。
- **虚拟路径模型**:挂载路径唯一(重复报 `UNIQUE constraint failed: x_storages.mount_path`),
  但**可嵌套**——`getStoragesByPath` 按最长前缀匹配(`/quark` 和 `/quark/backup`
  可同时存在);`.balance` 后缀实现同路径多账户负载均衡;alias 驱动可把多个挂载点
  再聚合出一个入口。
- **运行时热增删**:存储的增删改/启停全是 API 触发(写 SQLite + 内存 storagesMap),
  不重启进程;某个盘 Init 失败(如 token 失效)**不阻塞其他盘**,把错误状态写回存储
  记录、Web UI 展示(SetStatus 模式)。
- **driver 接口是"小必选集 + 大可选能力"**:

  ```go
  type Driver interface { Meta; Reader }   // 必需:Config/Init/Drop + List + Link
  // 可选能力(类型断言检测,没实现就降级):
  type Mkdir / Move / Rename / Copy / Remove / Put   // 写能力
  type Getter / WithDetails / DirectUploader / Reference …
  ```

  **最小驱动 = List + Link 两个方法**(纯只读盘),下载走 `Link` 返回的直链 302
  直连网盘 CDN、不过 AList 中转;声明式的 Addition struct tag(`type:"select"
  options:"a,b" required:"true"`)直接生成 Web UI 配置表单。op 层统一白送:
  目录缓存(可按通配符配 TTL)、singleflight、上传/复制任务队列、跨存储 transfer。
  单个开放接口驱动的实现量约 300-800 行。
- **token 驱动自治**:每个驱动自带 refresh 循环;≥3.42 有 "Reference" 机制
  (备注里写 `ref:/挂载路径`)让多个存储共享同一份认证。
- **部署/自启**:一键脚本装 systemd / 手动 alist.service / Docker。多网盘 =
  **一个服务实例管所有**(同进程、同端口、同一个 SQLite)。
- 2025-06 信任危机:原作者出售项目、收购方被指加数据收集代码,社区硬分叉出
  OpenList(AGPL-3.0、drop-in 替代、迁移前别升 AList v3.46+)。给我们的提醒:
  **认证中转服务是信任链上最脆的一环**,我们的 token 全本地存储、不走任何托管服务,
  这点要守住。

来源:<https://github.com/AlistGo/alist>、<https://github.com/OpenListTeam/OpenList>
(驱动数经 GitHub API 实数:AList 93 目录/OpenList 88 目录)、OpenList main 分支源码
`internal/driver/driver.go`、`internal/op/storage.go`、`internal/op/path.go`、
AlistGo/docs(webdav.md / drivers/develop.md / install/*)、<https://doc.oplist.org>。

## 2. CloudDrive2:单守护进程 + 自带 FUSE,商业闭源

**形态**:单一后台核心服务(PC/NAS/Docker/Android,浏览器 19798 端口管理),
**自己做 FUSE/盘符挂载**——既可每个云盘挂成独立卷,也可把多个云合并成一棵统一
文件系统树(用户自选);同时内建一个聚合所有云的 WebDAV 服务。

- 所有平台 App 与 Web UI 建在同一套 gRPC API 上(官方:"没有留一条自己才能走的后门")。
- 主打卖点:完整 POSIX 语义(权限位/属主/时间戳/xattr,Windows 上 ACL 完整读写),
  云上存不了的元数据本地持久化;**随机写不需要先下载原文件**——未改区间直接从云上
  流式透传、本地拼接("streams the untouched ranges straight from the cloud, splices
  in what you wrote locally")。我们的写路径是"整文件暂存 + close 上传",它做到了
  区间级,这是差距也是远期方向。
- 缓存哲学:读默认**零落盘**(按需取区间),folder cache 是每目录可选开关;
  写必须先进本地临时目录(上传要完整哈希,50GB 文件要 50GB 空闲)——和我们
  uploads-<盘>/ 暂存思路一致。
- 版本限制:Basic 会员 1 个本地挂载,Core/Lifetime 不限。

来源:<https://www.clouddrive2.com/>(中文官网)、<https://www.clouddrive2.com/en/features.html>、
<https://hub.docker.com/r/cloudnas/clouddrive2>。

## 3. RaiDrive / 其他

- **RaiDrive**(Win/Linux):单一后台服务(RaiDrive Filesystem Service)+ GUI 管所有云;
  每个云盘账户一个**用户指定盘符**(也可挂成文件夹形态);挂载配置在服务层持久化,
  重启自动重连。没有国内网盘原生驱动,国内盘常见链路是"网盘 → AList WebDAV → RaiDrive"。
- **Air Explorer vs Air Live Drive**:前者是双栏云管理器(不挂载),后者才是挂载型
  (单服务、每云一盘)。产品形态先想清楚"管理"和"挂载"是不是同一个东西。
- **ExpanDrive / Mountain Duck**:跨平台挂载型,每云一个卷。

来源:<https://www.raidrive.com/>、<https://docs.raidrive.com/en/gui/options/drive-letter-label/>、
<https://www.airlivedrive.com>、<https://mountainduck.io>。

## 4. rclone 与 systemd 多实例

**配置模型**:所有云盘在**单个 INI 文件**(`rclone.conf`)里,每 remote 一个
`[section]`(名字即 remote 名,必有 `type = <backend>`);路径语法 `remote:path`
冒号命名空间;`rclone config` 交互向导(n/e/d/r/c/s 菜单 → 编号选 backend → OAuth);
无头机器在别的机子上跑 `rclone authorize` 拿 token 粘回;多机迁移 = 复制这一个文件。

**挂载进程模型**:**一个 mount = 一个 rclone 进程**,默认前台;官方没有任何
"一条命令挂多个 remote"的机制。`--daemon` 是 parent/child 双进程模型,但维护者
ncw 明确建议 systemd 下**不要**用 `--daemon`,让 systemd 直接托管前台进程;
systemd 集成支持 `Type=notify`(rclone 检测到自己在 systemd 下会自动发 sd_notify)。
**官方文档明说的坑**:systemd 跑的服务没有任何环境变量(包括 `HOME`/`PATH`),
配置和缓存目录必须给绝对路径。

**模板单元(社区标准,重点)**:官方 GitHub wiki 的 "Systemd rclone mount" 页贡献了
`rclone@.service` 模板 + `systemctl --user enable --now rclone@<remote名>` 的用法,
**一份 unit 文件管理任意多个 remote**。要点:`%i` = remote 名、`%h` = home、
每实例可用 `EnvironmentFile=-%h/.config/rclone/%i.env` 覆盖挂载参数(同一模板,
不同盘不同参数,甚至同盘挂多处)、ExecStartPre 做安装检查、
`ExecStop=fusermount -u`、Restart=always + RestartSec。rclone 官方仓库不随包发行
这个文件,但维护者认可方向,论坛/教程广泛一致——属于"社区广泛验证"。

这与 systemd 生态的一等公民机制同构:**实例名 ↔ 一份配置文件**——
`wg-quick@wg0` ↔ `/etc/wireguard/wg0.conf`、`openvpn-client@x` ↔
`/etc/openvpn/client/x.conf`、`rclone@dropbox` ↔ rclone.conf 里一个 section、
**我们的 `bdfs@wopan` ↔ settings.json 里 wopan 那份设置**,一模一样的设计。

**聚合多个 remote**(两条官方路线,语义不同):

- **union**(mergerfs 式**同构合并**):`upstreams = r1:dir r2:dir`,同名目录合并,
  靠 action/create/search 三组 policy 决定读写落在哪个上游,上游可打 `:ro`/`:nc`/
  `:writeback` 标签。**坑**:能力是各上游的逻辑 **AND**——跨上游 move/rename 走
  "下载再上传"回源路径甚至直接报 I/O 错误(ncw 在论坛确认,issue #5632);
  任一上游宕机整个 union 不可用。
- **combine**(v1.59+,Tier 1):`upstreams = images=s3:x files=drive:y`,
  **每个 remote 一个顶层目录**,无合并语义、没有 union 那套 policy 问题。
  异构多网盘聚合的官方正解。
- **纠错**:`rclone serve webdav` **只接受一个 remote**,不存在
  `serve webdav r1: r2:` 的多路径形式;多盘要先 combine/union 再 serve。

**其他工具**:s3fs 是"单个密码文件多行凭证(`bucket:ak:sk`)+ 每挂载一进程 +
fstab 自启";goofys 单 bucket 不管生命周期;Mountain Duck 每账号一个书签一个卷 +
常驻 GUI 进程。无一例外都是"每挂载一份配置/一个进程",生命周期外包给 OS。

来源:<https://rclone.org/docs/>、<https://rclone.org/commands/rclone_config/>、
<https://rclone.org/commands/rclone_mount/>、<https://rclone.org/union/>、
<https://rclone.org/combine/>、<https://github.com/rclone/rclone/wiki/Systemd-rclone-mount>、
<https://gist.github.com/kabili207/2cd2d637e5c7617411a666d8d7e97101>、
<https://forum.rclone.org/t/rclone-mount-w-systemd-when-user-logs-in-unmounts-logout/15101>、
<https://forum.rclone.org/t/cant-move-inside-union-rclone-mount/26645>、
<https://github.com/s3fs-fuse/s3fs-fuse>、<https://github.com/kahing/goofys>。

## 5. 路线对比:A 每盘一进程 vs B 单守护进程聚合

| | A. 每盘一个挂载进程(rclone 传统用法、bdfs 现状) | B. 单守护进程 + 虚拟目录(AList/CloudDrive2/RaiDrive) |
|---|---|---|
| 隔离性 | 好:一盘崩溃只丢一个挂载点 | 差:单点;AList 在 init 处 recover,运行时 panic 仍可能拖全局 |
| 统一路径 | 难:要 union/combine/autofs 二次拼 | 原生:挂载路径即命名空间,跨盘复制一层搞定 |
| 管理 | N 份配置、N 个 systemd 单元、token 各自续 | 一个入口(我们的菜单/Web UI)、热增删不重启 |
| 性能 | 直连无中转 | AList 经 WebDAV 双跳;CloudDrive2 自带 FUSE 无此损耗 |
| 驱动复用 | 每进程重复实现认证/限速/重试 | 一套 driver 接口、共享缓存/任务队列 |

**AList 为什么选 B**:产品核心是 Web 文件列表/分享/预览,天然服务端;Go 单二进制 +
SQLite 零依赖;FUSE 三平台差异大(WinFsp/macFUSE/fuse3),外包给成熟生态最省力。
代价:本地体验多一跳 WebDAV、全盘共享进程稳定性、闭源授权服务曾带来 token 信任风险。

**CloudDrive2 选 B 且自己做 FUSE**:卖点是 POSIX/ACL/随机写/流播,必须控制 FUSE 层
和缓存引擎,WebDAV 中转做不到。

## 6. 对 bdfs 的启示与路线建议

已对齐的(不用改):

- `PanClient` trait ≈ AList driver 模型的 Rust 版:list/get_dlink/read_range/upload/
  mkdir/delete/mv/quota/account;`client_for(kind)` 注册表 ≈ drivers/all.go。
- **读路径是直连模型**:`get_dlink` + HTTP Range 直打网盘 CDN,等价 AList 的
  Link-302 直连——B 架构下吞吐上限的关键,我们天生就是对的。
- **写 = 本地暂存 + close 上传**,与 CloudDrive2 的"写经临时区"同型。
- token 本地存储、驱动自治刷新(百度 errno -6、联回 9999/1001 自动续期),
  不经任何第三方托管服务。
- systemd 自启走模板实例 `bdfs@.service`(本次改造),是 systemd 管理"同程序多实例"
  的标准形态,A 路线下这就是终态,不是过渡。

要补的(按优先级):

1. **能力 trait 拆分**(接新盘前做):现在 `PanClient` 是大 trait,新盘必须填满所有
   方法。学 AList 拆成 `核心(List/Link/元信息)+ 可选(Put/Mkdir/Move/Remove)`,
   fuser 层按能力分派,没实现写的盘挂载成 EROFS。**只读接入 ≈ 2 个方法**,
   这是"以后很多网盘"最大的门槛杠杆。
2. **配置改成开放注册表**:`Config{baidu: Settings, wopan: Settings}` 是闭集,
   改成 `backends: Map<id, Settings>` + `PanKind::all()` 动态发现;菜单的切换/登录/
   设置项全部按注册表渲染(自启管理页已经这么做了)。凭据文件已经按名区分,不用动。
   配置形态不必学 rclone 的单 INI——AList 用 SQLite、我们按盘分 JSON 文件,
   各自都验证过,单文件不是重点,"加盘不用改存量结构"才是。
3. **中期加 B 形态:`bdfs mount-all` 聚合挂载**:一个守护进程、一个 fuser session,
   挂载点下每个网盘一个顶层子目录(`/mnt/clouds/baidu/…`),后端路由表热更新。
   这是 rclone **combine** 式(每盘一个顶层目录),**不学 union 式合并**——
   union 的三组读写 policy + "能力是上游 AND"(跨盘 move 走回源甚至报错,
   rclone issue #5632)是公认的坑;我们跨子树 rename/move 直接返回 EXDEV
   (让用户显式复制)即可。比 AList 少一跳(它 WebDAV、我们原生 FUSE);
   比 A 模式少 N-1 个进程和单元。与现有 A 模式并存,用户自选。
   工程要点:fs.rs 加一层命名空间路由(顶层目录 → PanClient),
   目录/属性缓存按 (盘, 路径) 复合键。
4. **可选:暴露 WebDAV 口**(挂在同一守护进程上):AList 验证过这是最廉价的生态
   接口(播放器/RaiDrive/手机 App 都能接),我们已有完整数据模型,加个 handler 即可。
5. systemd 细节向 rclone 社区模板对齐:实例参数覆盖我们用 settings.json
   (等价它的 `%i.env` EnvironmentFile);不做 daemon 化(systemd 直接托管前台);
   **systemd 服务没有 `HOME`**(官方文档明说)——凭据在家目录里的,实例要靠
   drop-in `Environment="HOME=…"` 指回安装者(已实现);未来可考虑
   `Type=notify` + fuser 就绪时发 sd_notify。
6. 心态:CloudDrive2 的区间级随机写很远,不必追;AList 的"最小驱动两方法"要追。
