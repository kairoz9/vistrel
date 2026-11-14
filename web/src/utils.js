/**
 * Vistrel Web Client 通用工具
 * 剪贴板写入、浏览器下载触发与展示格式化
 */


/**
 * 写浏览器剪贴板：优先异步 Clipboard API，不可用或被拒时回退 execCommand
 *
 * @param {string} text 文本内容
 * @param {() => void} onSuccess 写入成功回调
 * @param {() => void} [onFailure] 两条路径都失败时的回调（如非安全上下文且 execCommand 被拒）
 */
export function copyTextWithFallback(text, onSuccess, onFailure) {
    const done = () => {
        if (onSuccess) onSuccess();
    };
    const fail = () => {
        if (onFailure) onFailure();
    };
    if (window.isSecureContext && navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(done, () => copyViaExecCommand(text, done, fail));
    } else {
        copyViaExecCommand(text, done, fail);
    }
}

/** execCommand 兜底：离屏 textarea 选中后复制，覆盖非安全上下文（http://局域网地址） */
function copyViaExecCommand(text, done, fail) {
    const scratch = document.createElement("textarea");
    scratch.className = "clipboard-scratch";
    scratch.value = text;
    document.body.appendChild(scratch);
    // iOS Safari 的 select() 常不产生选区，须显式设选中范围 execCommand 才生效
    scratch.setSelectionRange(0, text.length);
    scratch.select();
    let ok = false;
    try {
        ok = document.execCommand("copy");
    } catch (_) {}
    scratch.remove();
    if (ok) {
        done();
    } else {
        fail();
    }
}

/**
 * 开新窗直链：成功即断开 opener
 *
 * @param {string} url 直链地址（协议合法性由调用方校验）
 * @returns {boolean} 是否成功开窗；弹窗被浏览器拦截为 false，由调用方决定兜底呈现
 */
export function openExternalUrl(url) {
    const opened = window.open(url, "_blank");
    if (opened) opened.opener = null;
    return Boolean(opened);
}

/* ---------------- 下载 ---------------- */

/** 当前时刻 → 2026-10-31_18-06-28 形态的时间戳串 */
export function fileStamp() {
    const pad = (n) => String(n).padStart(2, "0");
    const d = new Date();
    return (
        `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}` +
        `_${pad(d.getHours())}-${pad(d.getMinutes())}-${pad(d.getSeconds())}`
    );
}

/** 以浏览器下载方式保存 Blob，10 秒后回收对象 URL */
export function downloadBlob(blob, filename) {
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = filename;
    anchor.click();
    setTimeout(() => URL.revokeObjectURL(url), 10000);
}

/* ---------------- 格式化 ---------------- */

/** 秒 → 分:秒（超一小时为 时:分:秒） */
export function formatDuration(totalSeconds) {
    const s = Math.max(0, Math.floor(totalSeconds));
    const hh = Math.floor(s / 3600);
    const mm = String(Math.floor((s % 3600) / 60)).padStart(2, "0");
    const ss = String(s % 60).padStart(2, "0");
    return hh > 0 ? `${hh}:${mm}:${ss}` : `${mm}:${ss}`;
}

/** 字节数 → KB（整数）/ MB（一位小数）/ GB（两位小数） */
export function formatSize(bytes) {
    if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(2)} GB`;
    if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
    return `${Math.max(0, Math.round(bytes / 1024))} KB`;
}
