# 本地补丁说明

来源:crates.io `cloud-filter 0.0.6`(ho-229/cloud-filter-rs,MIT),原样 vendor 到本目录,
仅含以下改动。上游出新版时先比对是否已修,再考虑升级去掉 vendor。

## 补丁 1:`Write::fail` 失败路径修正(commands.rs / ticket.rs)

上游 0.0.6 从 `SyncFilter::fetch_data` 等回调返回 `Err` 时,proxy 会同步调
`CfExecute(TRANSFER_DATA, 错误状态)`,但 `Offset`/`Length` 填全 0、`Buffer` 指向栈上
`[0;1]`。实测平台直接拒绝,返回 `ERROR_CLOUD_OPERATION_INVALID`(0x8007017C"云操作无效"),
随后 proxy 里的 `.unwrap()` panic 击穿 `extern "system"` 边界,**宿主进程整个死掉**
(`bdfs sync` 起来后 `Get-Content <占位符>` 即可复现,进程退出码 9)。

修正分两层:

- **失败应答必须带 required range**(核心):`Offset`/`Length` 填 FETCH_DATA 回调请求的
  必需区间、`Buffer` 置空(对齐 MS CfApiSample)。新增 `Write::fail_range` 与
  `ticket::FetchData::fail(kind, range)`,实测能即时送达,应用侧 0.04s 拿到
  "云操作不成功",而不是挂 60s 超时。
- **proxy 不再 unwrap**(兜底):6 处 `command::*::fail(..).unwrap()` 改为 `deliver_fail()`
  记日志后放弃,失败应答万一仍送不出去时按平台超时处理,provider 存活。

## 补丁 2:`Placeholder::update` 的 dehydrate_ranges 条件写反(placeholder.rs)

上游 `update()` 把"dehydrate_ranges 是否传平台"的条件写反了:
`options.dehydrate_ranges.is_empty().then_some(&options.dehydrate_ranges)` ——
空区间时传 `Some`(空 Vec → 悬空指针 + count 0),真有区间时反倒传 `None`
(静默丢弃)。改为 `(!is_empty()).then_some(...)`,空区间传 NULL、非空才传切片。

## 平台行为备忘(非补丁,踩坑记录)

- `CfUpdatePlaceholder` **只能在连了同步根的 provider 进程内调用**:外部工具
  (GENERIC_WRITE、独占、甚至空 update)一律 `ERROR_CLOUD_FILE_ACCESS_DENIED`
  (0x8007018B)。cfops 的 `free`/`noop` 动词留作对照复现。
- `CfDehydratePlaceholder`(crate 的 `ext::FileExt::dehydrate`)已废弃,ack 送达后
  永远不完成——现代路径是 `CfUpdatePlaceholder` + `CF_UPDATE_FLAG_DEHYDRATE`
  (GENERIC_WRITE + 独占 Win32 句柄;oplock 句柄会 E_HANDLE)。
- 平台对 pin/unpin 都是**懒**的:`attrib +u` 只清 pin 标志不脱水,`attrib +p`
  只置 pin 标志不水合。要 OneDrive 式即时行为,provider 得自己在
  `state_changed`(属性变更)回调里对齐:unpin+干净+有数据 → update(dehydrate);
  pin+盘上不全 → `Placeholder::hydrate(..)` 触发 fetch_data。
- 目录 `convert_to_placeholder` 默认视为"已填充完毕"(不再回调
  fetch_placeholders);要按需填充得带 `has_children()`
  (`CF_CONVERT_FLAG_ENABLE_ON_DEMAND_POPULATION`)。已是占位符的空目录可用
  `UpdateOptions::has_children()` 重开。
- 在 dehydrate 回调里 `Placeholder::open` 同一文件会**死锁**(回调等 oplock、
  脱水持有它):回调里只能查进程内状态(如 bdfs 的 dirty 集)。

## 使用方注意事项

从 `fetch_data` 返回 `Err` 仍会走 proxy 的同步失败应答(全 0 区间,会被拒、然后被
兜底吞掉,用户等 60s 超时)。**要立即报错就别返回 Err**:回调内直接
`ticket.fail(kind, info.required_file_range())` 然后返回 `Ok(())`(见
src/win/provider.rs 的 M2 桩)。
