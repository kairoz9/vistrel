import { UIManager } from "./ui/ui.js";
import { tryUnmuteAudio } from "./streaming/audio.js";
import { copyTextWithFallback, openExternalUrl } from "./utils.js";
import { InputController } from "./streaming/input.js";
import { StreamingSession, ConnectStatus, SESSION_EVENTS } from "./streaming/session.js";
import { RecordingController } from "./streaming/recorder.js";
import { ReplayClipController } from "./streaming/recorder.js";

const VISTREL_PASSWORD_KEY = "vistrel_password";
const URL_CODEC_KEY = "codec";

window.addEventListener("DOMContentLoaded", () => {
    const ui = new UIManager();
    const urlParams = new URLSearchParams(window.location.search);
    const urlCodec = urlParams.get(URL_CODEC_KEY);

    let currentPassword = (sessionStorage.getItem(VISTREL_PASSWORD_KEY) || "").trim();

    let recordingController = null;
    let inputController = null;

    // 建立浏览器侧串流会话：构造只接环境与输入，输出全部走订阅
    const session = new StreamingSession({
        videoElement: ui.video,
        audioElement: ui.audio,
        urlCodec: urlCodec,
        getBpp: () => ui.curBpp,
        getViewport: () => ui.videoViewport(),
    });

    // 会话状态与协商结果 → UI；start() 只在认证链末尾调用，订阅一定先于首批事件
    session.on(SESSION_EVENTS.STATUS, (status) => {
        ui.setStatus(status);
        // 录制进行中即使瞬态断线（可恢复）也保持停止按钮可用
        const recordUsable =
            status === ConnectStatus.CONNECTED || (recordingController !== null && recordingController.isRecording());
        ui.setRecordAvailability(recordingController !== null && recordingController.isSupported(), recordUsable);
    });
    session.on(SESSION_EVENTS.PROFILE, (profile) => {
        ui.modals.kicked.hide();
        // answer 携带的协商值先上屏（此刻视频元素还没有首帧），
        // 后续内容驱动的分辨率变化由视频元素 resize 事件接管
        ui.setStreamExtent(profile.width, profile.height);
        if (profile.codec) {
            ui.setCodec(profile.codec);
        }
        if (profile.backend) {
            ui.setBackend(profile.backend);
        }
    });
    session.on(SESSION_EVENTS.STATS, (stats) => {
        if (stats.rttMs !== null) ui.setRtt(stats.rttMs);
        if (stats.fps !== null) ui.setFps(stats.fps);
        if (stats.diagText !== null) ui.setNetDiag(stats.diagText);
        if (stats.actualCodec) ui.setCodec(stats.actualCodec);
    });
    session.on(SESSION_EVENTS.AUTH, (result) => {
        if (result.ok) {
            ui.hidePasswordModal();
            if (currentPassword) {
                sessionStorage.setItem(VISTREL_PASSWORD_KEY, currentPassword);
            }
            return;
        }
        currentPassword = "";
        sessionStorage.removeItem(VISTREL_PASSWORD_KEY);
        ui.showPasswordModal(result.message);
    });
    session.on(SESSION_EVENTS.KICKED, () => ui.modals.kicked.show());

    // 播放流录制控制器：断线时由会话拆除通知触发自动保存
    recordingController = new RecordingController({
        getTracks: () => ({ videoTrack: session.videoTrack, audioTrack: session.audioTrack }),
        getInboundBitrate: () => session.getInboundBitrateEstimate(),
        onStateChange: (active, text) => ui.setRecordState(active, text),
        onSaved: (saved) => {
            const reasonText = { interrupted: "（连接中断）", error: "（录制出错）" }[saved.reason] ?? "";
            ui.showToast(`已保存 ${saved.sizeText}${reasonText}`);
        },
        onError: (msg) => {
            ui.showToast(msg);
        },
    });
    ui.bindRecordToggle(() => recordingController.toggle());
    ui.setRecordAvailability(recordingController.isSupported(), false);

    // 回放剪辑控制器：与录制零耦合，断线时静默弃置缓冲
    const replayController = new ReplayClipController({
        getTracks: () => ({ videoTrack: session.videoTrack, audioTrack: session.audioTrack }),
        getInboundBitrate: () => session.getInboundBitrateEstimate(),
        onBufferActive: (active, reason) => ui.setClipAvailability(active, reason),
        onNote: (msg) => ui.showToast(msg),
    });
    ui.setReplayToggle(replayController.isEnabled());
    ui.bindReplayToggle(() => {
        replayController.setEnabled(!replayController.isEnabled());
        ui.setReplayToggle(replayController.isEnabled());
    });
    ui.bindClipSave(() => replayController.saveClip());
    replayController.notifyAvailability();

    // 媒体解锁：iOS 低电量模式连 muted 自动播放都会被拦，所有用户手势顺带补一次播放
    const unlockMedia = () => {
        tryUnmuteAudio(ui.audio);
        ui.video.play().catch(() => {});
    };

    // 建立输入控制器
    inputController = new InputController({
        targetElement: ui.gameSurface,
        videoElement: ui.video,
        getDataChannel: () => session.getInputDataChannel(),
        sendTextInput: (text) => session.sendTextInput(text),
        onUserInteraction: unlockMedia,
        onLockChange: (isLocked) => {
            ui.updateSystemCursor(isLocked);
        },
        onUnlockHint: (visible) => ui.setLockHint(visible),
    });
    ui.bindModalChange((visible) => inputController.setModalOpen(visible));

    // 依赖就位的边沿订阅：轨道到达唤醒回放缓冲，游戏侧动作转交输入层与浏览器
    session.on(SESSION_EVENTS.VIDEO_TRACK, (track) => replayController.handleVideoTrack(track));
    session.on(SESSION_EVENTS.AUDIO_TRACK, (track) => replayController.handleAudioTrack(track));
    session.on(SESSION_EVENTS.ACTION, (action) => {
        if (action.type === "cursor") {
            inputController.setCursorState(action.visible);
            return;
        }
        if (action.type === "open-url") {
            // 浏览器侧只校验协议（Java 侧已限 http/https），打开本身不二次确认：游戏内确认屏即用户意图；
            // 非法协议静默丢弃，被弹窗拦截时落到卡片，由用户点按钮再开
            let safeUrl = null;
            try {
                const parsed = new URL(action.url);
                if (parsed.protocol === "http:" || parsed.protocol === "https:") safeUrl = parsed.href;
            } catch (_) {}
            if (!safeUrl) return;
            if (!openExternalUrl(safeUrl)) ui.showNoticeCard("url", safeUrl);
            return;
        }
        // clipboard
        if (!action.text) return;
        // 游戏内复制动作即用户意图：静默直写不二次确认，成功只发一条无需点击的 toast 反馈；
        // 仅当非安全上下文且激活已过期（两条写入路径都物理不可行）时出恢复弹窗
        copyTextWithFallback(
            action.text,
            () => ui.showToast("已复制到浏览器剪贴板"),
            () => ui.showNoticeCard("clipboard", action.text),
        );
    });
    session.on(SESSION_EVENTS.TORN_DOWN, () => {
        if (recordingController) recordingController.handleSessionTornDown();
        replayController.handleSessionTornDown();
    });

    // 绑定画质档位变更：偏好同步走 settings 消息，几何与帧率由服务端内容推导
    ui.bindProfileChange(() => {
        session.sendSettings();
    });

    // 窗口尺寸变化防抖同步偏好（视口随 settings 一并携带）：
    // 自动上限输入，随后的实际档位由服务端稳定窗口推导
    let viewportReportTimer = null;
    window.addEventListener("resize", () => {
        if (viewportReportTimer) clearTimeout(viewportReportTimer);
        viewportReportTimer = setTimeout(() => session.sendSettings(), 1000);
    });

    // 绑定认证提交与断开重连
    ui.bindPasswordSubmit((password) => {
        tryUnmuteAudio(ui.audio);
        currentPassword = password;
        session.start(currentPassword);
    });

    ui.bindReconnect(() => {
        session.resetKicked();
        session.start(currentPassword);
    });

    // 启动初始连接或展示密码弹窗：先向主机确认是否启用了密码认证，免认证时直接进会话
    const startWithStoredPassword = () => {
        if (currentPassword) {
            session.start(currentPassword);
        } else {
            ui.showPasswordModal();
        }
    };
    fetch("/api/auth")
        .then((r) => r.json())
        .then((info) => {
            if (info && info.passwordRequired === false) {
                currentPassword = "";
                session.start("");
                return;
            }
            startWithStoredPassword();
        })
        .catch(startWithStoredPassword);
});
