import { BPP_TIERS, FIT_MODES } from "../streaming/protocol.js";
import { MetricRow, StatusDot, Modal, DiscreteSlider, Button } from "./components.js";
import { DraggablePanel } from "./draggable-panel.js";
import { copyTextWithFallback, openExternalUrl } from "../utils.js";
import { notice, pin, toast, unpin } from "./notice.js";

const VISTREL_HUD_KEY = "vistrel_hud_pos";
// 上次的画质档位与显示适配：几何与帧率归服务端内容推导，客户端只保留这两个偏好
const VISTREL_PROFILE_KEY = "vistrel_profile";
// 解锁后的常驻提示文案：常驻槽位归 notice 模块所有，被一次性提示遮蔽后自动放回
const LOCK_HINT_TEXT = "点击画面锁定鼠标";

/** 读取上次的偏好；旧版含 width/height/fps 的存档只取 bpp，几何字段作废 */
function readProfilePreference() {
    try {
        const raw = localStorage.getItem(VISTREL_PROFILE_KEY);
        if (!raw) return null;
        const saved = JSON.parse(raw);
        const pref = {};
        if (typeof saved.bpp === "number") pref.bpp = saved.bpp;
        if (typeof saved.fitMode === "string") pref.fitMode = saved.fitMode;
        return Object.keys(pref).length > 0 ? pref : null;
    } catch (_) {
        return null;
    }
}

/**
 * 状态点的简短文案，完整描述走 title 悬浮提示
 */
const STATUS_LABELS = {
    connecting: "连接中",
    connected: "已连接",
    disconnected: "重试中",
    closed: "已关闭",
};

/** 状态名到色点级别的映射：级别由 CSS 的 `.status-dot.is-*` 给出 */
const STATUS_LEVELS = {
    connecting: "is-info",
    connected: "is-ok",
    disconnected: "is-warn",
    closed: "is-dead",
};

/** codec 展示标签（HUD 徽标用） */
function codecLabel(codec) {
    if (codec === "av1") return "AV1";
    if (codec === "h265") return "H.265";
    if (codec === "h264") return "H.264";
    if (codec === "vp9") return "VP9";
    if (codec === "vp8") return "VP8";
    return "未知";
}

/** 编码后端展示标签 */
function backendLabel(backend) {
    if (backend === "nvenc") return "NVENC";
    if (backend === "vulkan_video") return "Vulkan Video";
    if (backend === "software") return "软编";
    return "--";
}

/**
 * Vistrel Web Client UI 与交互管理
 */
export class UIManager {
    constructor() {
        this.video = document.getElementById("remoteVideo");
        this.video.addEventListener("resize", () => {
            this.setStreamExtent(this.video.videoWidth, this.video.videoHeight);
        });
        this.audio = document.getElementById("remoteAudio");
        this.videoContainer = document.getElementById("video-container");
        // 输入汇聚点：边内 DOM 属游戏，边外一律视为页面 UI
        this.gameSurface = document.getElementById("game-surface");
        this.overlay = document.getElementById("overlay");
        this.statusDot = new StatusDot(document.getElementById("hudStatus"));
        this.setStatus("connecting");
        this.netDiag = document.getElementById("netDiag");
        // 指标行：标签格静态，值由调用方算好推入
        this.metrics = new MetricRow(document.getElementById("hudMetrics"), {
            quality: { label: "画质" },
            codec: { label: "Codec" },
            backend: { label: "编码后端" },
            rtt: { label: "RTT", value: "-- ms", minCh: 5 },
            fps: { label: "帧率", value: "-- FPS", minCh: 5 },
        });

        // 弹窗可见性向输入层推式通知：任一遮罩翻转后统一派生「是否有弹窗」，仅在整层翻转时通知
        this.modalChangeCallback = null;
        this.modalVisibleNotified = false;
        const onModalChange = () => this.syncModalVisibility();
        // 卡片壳层由 Modal 生成，表单与富文本段落留在 index.html 的模板里，克隆后作为 content 插入
        this.modals = {
            password: new Modal({
                id: "passwordModal",
                eyebrow: "Vistrel",
                title: "访问认证",
                desc: ["输入服务端配置的访问密码，以接入游戏画面与键鼠控制。"],
                content: this.cloneTemplate("passwordBodyTpl"),
                // 法律声明入口：非认证动作，用 bare 变体当文字链接放在卡片底部
                footer: new Button({
                    id: "aboutFromAuthBtn",
                    text: "关于 Vistrel 与法律声明",
                    variant: "bare",
                    size: "xs",
                    onClick: () => this.showAboutModal(),
                }).getElement(),
                actions: [{ label: "进入游戏", primary: true, onClick: () => this.submitPassword() }],
                onChange: onModalChange,
            }),
            kicked: new Modal({
                id: "kickedModal",
                eyebrow: "会话已中断",
                title: "连接已断开",
                desc: ["当前游戏会话已在其他窗口或设备接入。"],
                actions: [{ label: "重新连接", onClick: () => this.reconnect() }],
                actionsAlignCenter: true,
                onChange: onModalChange,
            }),
            about: new Modal({
                id: "aboutModal",
                eyebrow: "Vistrel",
                title: "关于与法律声明",
                content: this.cloneTemplate("aboutBodyTpl"),
                actions: [{ label: "关闭", onClick: () => this.hideAboutModal() }],
                closeOnBackdrop: true,
                onChange: onModalChange,
            }),
            settings: new Modal({
                id: "settingsModal",
                title: "设置",
                content: this.cloneTemplate("settingsBodyTpl"),
                actions: [{ label: "关闭", onClick: () => this.hideSettingsModal() }],
                closeOnBackdrop: true,
                onChange: onModalChange,
            }),
        };

        this.passwordSubmitHandler = null;
        this.reconnectHandler = null;
        this.passwordInput = document.getElementById("passwordInput");
        this.passwordError = document.getElementById("passwordError");
        this.submitPasswordBtn = this.modals.password.buttons[0];

        // 页面里的按钮统一包成 Button：状态、无障碍属性与就地提示都只在 button.js 里改 DOM
        this.hudActions = document.getElementById("hudActions");
        this.recordBtn = new Button({
            id: "recordBtn",
            text: "开始录制",
            tooltip: "录制",
            size: "sm",
            variant: "ghost",
        });
        this.clipBtn = new Button({ text: "回放", tooltip: "回放", size: "sm", variant: "ghost" });
        this.replayToggle = new Button({ id: "replayToggle" });
        this.settingsBtn = new Button({
            id: "settingsBtn",
            tooltip: "设置",
            size: "sm",
            variant: "ghost",
            shape: "square",
            icon: "/icons/settings.svg",
            onClick: () => this.showSettingsModal(),
        });
        this.settingsBtn.getElement().style.marginLeft = "auto";
        this.aboutBtn = new Button({
            tooltip: "关于",
            size: "sm",
            variant: "ghost",
            shape: "square",
            icon: "/icons/about.svg",
            onClick: () => this.showAboutModal(),
        });
        this.hudActions.append(
            this.recordBtn.getElement(),
            this.clipBtn.getElement(),
            this.settingsBtn.getElement(),
            this.aboutBtn.getElement(),
        );

        this.profileChangeCallback = null;

        // 档位表就是唯一事实源：数量、刻度文案、当前值标签全由 DiscreteSlider 派生，松手才提交
        // 滑条挂在设置弹窗卡片里（模板克隆时已插入卡片）。
        // 分辨率与帧率控件已退役：几何与帧率由游戏实例决定，客户端只保留 BPP 与显示适配
        const qualityHost = this.modals.settings.element.querySelector("#qualityControls");
        this.fitModeSlider = new DiscreteSlider({
            host: qualityHost,
            label: "显示模式",
            options: FIT_MODES,
            value: FIT_MODES[0], // 默认包含（letterbox）
            format: (mode) => mode.label,
            onCommit: () => this.applyFitMode(),
        });
        this.bppSlider = new DiscreteSlider({
            host: qualityHost,
            label: "画质档位",
            options: BPP_TIERS,
            value: BPP_TIERS[0], // 默认 0.10
            format: (bpp) => bpp.toFixed(2),
            tickLabel: (bpp) => (bpp === BPP_TIERS[0] || bpp === BPP_TIERS[BPP_TIERS.length - 1] ? bpp.toFixed(2) : ""),
            onCommit: () => this.notifyProfileChange(),
        });
        this.restoreProfilePreference();
        this.initHudPanel();
    }

    initHudPanel() {
        this.hud = document.getElementById("hud");
        if (!this.hud) return;

        this.hudToggle = new Button({
            id: "hudToggle",
            icon: "/icons/collapse.svg",
            onClick: () => this.hudPanel.toggle(),
        });

        // 收起/展开的几何与轻点展开全归组件，这里只管按钮的点击入口
        this.hudPanel = new DraggablePanel({
            element: this.hud,
            storageKey: VISTREL_HUD_KEY,
        });
    }

    bindProfileChange(callback) {
        this.profileChangeCallback = callback;
    }

    notifyProfileChange() {
        this.saveProfilePreference();
        if (this.profileChangeCallback) {
            this.profileChangeCallback(this.bppSlider.value);
        }
    }

    /** 偏好跨会话保持：只有 BPP 与显示适配属于客户端；协商回执不覆盖它们 */
    saveProfilePreference() {
        try {
            localStorage.setItem(
                VISTREL_PROFILE_KEY,
                JSON.stringify({
                    bpp: this.curBpp,
                    fitMode: this.fitMode,
                }),
            );
        } catch (_) {
            // 隐私模式下写入失败，仅本次会话内生效
        }
    }

    /** 恢复上次的偏好；存档缺失或形状不符保持默认 */
    restoreProfilePreference() {
        const saved = readProfilePreference();
        if (!saved) return;
        if (typeof saved.bpp === "number") this.selectBpp(saved.bpp);
        if (typeof saved.fitMode === "string") {
            const mode = FIT_MODES.find((m) => m.value === saved.fitMode);
            if (mode) this.fitModeSlider.select(mode);
        }
        this.applyFitMode();
    }

    /**
     * HUD 画质徽标：实际到达的流分辨率（视频元素事实，非服务端配置值），
     * 协商前 / 无帧时为占位符。实测帧率与编解码器由 stats 行另行展示
     */
    setStreamExtent(width, height) {
        if (width > 0 && height > 0) {
            this.metrics.quality.set(`${width}×${height}`);
        } else {
            this.metrics.quality.set("--");
        }
    }

    get fitMode() {
        return this.fitModeSlider.value.value;
    }

    get curBpp() {
        return this.bppSlider.value;
    }

    /** 显示适配只改浏览器内呈现（object-fit），绝不触达编码帧 */
    applyFitMode() {
        if (this.video) {
            this.video.style.objectFit = this.fitMode;
        }
        this.saveProfilePreference();
    }

    /**
     * 视频显示区视口的物理像素上限（CSS 尺寸 × DPR 在源头算好，服务端零换算）。
     * 取视频元素的显示区而非窗口：letterbox 黑边不计入上限
     */
    videoViewport() {
        if (!this.video) return null;
        const rect = this.video.getBoundingClientRect();
        const dpr = window.devicePixelRatio || 1;
        const width = Math.round(rect.width * dpr);
        const height = Math.round(rect.height * dpr);
        if (width <= 0 || height <= 0) return null;
        return { width, height };
    }

    /** 回包/存档里的画质值按百分数就近对齐滑杆档位 */
    selectBpp(bpp) {
        if (typeof bpp !== "number") return;
        const hundredths = Math.round(bpp * 100);
        const option = BPP_TIERS.find((v) => Math.round(v * 100) === hundredths);
        if (option !== undefined) this.bppSlider.select(option);
    }

    setStatus(state) {
        this.statusDot.set(STATUS_LEVELS[state] || "is-info", STATUS_LABELS[state] || "未知");
    }

    setCodec(codec) {
        if (codec) this.metrics.codec.set(codecLabel(codec));
    }

    setBackend(backend) {
        if (backend) this.metrics.backend.set(backendLabel(backend));
    }

    setRtt(rttMs) {
        this.metrics.rtt.set(`${rttMs} ms`);
    }

    setFps(fps) {
        this.metrics.fps.set(`${fps} FPS`);
    }

    setNetDiag(text) {
        if (this.netDiag) {
            this.netDiag.innerText = text;
        }
    }

    /**
     * 锁定状态切换系统光标显隐：锁定后光标交给游戏画面，解锁后还给页面
     */
    updateSystemCursor(isLocked) {
        if (this.videoContainer) this.videoContainer.style.cursor = isLocked ? "none" : "default";
    }

    /**
     * 解锁提示（无 GUI 且指针未锁定）：常驻一条纯提示，点击穿透到画面即可回锁
     */
    setLockHint(visible) {
        if (visible) {
            pin(LOCK_HINT_TEXT);
        } else {
            unpin();
        }
    }

    /** 一次性动作反馈：默认 3 秒自动收起 */
    showToast(text) {
        toast(text);
    }

    /** 克隆 index.html 里的 <template>：节点交给组件插入，作者仍在 HTML 里写结构 */
    cloneTemplate(id) {
        const template = document.getElementById(id);
        return template ? template.content.cloneNode(true) : null;
    }

    showPasswordModal(errorMsg) {
        this.modals.password.show();
        if (this.passwordInput) this.passwordInput.value = "";
        if (this.passwordError) {
            if (errorMsg) {
                this.passwordError.innerText = errorMsg;
                this.passwordError.style.display = "block";
            } else {
                this.passwordError.style.display = "none";
                this.passwordError.innerText = "";
            }
        }
        if (this.submitPasswordBtn) {
            this.submitPasswordBtn.setDisabled(false);
            this.submitPasswordBtn.setText("进入游戏");
        }
        setTimeout(() => {
            if (this.passwordInput) this.passwordInput.focus();
        }, 50);
    }

    hidePasswordModal() {
        this.modals.password.hide();
        if (this.passwordError) {
            this.passwordError.style.display = "none";
            this.passwordError.innerText = "";
        }
    }

    showSettingsModal() {
        this.modals.settings.show();
    }

    showAboutModal() {
        this.modals.about.show();
    }

    hideAboutModal() {
        this.modals.about.hide();
    }

    hideSettingsModal() {
        this.modals.settings.hide();
    }
    bindPasswordSubmit(onSubmit) {
        this.passwordSubmitHandler = onSubmit;
        if (this.passwordInput) {
            this.passwordInput.addEventListener("keydown", (e) => {
                if (e.key === "Enter") this.submitPassword();
            });
        }
    }

    /** 密码提交：空值就地报错，否则按钮挂起并交给外部提交 */
    submitPassword() {
        const code = this.passwordInput ? this.passwordInput.value.trim() : "";
        if (!code) {
            if (this.passwordError) {
                this.passwordError.innerText = "请输入访问密码";
                this.passwordError.style.display = "block";
            }
            return;
        }
        if (this.submitPasswordBtn) {
            this.submitPasswordBtn.setDisabled(true);
            this.submitPasswordBtn.setText("连接中...");
        }
        if (this.passwordSubmitHandler) this.passwordSubmitHandler(code);
    }

    bindReconnect(onReconnect) {
        this.reconnectHandler = onReconnect;
    }

    /** 重新连接：先收起断线弹窗再交给外部重建会话 */
    reconnect() {
        this.modals.kicked.hide();
        if (this.reconnectHandler) this.reconnectHandler();
    }

    setRecordAvailability(supported, connected) {
        this.recordBtn.setDisabled(!(supported && connected));
        this.recordBtn.setTooltip(!supported ? "当前浏览器不支持 MP4 录制" : !connected ? "连接建立后可录制" : "");
    }

    setRecordState(active, text) {
        this.recordBtn.setText(active ? "停止录制" : "开始录制");
        this.recordBtn.setActive(active);
        // 录制时长与体积留在按钮悬浮提示上；禁用态的提示语由 setRecordAvailability 管
        if (active) this.recordBtn.setTooltip(text ? `停止录制并保存（${text}）` : "停止录制并保存");
        else if (!this.recordBtn.disabled) this.recordBtn.setTooltip("");
    }

    bindRecordToggle(onToggle) {
        this.recordBtn.bindClick(() => onToggle());
    }

    /* ---------------- 回放剪辑 ---------------- */

    setReplayToggle(on) {
        this.replayToggle.setActive(on);
    }

    bindReplayToggle(onToggle) {
        this.replayToggle.bindClick(() => onToggle());
    }

    bindClipSave(onSave) {
        this.clipBtn.bindClick(() => onSave());
    }

    setClipAvailability(active, reason) {
        this.clipBtn.setDisabled(!active);
        this.clipBtn.setTooltip(reason || "保存最近 30 秒画面与声音为 MP4");
    }

    /** 任一遮罩可见即视为有弹窗：每次现算，读取不依赖缓存 */
    isAnyModalVisible() {
        return Object.values(this.modals).some((modal) => modal.visible);
    }

    /** 遮罩自身翻转后统一派生整层状态，仅在真实翻转时通知输入层一次 */
    syncModalVisibility() {
        const anyVisible = this.isAnyModalVisible();
        if (anyVisible === this.modalVisibleNotified) return;
        this.modalVisibleNotified = anyVisible;
        if (this.modalChangeCallback) this.modalChangeCallback(anyVisible);
    }

    /** 订阅弹窗可见性：订阅即回放当前状态，输入层无需另行初始化 */
    bindModalChange(callback) {
        this.modalChangeCallback = callback;
        this.modalVisibleNotified = this.isAnyModalVisible();
        if (callback) callback(this.modalVisibleNotified);
    }

    /* ---------------- 游戏实例推送事件弹窗 ---------------- */

    /**
     * 展示游戏实例推送的事件卡片（默认顶部居中）：新卡片替换旧卡片，超时自动消退
     * 复制结果用按钮文案就地反馈，卡片内容不因复制而消失
     *
     * @param {string} kind 'url' 链接+打开；'clipboard' 可整段选中的文本+复制
     * @param {string} value 链接地址 / 文本内容
     * @param {{title?: string, timeoutMs?: number}} [opts] 覆盖默认标题与展示时长（timeoutMs<=0 常驻）
     */
    showNoticeCard(kind, value, opts) {
        const options = opts || {};
        const timeoutMs = options.timeoutMs ?? 10000;
        if (kind === "url") {
            notice({
                title: options.title || "游戏内链接",
                text: value,
                timeoutMs,
                actions: this.openUrlAction(),
            });
            return;
        }
        notice({
            title: options.title || "复制到剪贴板",
            text: value,
            multiline: true,
            timeoutMs,
            actions: this.copyAction(),
        });
    }

    /** 打开链接：点击即真实 user gesture，被浏览器拦截时落到可手动复制的卡片 */
    openUrlAction() {
        return [
            {
                label: "打开链接",
                onClick: (_title, url) => {
                    if (!openExternalUrl(url)) {
                        this.showManualCopyCard(url);
                    }
                },
            },
        ];
    }

    /** 复制动作：目标文本由通知在回调时回传，这里不闭包捕获内容 */
    copyAction() {
        return [
            {
                label: "复制",
                onClick: (_title, text) =>
                    copyTextWithFallback(
                        text,
                        () => this.showToast("已复制到浏览器剪贴板"),
                        () => this.showManualCopyCard(text),
                    ),
            },
        ];
    }

    /** 两条写入路径都不可行：常驻卡片，让用户在只读文本框里手动选中 */
    showManualCopyCard(text) {
        notice({
            title: "复制失败，请手动选中复制",
            text,
            multiline: true,
            timeoutMs: 0,
        });
    }
}
