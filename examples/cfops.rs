//! 开发辅助:手动触发云端文件操作,模拟 Explorer 右键菜单。
//!
//! cargo run --example cfops -- dehyd <路径>     # 释放空间(废弃 API,会挂,仅留作对照)
//! cargo run --example cfops -- free <路径>      # 释放空间(CfUpdatePlaceholder+DEHYDRATE)
//! cargo run --example cfops -- insync <路径>    # 标记 in-sync(测试后恢复状态)
//! cargo run --example cfops -- info <路径>      # 查看 pin/in-sync/盘上数据量

use cloud_filter::ext::FileExt;
use cloud_filter::placeholder::Placeholder;
use std::path::Path;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let verb = args
        .next()
        .unwrap_or_else(|| usage("缺动词"));
    let path = args.next().unwrap_or_else(|| usage("缺路径"));
    let path = Path::new(&path);
    match verb.to_string_lossy().as_ref() {
        "dehyd" => {
            // 全共享(含 DELETE):脱水要替换文件数据,普通 File::open 的共享模式
            // 会把平台的脱水卡死,回调线程也跟着挂在 CfExecute 里
            use std::os::windows::fs::OpenOptionsExt;
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true) // 脱水=改文件,只读句柄平台走不下去
                .share_mode(7) // FILE_SHARE_READ|WRITE|DELETE
                .open(path)
                .expect("打开文件");
            f.dehydrate(..).expect("触发脱水");
            println!("脱水完成:{}", path.display());
        }
        "noop" => {
            // 诊断:不带任何 flag/metadata 的空 CfUpdatePlaceholder
            use cloud_filter::placeholder::UpdateOptions;
            use std::os::windows::fs::OpenOptionsExt;
            let f = std::fs::OpenOptions::new()
                .access_mode(0x4000_0000)
                .share_mode(0)
                .open(path)
                .expect("独占打开失败(被占用?)");
            let mut ph = Placeholder::from(f);
            ph.update(UpdateOptions::default(), None).expect("空 update");
            println!("空 update 成功:{}", path.display());
        }
        "free" => {
            // 现代脱水路径:CfUpdatePlaceholder + CF_UPDATE_FLAG_DEHYDRATE。
            // 句柄照抄 Nextcloud:CreateFile(access=0, share=0) 独占、无读写权——
            // cfapi 的 oplock 保护句柄传进去会报 E_HANDLE,普通 Win32 句柄才行
            use cloud_filter::placeholder::UpdateOptions;
            use std::os::windows::fs::OpenOptionsExt;
            let f = std::fs::OpenOptions::new()
                .access_mode(0x4000_0000) // GENERIC_WRITE(WRITE_DATA,文档要求)
                .share_mode(0) // 独占(平台对脱水句柄的独占要求)
                .open(path)
                .expect("独占打开失败(被占用?)");
            let mut ph = Placeholder::from(f);
            ph.update(UpdateOptions::default().dehydrate(), None)
                .expect("触发脱水更新");
            println!("脱水完成:{}", path.display());
        }
        "insync" => {
            let mut ph = Placeholder::open(path).expect("打开占位符");
            ph.mark_in_sync(true, None).expect("标记 in-sync");
            println!("已标记 in-sync:{}", path.display());
        }
        "info" => {
            let ph = Placeholder::open(path).expect("打开占位符");
            let info = ph.info().expect("查询状态").expect("不是占位符");
            println!(
                "pin={:?} in_sync={} 盘上={} 字节",
                info.pin_state(),
                info.is_in_sync(),
                info.on_disk_data_size()
            );
        }
        _ => usage("未知动词"),
    }
}

fn usage(msg: &str) -> ! {
    eprintln!("{msg};用法:cfops <dehyd|insync|info> <路径>");
    std::process::exit(2);
}
