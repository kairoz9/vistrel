/**
 * Vistrel Web Client 播放流抓取
 * 一条持续重编码的 fMP4 分片流（CaptureController）承载两套用法：Playout Recording
 * 停止即落盘，回放剪辑只留 init segment 与最近 60s 墙钟窗口按需取用。两套用法各自
 * 注入分片保留策略并自行处置产物 Blob，CaptureController 内没有任何模式分支。
 * 线格式为顶行在前，轨道直录方向即显示方向。
 */

import { downloadBlob, fileStamp, formatDuration, formatSize } from "../utils.js";

// MP4 候选 MIME 串：音频编码按平台落 AAC（Safari）或 Opus（Chromium 的 MP4 muxer 不支持 AAC），逐一探测取首个命中
const MP4_MIME_CANDIDATES = [
    'video/mp4;codecs="avc1.640028,mp4a.40.2"',
    'video/mp4;codecs="avc1,mp4a.40.2"',
    "video/mp4;codecs=avc1,opus",
    "video/mp4;codecs=avc1",
    "video/mp4",
];

const FALLBACK_VIDEO_BITRATE_BPS = 12000000;
const MIN_VIDEO_BITRATE_BPS = 6000000;
const MAX_VIDEO_BITRATE_BPS = 32000000;
// 重编码存在代际损失，两套用法的录制码率都在入流码率之上留 20% 余量
const INBOUND_HEADROOM = 1.2;
const AUDIO_BITRATE_BPS = 128000;
// 分片节奏两套用法共用：录制逐分片刷新时长与体积，剪辑逐分片推进墙钟窗口
const TIMESLICE_MS = 1000;

// 回放剪辑：常驻缓冲最近 60s 的 fMP4 分片
const REPLAY_WINDOW_MS = 60000;
const REPLAY_ENABLED_KEY = "vistrel_replay_enabled";
// 音频轨与视频轨通常同批到达，留短宽限把音频一并纳入缓冲；超时则先录纯视频
const AUDIO_GRACE_MS = 1000;

/**
 * 选出当前浏览器支持的首个 MP4 录制串，全不支持返回 null
 */
function pickRecordingMime() {
    for (const type of MP4_MIME_CANDIDATES) {
        try {
            if (typeof MediaRecorder !== "undefined" && MediaRecorder.isTypeSupported(type)) return type;
        } catch (_) {}
    }
    return null;
}

/**
 * 由入流码率估算派生录制视频码率：×1.2 后夹取 [6, 32] Mbps，无估算回退 12 Mbps
 */
function deriveRecordingBitrate(inboundBps) {
    if (typeof inboundBps !== "number" || !Number.isFinite(inboundBps) || inboundBps <= 0) {
        return FALLBACK_VIDEO_BITRATE_BPS;
    }
    return Math.round(Math.min(MAX_VIDEO_BITRATE_BPS, Math.max(MIN_VIDEO_BITRATE_BPS, inboundBps * INBOUND_HEADROOM)));
}

/**
 * fMP4 分片抓取：装配 MediaRecorder、兜住视频轨 ended、幂等停止并交付末分片。
 * 不持有任何文案/呈现策略——缓冲内容形态由注入的 retention 决定，产物交回 onStopped。
 * 停止可由用户、会话拆除、轨道 ended、录制器错误多路触发，必须保持幂等。
 */
export class CaptureController {
    /**
     * @param {Object} options
     * @param {() => {videoTrack: MediaStreamTrack|null, audioTrack: MediaStreamTrack|null}} options.getTracks
     * @param {() => number|null} [options.getInboundBitrate] 入流视频码率估算 (bps)，无估算返回 null
     * @param {Object} options.retention 分片保留策略，见 keepAll / windowOf
     * @param {() => void} options.onChunk 收到一个可用分片（缓冲内容已更新）
     * @param {(blob: Blob|null, cause: string) => void} options.onStopped 停止收尾，参数为合成好的 Blob
     *   （无内容时 null）与停止来源 cause ∈ user / ended / error
     */
    constructor({ getTracks, getInboundBitrate = null, retention, onChunk, onStopped }) {
        this.getTracks = getTracks;
        this.getInboundBitrate = getInboundBitrate;
        this.retention = retention;
        this.onChunk = onChunk;
        this.onStopped = onStopped;

        this.mime = pickRecordingMime();
        this.recorder = null;
        this.track = null;
        this.trackEndedHandler = null;
        this.startedAt = 0;
        this.settled = false;
        this.stopCause = "user";
    }

    get isActive() {
        return this.recorder !== null;
    }

    get durationMs() {
        return this.recorder === null ? 0 : Date.now() - this.startedAt;
    }

    /**
     * 起一段分片流。幂等：已在录时直接成功返回。
     * @returns {null|{reason: string, error?: unknown}} null 表示已启动，否则给出失败原因
     */
    start() {
        if (this.recorder !== null) return null;
        if (this.mime === null) return { reason: "unsupported" };

        const { videoTrack, audioTrack } = this.getTracks();
        if (!videoTrack) return { reason: "no-video" };

        const tracks = audioTrack ? [videoTrack, audioTrack] : [videoTrack];
        let recorder;
        try {
            recorder = new MediaRecorder(new MediaStream(tracks), {
                mimeType: this.mime,
                videoBitsPerSecond: deriveRecordingBitrate(this.getInboundBitrate ? this.getInboundBitrate() : null),
                audioBitsPerSecond: AUDIO_BITRATE_BPS,
            });
        } catch (e) {
            return { reason: "failed", error: e };
        }

        this.recorder = recorder;
        this.track = videoTrack;
        this.startedAt = Date.now();
        this.settled = false;
        this.stopCause = "user";
        this.retention.reset();

        // 轨道 ended 是会话拆除之外的兜底停止点（如游戏侧流重启）
        this.trackEndedHandler = () => this.stop("ended");
        videoTrack.addEventListener("ended", this.trackEndedHandler);

        recorder.ondataavailable = (e) => {
            // 收尾后到达的分片直接丢弃，停止路径因此不需要摘句柄
            if (this.recorder !== recorder) return;
            if (!e.data || e.data.size === 0) return;
            this.retention.push(e.data);
            this.onChunk();
        };
        recorder.onerror = () => this.stop("error");
        recorder.onstop = () => this.settle();
        recorder.start(TIMESLICE_MS);
        return null;
    }

    /**
     * 停止当前分片流：摘 ended 兜底、请求停止，末分片收齐后经 onStopped 交付。
     * 幂等，多路触发安全。
     * @param {string} cause 停止来源，∈ user / ended / error，随 onStopped 交回调用方判定语义
     */
    stop(cause = "user") {
        const recorder = this.recorder;
        if (recorder === null) return;
        this.stopCause = cause;
        if (this.track && this.trackEndedHandler) {
            this.track.removeEventListener("ended", this.trackEndedHandler);
        }
        this.track = null;
        this.trackEndedHandler = null;

        // 先摘句柄防重入，再请求停止；已 inactive（如致命错误后）则直接收尾
        this.recorder = null;
        if (recorder.state !== "inactive") {
            recorder.stop();
        } else {
            this.settle();
        }
    }

    /** 非破坏取当前缓冲合成 MP4：剪辑可连续保存，录制收尾也走它 */
    takeBlob() {
        const parts = this.retention.take();
        return parts.length > 0 ? new Blob(parts, { type: this.mime }) : null;
    }

    settle() {
        if (this.settled) return;
        this.settled = true;
        const blob = this.takeBlob();
        this.retention.reset();
        this.onStopped(blob, this.stopCause);
    }
}

/**
 * 录制保留策略：全量累积到停止为止，体积即录制时长 × 码率
 */
export function keepAll() {
    let parts = [];
    let bytes = 0;
    return {
        push(chunk) {
            parts.push(chunk);
            bytes += chunk.size;
        },
        take() {
            return parts.slice();
        },
        reset() {
            parts = [];
            bytes = 0;
        },
        get bytes() {
            return bytes;
        },
    };
}

/**
 * 剪辑保留策略：首个分片是 init segment（ftyp+moov），整个会话永久保留；
 * 其余分片进墙钟窗口环形缓冲，过期即弃。数据停滞时窗口自然冻结。
 * take() 返回的序列可直接合成 MP4，窗口内无分片时为空。
 * @param {number} windowMs 窗口长度（墙钟毫秒）
 */
export function windowOf(windowMs) {
    let init = null;
    let recent = [];
    let bytes = 0;
    return {
        push(chunk) {
            bytes += chunk.size;
            if (init === null) {
                // 合成剪辑必须把 init segment 置于最前
                init = chunk;
                return;
            }
            recent.push({ blob: chunk, at: Date.now() });
            const cutoff = Date.now() - windowMs;
            while (recent.length > 0 && recent[0].at < cutoff) {
                bytes -= recent.shift().blob.size;
            }
        },
        take() {
            // init 单独存在不可播，至少要有一个窗口分片才算就绪
            if (init === null || recent.length === 0) return [];
            return [init, ...recent.map((c) => c.blob)];
        },
        reset() {
            init = null;
            recent = [];
            bytes = 0;
        },
        get bytes() {
            return bytes;
        },
        get ready() {
            return init !== null && recent.length > 0;
        },
    };
}

function recordingFilename() {
    return `vistrel_${fileStamp()}.mp4`;
}

const RECORD_FAILURE_TEXT = {
    unsupported: "当前浏览器不支持 MP4 录制",
    "no-video": "当前没有可录制的画面",
};

/**
 * Playout Recording：停止即落盘。用户、会话拆除、轨道 ended、录制器错误
 * 四路停止都必须保存已录部分，绕开这条纪律等于丢用户数据。
 */
export class RecordingController {
    /**
     * @param {Object} options
     * @param {() => {videoTrack: MediaStreamTrack|null, audioTrack: MediaStreamTrack|null}} options.getTracks
     * @param {() => number|null} options.getInboundBitrate 入流视频码率估算 (bps)，无估算返回 null
     * @param {(active: boolean, text: string) => void} options.onStateChange 录制态与实时文案 (REC 时长 · 大小)
     * @param {({sizeText: string, reason: string}) => void} options.onSaved 保存完成，reason ∈ user / interrupted / error
     * @param {(msg: string) => void} options.onError
     */
    constructor({ getTracks, getInboundBitrate, onStateChange, onSaved, onError }) {
        this.onStateChange = onStateChange;
        this.onSaved = onSaved;
        this.onError = onError;
        this.stopReason = null;
        this.capture = new CaptureController({
            getTracks,
            getInboundBitrate,
            retention: keepAll(),
            onChunk: () => this.emitState(),
            onStopped: (blob, cause) => this.finalize(blob, cause),
        });

        // 常驻卸载守卫：录制中才拦截，空闲时惰性；剪辑的滚动缓冲丢了无所谓，故不共用
        this.beforeUnloadHandler = (e) => {
            if (!this.capture.isActive) return;
            e.preventDefault();
            e.returnValue = "";
        };
        window.addEventListener("beforeunload", this.beforeUnloadHandler);
    }

    isSupported() {
        return this.capture.mime !== null;
    }

    isRecording() {
        return this.capture.isActive;
    }

    toggle() {
        if (this.capture.isActive) {
            this.stop("user");
        } else {
            this.start();
        }
    }

    start() {
        this.stopReason = null;
        const failure = this.capture.start();
        if (failure === null) {
            this.emitState();
            return;
        }
        this.onError(
            failure.reason === "failed"
                ? `录制启动失败: ${failure.error?.message || failure.error}`
                : RECORD_FAILURE_TEXT[failure.reason],
        );
    }

    /**
     * 停止录制并保存已录部分
     * @param {string} reason 停止原因，∈ user / interrupted / error
     */
    stop(reason) {
        if (!this.capture.isActive) return;
        this.stopReason = reason;
        this.capture.stop();
    }

    /** 会话拆除通知：断线/被抢占/重建连接时保存已录部分 */
    handleSessionTornDown() {
        this.stop("interrupted");
    }

    finalize(blob, cause) {
        // 用户与拆除是我们自己喊停，轨道 ended 与录制器错误由机制层上报
        const reason = this.stopReason || (cause === "ended" ? "interrupted" : cause);
        this.onStateChange(false, "");
        if (blob === null) {
            this.onError("未录制到内容，未生成文件");
            return;
        }
        downloadBlob(blob, recordingFilename());
        this.onSaved({ sizeText: formatSize(blob.size), reason });
    }

    emitState() {
        const elapsedSec = this.capture.durationMs / 1000;
        this.onStateChange(true, `REC ${formatDuration(elapsedSec)} · ${formatSize(this.capture.retention.bytes)}`);
    }
}

function readReplayEnabled() {
    try {
        return localStorage.getItem(REPLAY_ENABLED_KEY) !== "0";
    } catch (_) {
        return true;
    }
}

function clipFilename() {
    return `vistrel-clip_${fileStamp()}.mp4`;
}

/**
 * 回放剪辑：常驻分片流只保留 init segment 与最近 60s 墙钟窗口，
 * 按钮触发时取当前窗口合成下载；会话拆除静默弃置——与 Playout Recording 的落盘保存语义刻意相反。
 * 与 Playout Recording 各自持有分片流实例，互不感知，可同时运行。
 */
export class ReplayClipController {
    /**
     * @param {Object} options
     * @param {() => {videoTrack: MediaStreamTrack|null, audioTrack: MediaStreamTrack|null}} options.getTracks
     * @param {() => number|null} options.getInboundBitrate 入流视频码率估算 (bps)，无估算返回 null
     * @param {(active: boolean, reason: string) => void} options.onBufferActive 剪辑按钮可用态与不可用原因
     * @param {(msg: string, isError: boolean) => void} options.onNote 剪辑动作结果提示
     */
    constructor({ getTracks, getInboundBitrate, onBufferActive, onNote }) {
        this.getTracks = getTracks;
        this.onBufferActive = onBufferActive;
        this.onNote = onNote;
        this.enabled = readReplayEnabled();
        this.audioGraceTimer = null;
        this.availabilityKey = "";
        this.capture = new CaptureController({
            getTracks,
            getInboundBitrate,
            retention: windowOf(REPLAY_WINDOW_MS),
            onChunk: () => this.notifyAvailability(),
            onStopped: () => this.notifyAvailability(),
        });
    }

    isEnabled() {
        return this.enabled;
    }

    /**
     * 缓冲开关：关闭即弃置现有缓冲（静默）；重新开启且轨道在位时重新起录。
     * 开关只表达意愿，缓冲能否运行由轨道到达驱动。
     */
    setEnabled(on) {
        if (this.enabled === on) return;
        this.enabled = on;
        try {
            localStorage.setItem(REPLAY_ENABLED_KEY, on ? "1" : "0");
        } catch (_) {
            // 隐私模式下写入失败，仅本次会话内生效
        }
        if (on && this.getTracks().videoTrack) {
            this.scheduleBufferStart();
        } else if (!on) {
            this.stopBuffer();
        }
        if (on) {
            this.notifyAvailability();
        }
    }

    /** 新视频轨到达（含重连换轨）：缓冲起录的唯一驱动入口 */
    handleVideoTrack(_track) {
        if (!this.enabled || this.capture.isActive) return;
        this.scheduleBufferStart();
    }

    /** 音频轨到达：若视频轨还在宽限等待，立即起录一并纳入 */
    handleAudioTrack(_track) {
        if (!this.enabled || this.audioGraceTimer === null) return;
        clearTimeout(this.audioGraceTimer);
        this.audioGraceTimer = null;
        this.startBuffer();
    }

    /** 会话拆除通知：静默弃置，剪辑语义只覆盖「最近 60 秒」，断线残段没有保存价值 */
    handleSessionTornDown() {
        this.stopBuffer();
    }

    scheduleBufferStart() {
        if (this.capture.isActive || this.audioGraceTimer !== null) return;
        this.audioGraceTimer = setTimeout(() => {
            this.audioGraceTimer = null;
            this.startBuffer();
        }, AUDIO_GRACE_MS);
    }

    startBuffer() {
        if (this.capture.isActive || this.capture.mime === null) return;
        const failure = this.capture.start();
        if (failure && failure.reason === "failed") {
            this.onNote(`回放缓冲启动失败: ${failure.error?.message || failure.error}`, true);
        }
    }

    /** 停止缓冲并静默弃置缓冲内容：不合成、不下载、不提示，只刷新按钮可用态 */
    stopBuffer() {
        if (this.audioGraceTimer !== null) {
            clearTimeout(this.audioGraceTimer);
            this.audioGraceTimer = null;
        }
        if (!this.capture.isActive) return;
        this.capture.stop();
    }

    /** 合成当前窗口并下载：init segment + 窗口内分片，无冷却节流 */
    saveClip() {
        const blob = this.capture.takeBlob();
        if (blob === null) {
            this.onNote(this.capture.mime === null ? "当前浏览器不支持 MP4 录制" : "回放缓冲尚未就绪", true);
            return;
        }
        downloadBlob(blob, clipFilename());
        this.onNote(`已保存 ${formatSize(blob.size)}`, false);
    }

    /** 剪辑按钮可用态：缓冲在跑且窗口内有内容即可剪辑；分片每秒到达，就绪态没翻转就不碰 DOM */
    notifyAvailability() {
        const active = this.capture.isActive && this.capture.retention.ready;
        let reason = "";
        if (!active) {
            if (this.capture.mime === null) {
                reason = "当前浏览器不支持 MP4 录制";
            } else if (!this.enabled) {
                reason = "回放缓冲已关闭";
            } else if (this.capture.isActive) {
                reason = "回放缓冲就绪中…";
            } else {
                reason = "等待画面接入";
            }
        }
        const key = active ? "ready" : reason;
        if (key === this.availabilityKey) return;
        this.availabilityKey = key;
        this.onBufferActive(active, reason);
    }
}
