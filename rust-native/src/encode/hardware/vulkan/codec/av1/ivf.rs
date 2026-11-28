//! AV1 码流的 IVF 调试落盘

/// 调试用：把一帧码流按 IVF 容器追加落盘，供 `ffprobe` / `ffmpeg` 直接读
///
/// **为什么用 IVF 而不是裸 OBU**：ffmpeg 的 `obu` demuxer 靠 temporal delimiter 切帧，
/// 而驱动的输出不保证带 TD。IVF 只是 32 字节文件头 + 每帧 `(u32 长度, u64 时间戳, 数据)`，
/// 自己拼十几行就够，且 ffmpeg 原生支持。
///
/// **只用于排障**（`VISTREL_DUMP_IVF` 开关）：写文件在编码热路径上，`write_all` 失败
/// 一律静默忽略——调试代码不该因为磁盘问题让编码器报错。
///
/// fourcc 写死 `AV01`：本函数只为 AV1 排障，拿 H.264 / H.265 跑出来的文件 ffprobe 会认错。
pub(in crate::encode::hardware::vulkan) fn dump(path: &str, width: u32, height: u32, frame: &[u8]) {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    // 时间戳用独立自增计数，不用 `frame_num`：后者在每次 IDR 归零，会让 IVF 里的
    // 时间戳倒退，ffmpeg 可能据此重排或告警。IVF 的 timebase 是 60/1，计数即帧序号。
    static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);
    // 打不开只报一次，避免每帧刷屏
    static OPEN_FAILED: std::sync::Once = std::sync::Once::new();

    // 头只在文件不存在时写：存在说明是同一轮的后续帧，直接追加
    let fresh = !std::path::Path::new(path).exists();
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(file) => file,
        Err(e) => {
            OPEN_FAILED.call_once(|| log::warn!("码流落盘失败（{path}）: {e}"));
            return;
        }
    };

    if fresh {
        // 打印绝对路径：编码器跑在 Minecraft 进程里，工作目录不一定是命令行所在目录，
        // 「文件在哪」本身就是一个需要排除的变量
        if let Ok(cwd) = std::env::current_dir() {
            log::info!("码流落盘: {}", cwd.join(path).display());
        }
        let mut header = Vec::with_capacity(32);
        header.extend_from_slice(b"DKIF");
        header.extend_from_slice(&0u16.to_le_bytes()); // version
        header.extend_from_slice(&32u16.to_le_bytes()); // 文件头长度
        header.extend_from_slice(b"AV01");
        header.extend_from_slice(&(width as u16).to_le_bytes());
        header.extend_from_slice(&(height as u16).to_le_bytes());
        header.extend_from_slice(&60u32.to_le_bytes()); // timebase 分母
        header.extend_from_slice(&1u32.to_le_bytes()); // timebase 分子
        header.extend_from_slice(&0u32.to_le_bytes()); // 帧数，0 = 未知
        header.extend_from_slice(&0u32.to_le_bytes());
        let _ = file.write_all(&header);
    }

    let ts = FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
    let _ = file.write_all(&(frame.len() as u32).to_le_bytes());
    let _ = file.write_all(&ts.to_le_bytes());
    let _ = file.write_all(frame);
}
