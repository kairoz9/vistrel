import { HID_KEY_MAP, INPUT_PACKET_TYPE } from "./protocol.js";

/**
 * 归一化坐标收拢到 [0, 1]
 */
function clamp01(value) {
    return Math.min(1, Math.max(0, value));
}

/**
 * 可打印单字符且非组合快捷键；AltGr (Ctrl+Alt 同按) 视为字符输入
 */
function isPrintableChar(e) {
    if (!e.key || e.key.length !== 1) return false;
    if (e.metaKey) return false;
    if (e.ctrlKey && e.altKey) return true;
    return !e.ctrlKey && !e.altKey;
}

/**
 * 事件目标属于页面自身的可编辑元素：输入权归浏览器（连接页口令框、弹窗输入等）
 */
function isPageEditableTarget(target) {
    return (
        target instanceof HTMLElement &&
        target.id != "imeCollector" &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable)
    );
}

// 输入汇聚点（#game-surface）之外的事件一律不进游戏：UI 只要不在汇聚点内即被天然拦截，输入层不持有任何选择器

// 触摸手势参数：轻点判定与双指滚轮步进
const TAP_SLOP_PX = 10;
const TAP_MAX_MS = 250;
const WHEEL_TICK_PX = 40;
// 触摸结束后的时间窗：窗内合成 click 不触发指针锁定
const TOUCH_CLICK_SUPPRESS_MS = 500;
// 解锁提示延迟：转瞬即锁（回锁成功、开关 GUI）不闪提示
const UNLOCK_HINT_DELAY_MS = 500;

const MODE_LOCKED = "locked";
const MODE_GUI = "gui";
const MODE_IDLE = "idle";

/**
 * Vistrel Web Client 输入控制器
 * 处理 Pointer Lock、鼠标相对位移、鼠标按键、滚轮与键盘扫描码打包传输；
 * 游戏实例 GUI 态程序性释放指针锁，系统光标位置经绝对坐标 (0x05) 透传
 */
export class InputController {
    /**
     * @param {object} options
     * @param {HTMLElement} options.targetElement 输入汇聚点 DOM（如 #game-surface）：指针锁定载体，且汇聚点外的输入一律丢弃
     * @param {HTMLElement} options.viewportElement 显示视口 DOM（#videoViewport）：GUI 态透传坐标按它的矩形归一化
     * @param {() => RTCDataChannel | null} options.getDataChannel 获取活动 input DataChannel 的提供者
     * @param {(text: string) => void} options.sendTextInput 输入文本下发回调（经信令 WS）
     * @param {() => void} options.onUserInteraction 用户交互触发回调 (例如解除音频静音)
     * @param {(isLocked: boolean) => void} options.onLockChange 锁定状态变动通知
     * @param {(visible: boolean) => void} options.onUnlockHint 解锁提示显隐通知
     */
    constructor({
        targetElement,
        viewportElement,
        getDataChannel,
        sendTextInput,
        onUserInteraction,
        onLockChange,
        onUnlockHint,
    }) {
        this.target = targetElement;
        this.viewport = viewportElement;
        this.getDataChannel = getDataChannel;
        this.sendTextInput = sendTextInput;
        this.onUserInteraction = onUserInteraction;
        this.onLockChange = onLockChange;

        this.isLocked = false;
        this.pressedKeys = new Set();

        // GUI 态标志：来自游戏实例侧 cursor_state 推送，true=有界面（程序性释放指针锁改用系统光标）
        this.cursorVisible = false;
        // 本次退锁由本模块主动发起（GUI 打开）：抑制退锁补发 ESC，否则界面会被自己的合成 ESC 关掉
        this.ownExitRequested = false;
        // 弹窗可见性：由 UI 推式通知（唯一事实源在 UI 层，输入层不逐事件回查）
        this.modalOpen = false;
        // 按下起点落在 UI 上：本次拖拽直到抬起为止都不向游戏透传（弹窗/HUD 内拖动不得带走游戏光标）
        this.uiPointerHeld = false;
        // 已下发给游戏的鼠标键：抬起必须成对补发，哪怕停在 UI 上
        this.sentButtons = new Set();

        // 解锁提示：解锁态持续满 UNLOCK_HINT_DELAY_MS 且无 GUI 才提示点击回锁
        this.onUnlockHint = onUnlockHint;
        this.unlockHintTimer = null;
        this.unlockHintShown = false;

        // IME 收集器：只做组合文本载体不存状态，MC 的 EditBox 是唯一事实源
        this.imeCollector = document.getElementById("imeCollector");
        this.composing = false;

        // 最近一次 ESC 按下/抬起时刻：回锁必须避开 ESC 手势窗口（见 attemptRelockAfterEsc）
        this.escDownAt = 0;
        this.escUpAt = 0;
        // 最近一次触摸结束时刻：轻点合成的 click 不得触发指针锁定
        this.lastTouchEndAt = 0;

        // 单指手势（游戏态转视角 / GUI 态光标绝对跟随），双指滚轮模式
        this.touchGesture = null;
        this.touchWheel = null;

        this.initEventListeners();
        // 首帧即评估一次：未锁定又无 GUI 的初始态不会有事件触达，不主动评估就永远不提示
        this.syncUnlockHint();
    }

    send(buffer) {
        const channel = this.getDataChannel();
        if (channel && channel.readyState === "open") {
            channel.send(buffer);
        }
    }

    releaseAllPressedKeys() {
        for (const code of this.pressedKeys) {
            this.sendKeyboard(code, 0);
        }
        this.pressedKeys.clear();
    }

    /**
     * 事件目标是否落在输入汇聚点内：指针锁期间目标恒为锁元素（部分浏览器报 body），故以锁元素为准
     */
    inSurface(target) {
        const owner = document.pointerLockElement || this.target;
        return target instanceof Node && (target === owner || owner.contains(target));
    }

    /**
     * 键盘目标归属：汇聚点内的编辑元素（IME 收集器）属游戏；无焦点时事件落在 body/document 上同样属游戏
     */
    ownsKeyTarget(target) {
        if (target === document.body || target === document.documentElement) return true;
        return this.inSurface(target);
    }

    /**
     * 输入模式：由双权威派生，不另存枚举副本
     * locked=游戏接管鼠标；gui=解锁且游戏开gui（键码与绝对坐标透传）；idle=解锁无界面，属页面态
     */
    mode() {
        if (this.isLocked) return MODE_LOCKED;
        return this.cursorVisible ? MODE_GUI : MODE_IDLE;
    }

    /**
     * 主动交出指针锁：退锁只有一个发起方，ownExitRequested 仅在此处置位
     */
    releaseLock() {
        if (!this.isLocked) return;
        this.ownExitRequested = true;
        document.exitPointerLock();
    }

    /**
     * 弹窗可见性变更（UI 推式）：弹窗期间焦点归页面，IME 收集器必须让位
     */
    setModalOpen(visible) {
        this.modalOpen = visible;
        this.syncImeFocus();
    }

    /**
     * 游戏实例侧光标状态推送入口：true=GUI 打开（锁定中则程序性释放指针锁改用系统光标），
     * false=GUI 关闭（解锁中则借免手势宽限尝试自动回锁）
     */
    setCursorState(visible) {
        const wasVisible = this.cursorVisible;
        this.cursorVisible = visible;
        if (visible && !wasVisible) {
            // GUI 打开：主动交出指针锁改用系统光标
            this.releaseLock();
        }
        // GUI 关闭且处于解锁态（如点了"回到游戏"或刚转发过 ESC）：自动回锁
        if (wasVisible && !visible && !this.isLocked) {
            this.attemptRelockAfterEsc();
        }
        this.syncUnlockHint();
        this.syncImeFocus();
    }

    /**
     * 解锁提示同步：解锁态且无 GUI 才提示点击画面回锁（GUI 态的锁本就交还系统光标）；
     * 已起表或已提示则不重复起表，避免连续状态推送把倒计时反复重置。
     * 弹窗不参与判定：模态层 z-index 本就盖住提示，且弹窗关闭没有对应事件可回灌重评
     */
    syncUnlockHint() {
        if (!this.onUnlockHint) return;
        const shouldHint =
            this.mode() === MODE_IDLE &&
            // 触摸设备无指针锁语义（出现过触摸即永久豁免）
            this.lastTouchEndAt === 0;
        if (!shouldHint) {
            if (this.unlockHintTimer) {
                clearTimeout(this.unlockHintTimer);
                this.unlockHintTimer = null;
            }
            if (this.unlockHintShown) {
                this.unlockHintShown = false;
                this.onUnlockHint(false);
            }
            return;
        }
        if (this.unlockHintTimer || this.unlockHintShown) return;
        this.unlockHintTimer = setTimeout(() => {
            this.unlockHintTimer = null;
            this.unlockHintShown = true;
            this.onUnlockHint(true);
        }, UNLOCK_HINT_DELAY_MS);
    }

    /**
     * GUI 态把焦点交给 IME 收集器（文本输入载体），其余状态交还页面
     */
    syncImeFocus() {
        if (!this.imeCollector) return;
        if (this.mode() === MODE_GUI && !this.modalOpen) {
            this.imeCollector.focus({ preventScroll: true });
        } else {
            this.imeCollector.blur();
        }
    }

    /**
     * GUI 态透传：系统光标位置换算为归一化坐标下发。
     *
     * 视口矩形只承载显示区域（显示适配已落到其布局尺寸），与游戏窗口一对一映射，
     * 三种模式共用一条公式；落在 letterbox 黑边上的坐标按边缘钳制。
     */
    sendAbsoluteMousePosition(clientX, clientY) {
        if (!this.viewport) return;
        const rect = this.viewport.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return;
        const normX = (clientX - rect.left) / rect.width;
        const normY = (clientY - rect.top) / rect.height;
        this.sendAbsolutePosition(clamp01(normX), clamp01(normY));
    }

    sendAbsolutePosition(normX, normY) {
        const buf = new ArrayBuffer(5);
        const view = new DataView(buf);
        view.setUint8(0, INPUT_PACKET_TYPE.ABSOLUTE_MOUSE_MOVE);
        view.setUint16(1, Math.round(normX * 65535), false);
        view.setUint16(3, Math.round(normY * 65535), false);
        this.send(buf);
    }

    sendMouseButton(button, state) {
        const buf = new ArrayBuffer(3);
        const view = new DataView(buf);
        view.setUint8(0, INPUT_PACKET_TYPE.MOUSE_BUTTON);
        view.setUint8(1, button);
        view.setUint8(2, state);
        this.send(buf);
    }

    sendMouseMove(dx, dy) {
        const buf = new ArrayBuffer(5);
        const view = new DataView(buf);
        view.setUint8(0, INPUT_PACKET_TYPE.MOUSE_MOVE);
        view.setInt16(1, dx, false);
        view.setInt16(3, dy, false);
        this.send(buf);
    }

    sendWheel(sy) {
        const buf = new ArrayBuffer(2);
        const view = new DataView(buf);
        view.setUint8(0, INPUT_PACKET_TYPE.MOUSE_WHEEL);
        view.setInt8(1, sy);
        this.send(buf);
    }

    /** 键盘输入帧：state 0=抬起 1=按下 2=自动重复 */
    sendKeyboard(code, state) {
        const buf = new ArrayBuffer(6);
        const view = new DataView(buf);
        view.setUint8(0, INPUT_PACKET_TYPE.KEYBOARD);
        view.setInt32(1, code, false);
        view.setUint8(5, state);
        this.send(buf);
    }

    /* ---------------- 触摸手势（触控板遥控，仅 pointerType==='touch' 事件驱动） ---------------- */

    /**
     * 触摸按下：记单指手势；GUI 态光标立即跳到触点（绝对直映，手指即指针）；
     * 第二指落下则终结当前手势转入双指滚轮
     */
    handleTouchDown(e) {
        if (this.touchWheel) return;
        if (this.touchGesture) {
            const first = this.touchGesture;
            this.touchGesture = null;
            this.touchWheel = {
                ids: new Set([first.pointerId, e.pointerId]),
                ys: new Map([
                    [first.pointerId, first.lastY],
                    [e.pointerId, e.clientY],
                ]),
                lastMidY: (first.lastY + e.clientY) / 2,
                accum: 0,
            };
            return;
        }
        this.touchGesture = {
            pointerId: e.pointerId,
            lastX: e.clientX,
            lastY: e.clientY,
            startX: e.clientX,
            startY: e.clientY,
            startT: performance.now(),
            moved: false,
        };
        if (this.cursorVisible) {
            this.sendAbsoluteMousePosition(e.clientX, e.clientY);
        }
    }

    /**
     * 触摸移动：双指滚轮按两指中点位移攒步进；单指在游戏态喂相对位移转视角、
     * GUI 态光标绝对跟随手指
     */
    handleTouchMove(e) {
        if (this.touchWheel) {
            if (!this.touchWheel.ids.has(e.pointerId)) return;
            this.touchWheel.ys.set(e.pointerId, e.clientY);
            const ys = [...this.touchWheel.ys.values()];
            const midY = (ys[0] + ys[1]) / 2;
            this.touchWheel.accum += midY - this.touchWheel.lastMidY;
            this.touchWheel.lastMidY = midY;
            while (Math.abs(this.touchWheel.accum) >= WHEEL_TICK_PX) {
                const sy = this.touchWheel.accum < 0 ? 1 : -1;
                this.sendWheel(sy);
                this.touchWheel.accum -= Math.sign(this.touchWheel.accum) * WHEEL_TICK_PX;
            }
            return;
        }
        const g = this.touchGesture;
        if (!g || e.pointerId !== g.pointerId) return;
        if (!g.moved && Math.hypot(e.clientX - g.startX, e.clientY - g.startY) > TAP_SLOP_PX) {
            g.moved = true;
        }
        if (this.cursorVisible) {
            this.sendAbsoluteMousePosition(e.clientX, e.clientY);
        } else {
            const dx = Math.max(-32768, Math.min(32767, e.clientX - g.lastX));
            const dy = Math.max(-32768, Math.min(32767, e.clientY - g.lastY));
            if (dx !== 0 || dy !== 0) this.sendMouseMove(dx, dy);
        }
        g.lastX = e.clientX;
        g.lastY = e.clientY;
    }

    /**
     * 触摸结束：轻点（未超位移且够快）= 左键 down/up 成对；pointercancel 不算轻点
     */
    handleTouchEnd(e, cancelled) {
        if (this.touchWheel) {
            this.touchWheel.ys.delete(e.pointerId);
            this.touchWheel.ids.delete(e.pointerId);
            if (this.touchWheel.ids.size < 2) this.touchWheel = null;
            return;
        }
        const g = this.touchGesture;
        if (!g || e.pointerId !== g.pointerId) return;
        this.touchGesture = null;
        this.lastTouchEndAt = Date.now();
        this.syncUnlockHint();
        if (cancelled) return;
        const dt = performance.now() - g.startT;
        const dist = Math.hypot(e.clientX - g.startX, e.clientY - g.startY);
        if (!g.moved && dist < TAP_SLOP_PX && dt < TAP_MAX_MS) {
            this.sendMouseButton(0, 1);
            this.sendMouseButton(0, 0);
        }
    }

    /** 键码 down/up 成对下发：轻按语义，退锁补发 ESC 复用 */
    tapKeyCode(code) {
        this.sendKeyboard(code, 1);
        setTimeout(() => this.sendKeyboard(code, 0), 50);
    }

    /**
     * 尝试重新锁定指针：无手势或冷却期内会失败，静默回落到点击回锁
     */
    attemptRelock() {
        if (this.isLocked || !this.target.requestPointerLock) return;
        this.requestLock(true);
    }

    /**
     * 实际申请锁定：raw 为真时附带 unadjustedMovement 选项绕开 OS 鼠标加速拿原始物理增量。
     * 平台不支持原始输入的拒绝按 NotSupportedError 降级重试普通锁定
     * （Firefox/Safari 忽略该选项直接锁定，无需降级）；冷却期等其他失败保持静默
     */
    requestLock(raw) {
        try {
            const options = raw ? { unadjustedMovement: true } : undefined;
            const result = this.target.requestPointerLock(options);
            if (result && typeof result.catch === "function") {
                result.catch((e) => {
                    if (raw && e && e.name === "NotSupportedError") {
                        this.requestLock(false);
                    }
                });
            }
        } catch (_) {
            // 部分浏览器同步抛错同样静默
        }
    }

    /**
     * GUI 关闭后的自动回锁：必须避开 ESC 手势窗口——Chrome 会把窗口内（按住未抬/刚抬起 150ms 内）
     * 获取的锁立即撤销，撤销引发的退锁还会连锁补发 ESC 误开暂停，故等抬起落定后再锁
     */
    attemptRelockAfterEsc() {
        const escInFlight = this.escDownAt > this.escUpAt;
        const sinceUp = Date.now() - this.escUpAt;
        if (!escInFlight && sinceUp >= 155) {
            this.attemptRelock();
            return;
        }
        setTimeout(() => this.attemptRelockAfterEsc(), escInFlight ? 155 : 155 - sinceUp);
    }

    /**
     * GUI 态 Ctrl/Cmd+V：粘贴由 paste 事件接管，原始键码不下发。
     * grab 态无 GUI，vanilla 不响应 Ctrl+V，照旧透传
     */
    isPasteShortcut(e) {
        return this.cursorVisible && (e.ctrlKey || e.metaKey) && !(e.ctrlKey && e.altKey) && e.code === "KeyV";
    }

    initEventListeners() {
        // 只屏蔽游戏面内的右键菜单：UI（文本框、可选中文本、链接）保留原生菜单
        window.addEventListener("contextmenu", (e) => {
            if (this.inSurface(e.target)) e.preventDefault();
        });

        // 窗口失焦时，释放所有正在按下的键并终止 IME 组合状态
        window.addEventListener("blur", () => {
            this.releaseAllPressedKeys();
            this.composing = false;
            this.escUpAt = Date.now(); // 按住的 ESC 视为已抬起，避免回锁等待空转
            this.touchGesture = null;
            this.touchWheel = null;
            this.uiPointerHeld = false;
            if (this.imeCollector) this.imeCollector.value = "";
        });

        // PointerLock 状态变更监听
        document.addEventListener("pointerlockchange", () => {
            const wasLocked = this.isLocked;
            this.isLocked =
                document.pointerLockElement === this.target || document.pointerLockElement === document.body;
            if (this.onLockChange) {
                this.onLockChange(this.isLocked);
            }
            this.syncImeFocus();

            if (this.isLocked) {
                // 重新上锁：GUI 已关闭，清掉退锁发起标记
                this.ownExitRequested = false;
                // UI 拖拽不可能在锁定态继续，防止丢失的抬起把阻断标志卡住
                this.uiPointerHeld = false;
            } else if (wasLocked) {
                if (this.ownExitRequested) {
                    // 主动退锁（GUI 打开）：游戏没收到 ESC，不补发
                    this.ownExitRequested = false;
                } else {
                    // 浏览器退锁（ESC/失焦）：补发 ESC 让游戏同步开暂停
                    this.tapKeyCode(HID_KEY_MAP["Escape"]);
                }
            }

            if (!this.isLocked) {
                this.releaseAllPressedKeys();
            }
            this.syncUnlockHint();
        });

        // 点击画面请求锁定
        this.target.addEventListener("click", (e) => {
            if (this.onUserInteraction) this.onUserInteraction();
            // 触摸轻点合成的 click 不触发锁定：pointerdown 的 preventDefault 挡不住
            // click 合成，而锁定态会吞掉后续触摸移动事件（触屏笔记本实存路径）
            if (Date.now() - this.lastTouchEndAt < TOUCH_CLICK_SUPPRESS_MS) return;

            // GUI 态（含解锁透传）；仅游戏态点击画面才回锁
            if (this.mode() === MODE_IDLE) this.attemptRelock();
            e.preventDefault();
        });

        // 鼠标位移捕获：锁定视角走相对位移；GUI 态（程序性解锁）按系统光标实际位置透传
        window.addEventListener("mousemove", (e) => {
            // UI 上的移动与拖动不得透传：HUD/弹窗内移动会把游戏 GUI 光标一并带走
            if (this.uiPointerHeld || !this.inSurface(e.target)) return;
            const mode = this.mode();
            if (mode === MODE_IDLE) return;
            if (mode === MODE_GUI) {
                this.sendAbsoluteMousePosition(e.clientX, e.clientY);
                return;
            }

            const dx = Math.max(-32768, Math.min(32767, e.movementX || 0));
            const dy = Math.max(-32768, Math.min(32767, e.movementY || 0));
            if (dx === 0 && dy === 0) return;
            this.sendMouseMove(dx, dy);
        });

        // 鼠标按下
        window.addEventListener("mousedown", (e) => {
            if (this.onUserInteraction) this.onUserInteraction();
            // 起点落在汇聚点外（UI）：不抢锁也不注入，整个拖拽周期保持阻断（滑块的按下不得被锁定打断）
            if (!this.inSurface(e.target)) {
                this.uiPointerHeld = true;
                return;
            }

            const mode = this.mode();
            // 解锁无界面：点击只当回锁手势，不注入
            if (mode === MODE_IDLE) {
                this.attemptRelock();
                return;
            }

            // GUI 态点击：阻断默认聚焦转移，保护 IME 收集器焦点（游戏态无系统光标可聚焦）
            if (mode === MODE_GUI) e.preventDefault();

            this.sendMouseButton(e.button, 1);
            this.sentButtons.add(e.button);
        });

        // 鼠标释放
        window.addEventListener("mouseup", (e) => {
            const held = this.uiPointerHeld;
            this.uiPointerHeld = false;
            // 已下发过按下就必须补发抬起（哪怕停在 UI 上），否则游戏侧会卡在按住态
            if (this.sentButtons.delete(e.button)) {
                this.sendMouseButton(e.button, 0);
                return;
            }
            if (held || !this.inSurface(e.target)) return;
            if (this.mode() === MODE_IDLE) return;

            this.sendMouseButton(e.button, 0); // 0: Up
        });

        // 自动回锁失败（无手势/冷却期）属预期，静默避免控制台报错
        document.addEventListener("pointerlockerror", () => {});

        // 滚轮捕获 (sy)
        window.addEventListener(
            "wheel",
            (e) => {
                // 汇聚点外的滚动不得切换游戏快捷栏
                if (this.uiPointerHeld || !this.inSurface(e.target)) return;
                if (this.mode() === MODE_IDLE) return;

                const sy = e.deltaY < 0 ? 1 : -1;
                this.sendWheel(sy);
            },
            { passive: true },
        );

        // 触摸手势分流：pointerType==='touch' 的事件走手势状态机，桌面鼠标路径零感知；
        // preventDefault 压掉合成鼠标事件与浏览器触摸默认行为
        window.addEventListener("pointerdown", (e) => {
            if (e.pointerType !== "touch" || this.modalOpen) return;
            if (this.onUserInteraction) this.onUserInteraction();
            if (!this.inSurface(e.target)) return;
            e.preventDefault();
            this.handleTouchDown(e);
        });
        window.addEventListener("pointermove", (e) => {
            if (e.pointerType !== "touch" || this.modalOpen) return;
            e.preventDefault();
            this.handleTouchMove(e);
        });
        window.addEventListener("pointerup", (e) => {
            if (e.pointerType !== "touch") return;
            // 弹窗中途打开时手势必须照常清场：残留会被下次按下误判成第二指
            if (this.modalOpen) {
                this.touchGesture = null;
                this.touchWheel = null;
                return;
            }
            this.handleTouchEnd(e, false);
        });
        window.addEventListener("pointercancel", (e) => {
            if (e.pointerType !== "touch") return;
            this.handleTouchEnd(e, true);
        });

        // 键盘按下：锁定态全部键码直通；解锁但游戏侧 GUI 开着时同样透传；GUI 态可打印字符叠加文本通道
        window.addEventListener("keydown", (e) => {
            if (this.modalOpen) return;
            // 焦点落在汇聚点外（HUD 控件等）时键码归属 UI，不注入游戏
            if (!this.ownsKeyTarget(e.target)) return;
            // F11 归浏览器切换全屏，不拦截也不下发（游戏侧全屏会改动采集窗口）
            if (e.code === "F11") return;

            if (e.code === "Escape") {
                this.escDownAt = Date.now();
                // ESC 是开关型按键：自动重复会反复开关界面（长按退全屏时最明显），只认首次按下
                if (e.repeat) return;
            }

            const mode = this.mode();
            // 解锁无界面：键盘属于页面；GUI 态继续透传（暂停菜单/背包仍可操作）
            if (mode === MODE_IDLE) return;
            // GUI 态不转发自动重复：长按只应由首个按下生效
            if (mode === MODE_GUI && e.repeat) return;

            // IME 组合中的按键已由输入法消费，避免键码与输入文本双份注入
            if (this.composing || e.isComposing) return;

            // GUI 态粘贴快捷键不发原始键码：粘贴文本经 paste 监听走文本通道
            if (this.isPasteShortcut(e)) return;

            // F 前缀精确匹配 F1-F12：宽匹配会误伤 KeyF 并阻断其进入 IME 收集器
            if (
                e.code === "Tab" ||
                /^F\d/.test(e.code) ||
                e.code === "ContextMenu" ||
                e.code === "AltLeft" ||
                e.code === "AltRight"
            ) {
                e.preventDefault();
            }
            const code = HID_KEY_MAP[e.code];
            if (code === undefined) return;
            this.pressedKeys.add(code);
            this.sendKeyboard(code, e.repeat ? 2 : 1);

            // GUI 态可打印字符：等效本地 SDL 的 TEXTINPUT 事件（EditBox 只吃字符事件插入文本）
            if (this.cursorVisible && isPrintableChar(e)) {
                this.sendTextInput(e.key);
            }
        });

        // 键盘抬起
        window.addEventListener("keyup", (e) => {
            if (this.modalOpen) return;
            // 与 keydown 对齐：汇聚点外抬起不补发
            if (!this.ownsKeyTarget(e.target)) return;
            // 与 keydown 对齐：F11 不下发，避免孤儿抬起
            if (e.code === "F11") return;
            if (e.code === "Escape") this.escUpAt = Date.now();
            // 与 keydown 对齐：解锁无界面时不发；但该键曾下发给游戏就必须补发抬起，避免卡键
            if (this.mode() === MODE_IDLE && !this.pressedKeys.has(HID_KEY_MAP[e.code])) return;
            // 组合中的抬起（确认候选那一下）同样不转发，避免孤儿释放
            if (this.composing || e.isComposing) return;

            // 与 keydown 的粘贴吞键对齐：吞键未锁定态按下 V 抬起避免孤儿释放；锁定态按下照常补发防卡键
            if (this.isPasteShortcut(e) && !this.pressedKeys.has(HID_KEY_MAP[e.code])) return;

            const code = HID_KEY_MAP[e.code];
            if (code === undefined) return;
            this.pressedKeys.delete(code);
            this.sendKeyboard(code, 0);
        });

        // 反向剪贴板：串流中把浏览器粘贴文本透传进游戏 GUI 文本框（逐字注入，字符验收归 vanilla）。
        // 页面输入元素与未串流放行原生粘贴；剪贴板无文本时静默
        document.addEventListener("paste", (e) => {
            if (this.modalOpen) return;
            if (isPageEditableTarget(e.target)) return;
            const channel = this.getDataChannel();
            if (!channel || channel.readyState !== "open") return;
            const text = e.clipboardData ? e.clipboardData.getData("text/plain") : "";
            if (!text) return;
            e.preventDefault();
            this.sendTextInput(text);
        });

        // IME 组合生命周期：组合中抑制键码转发，确认后的整段文本经文本通道注入后立即清空
        if (this.imeCollector) {
            this.imeCollector.addEventListener("compositionstart", () => {
                this.composing = true;
            });
            this.imeCollector.addEventListener("compositionend", (e) => {
                this.composing = false;
                if (e.data) {
                    this.sendTextInput(e.data);
                }
                this.imeCollector.value = "";
            });
        }
    }
}
