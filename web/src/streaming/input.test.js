import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { InputController } from "./input.js";

// ---------- 测试侧硬编码帧解码器：不 import 生产编码实现，编码漂移在此炸断 ----------

function decodePacket(buffer) {
    const view = new DataView(buffer);
    const type = view.getUint8(0);
    switch (type) {
        case 0x01: // 相对位移：Int16BE dx/dy
            return { type, dx: view.getInt16(1, false), dy: view.getInt16(3, false) };
        case 0x02: // 按键：button/state
            return { type, button: view.getUint8(1), state: view.getUint8(2) };
        case 0x03: // 键盘：Int32BE 码 + state
            return { type, code: view.getInt32(1, false), state: view.getUint8(5) };
        case 0x04: // 滚轮：Int8 sy
            return { type, sy: view.getInt8(1) };
        case 0x05: // 绝对坐标：Uint16BE 归一化 x/y
            return { type, x: view.getUint16(1, false), y: view.getUint16(3, false) };
        default:
            return { type };
    }
}

// ---------- 事件构造助手：关键字段实例级覆写，规避 dom 环境保真度差异 ----------

function mouseEvent(type, { x = 0, y = 0, movementX = 0, movementY = 0, button = 0 } = {}) {
    const e = new MouseEvent(type, { bubbles: true, cancelable: true, clientX: x, clientY: y, button });
    Object.defineProperty(e, "movementX", { value: movementX });
    Object.defineProperty(e, "movementY", { value: movementY });
    return e;
}

function touchEvent(type, { pointerId = 1, x = 0, y = 0 } = {}) {
    const e = new PointerEvent(type, { bubbles: true, cancelable: true, clientX: x, clientY: y, pointerId });
    Object.defineProperty(e, "pointerType", { value: "touch" });
    return e;
}

function mousePointerEvent(type, { x = 0, y = 0 } = {}) {
    const e = new PointerEvent(type, { bubbles: true, cancelable: true, clientX: x, clientY: y });
    Object.defineProperty(e, "pointerType", { value: "mouse" });
    return e;
}

function keyEvent(type, { code, key = "", repeat = false, ctrlKey = false, altKey = false, metaKey = false } = {}) {
    const e = new KeyboardEvent(type, { bubbles: true, cancelable: true, code, key, repeat, ctrlKey, altKey, metaKey });
    Object.defineProperty(e, "code", { value: code });
    Object.defineProperty(e, "key", { value: key });
    Object.defineProperty(e, "repeat", { value: repeat });
    Object.defineProperty(e, "ctrlKey", { value: ctrlKey });
    Object.defineProperty(e, "altKey", { value: altKey });
    Object.defineProperty(e, "metaKey", { value: metaKey });
    return e;
}

function wheelEvent({ deltaY = 120 } = {}) {
    return new WheelEvent("wheel", { bubbles: true, cancelable: true, deltaY });
}

function pasteEvent(target, text) {
    const e = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(e, "clipboardData", { value: { getData: (mime) => (mime === "text/plain" ? text : "") } });
    target.dispatchEvent(e);
    return e;
}

// ---------- 会话级上下文 ----------

let controller;
let target;
let content;
let imeCollector;
let packets;
let interactionSpy;
let lockSpy;
let textSpy;
let hintSpy;

/** 构造控制器：DOM 结构贴近 index.html（#game-surface > #videoViewport > video + imeCollector） */
function setup() {
    document.body.innerHTML = `
        <div id="game-surface">
            <div id="videoViewport"><video id="fakeVideo"></video></div>
            <input id="imeCollector" type="text" />
        </div>`;
    target = document.getElementById("game-surface");
    content = document.getElementById("videoViewport");
    imeCollector = document.getElementById("imeCollector");
    packets = [];
    // send 必须捕获数组本身而非模块级变量：旧控制器的监听器在整个文件生命周期内存活，
    // 捕获变量会让历史控制器的帧灌进当前测试的断言数组
    const ownPackets = packets;
    const send = (buffer) => ownPackets.push(decodePacket(buffer));
    controller = new InputController({
        targetElement: target,
        viewportElement: content,
        getDataChannel: () => ({ readyState: "open", send }),
        sendTextInput: textSpy,
        onUserInteraction: interactionSpy,
        onLockChange: lockSpy,
        onUnlockHint: hintSpy,
    });
    document.exitPointerLock = vi.fn();
    target.requestPointerLock = vi.fn();
    return controller;
}

/** 经 pointerlockchange 事件走真实锁定状态机 */
function setLocked(locked) {
    Object.defineProperty(document, "pointerLockElement", { configurable: true, value: locked ? target : null });
    document.dispatchEvent(new Event("pointerlockchange"));
}

/** GUI 态 = 解锁 + cursorVisible；独立函数保证先解锁再置位 */
function enterGuiMode() {
    setLocked(false);
    controller.setCursorState(true);
}

/** 桩显示视口矩形：输入层只认可见视口（此处为 1600×900 内容 contain 到 800×600、纵向偏移 75） */
function setViewportRect({ left = 0, top = 75, width = 800, height = 450 } = {}) {
    content.getBoundingClientRect = () => ({ left, top, width, height });
}

const flush = async () => {
    for (let i = 0; i < 10; i++) await Promise.resolve();
};

beforeEach(() => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date", "performance"] });
    // document 级桩位跨测试残留会击穿 inSurface 判定，先归零
    Object.defineProperty(document, "pointerLockElement", { configurable: true, value: null });
    interactionSpy = vi.fn();
    lockSpy = vi.fn();
    textSpy = vi.fn();
    hintSpy = vi.fn();
});

afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
});

// ---------- 编码器字节面 ----------

describe("输入帧编码", () => {
    it("相对位移帧 0x01：Int16BE dx/dy", () => {
        setup();
        controller.sendMouseMove(-3, 700);
        expect(packets).toEqual([{ type: 1, dx: -3, dy: 700 }]);
    });

    it("按键帧 0x02：button/state 各一字节", () => {
        setup();
        controller.sendMouseButton(2, 1);
        expect(packets).toEqual([{ type: 2, button: 2, state: 1 }]);
    });

    it("键盘帧 0x03：Int32BE 码 + state", () => {
        setup();
        controller.sendKeyboard(41, 2);
        expect(packets).toEqual([{ type: 3, code: 41, state: 2 }]);
    });

    it("滚轮帧 0x04：Int8 sy", () => {
        setup();
        controller.sendWheel(-1);
        expect(packets).toEqual([{ type: 4, sy: -1 }]);
    });

    it("绝对坐标帧 0x05：Uint16BE 归一化坐标", () => {
        setup();
        controller.sendAbsolutePosition(0.5, 0.25);
        expect(packets).toEqual([{ type: 5, x: 32768, y: 16384 }]);
    });

    it("通道未就绪时静默丢弃", () => {
        document.body.innerHTML = '<div id="game-surface"></div>';
        target = document.getElementById("game-surface");
        controller = new InputController({
            targetElement: target,
            viewportElement: document.createElement("div"),
            getDataChannel: () => ({ readyState: "connecting", send: vi.fn() }),
            sendTextInput: textSpy,
            onUserInteraction: interactionSpy,
            onLockChange: lockSpy,
            onUnlockHint: hintSpy,
        });
        controller.sendMouseMove(1, 1);
        expect(controller.getDataChannel().send).not.toHaveBeenCalled();
    });

    it("releaseAllPressedKeys 按下发顺序成对补发抬起", () => {
        setup();
        setLocked(true);
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyW", key: "w" }));
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyA", key: "a" }));
        packets.length = 0;
        controller.releaseAllPressedKeys();
        expect(packets).toEqual([
            { type: 3, code: 26, state: 0 },
            { type: 3, code: 4, state: 0 },
        ]);
    });
});

// ---------- 鼠标管线 ----------

describe("鼠标管线", () => {
    it("锁定态位移转相对帧", () => {
        setup();
        setLocked(true);
        target.dispatchEvent(mouseEvent("mousemove", { movementX: 5, movementY: 7 }));
        expect(packets).toEqual([{ type: 1, dx: 5, dy: 7 }]);
    });

    it("零位移不下发", () => {
        setup();
        setLocked(true);
        target.dispatchEvent(mouseEvent("mousemove", {}));
        expect(packets).toHaveLength(0);
    });

    it("位移钳制到 Int16 边界", () => {
        setup();
        setLocked(true);
        target.dispatchEvent(mouseEvent("mousemove", { movementX: 40_000, movementY: -40_000 }));
        expect(packets).toEqual([{ type: 1, dx: 32_767, dy: -32_768 }]);
    });

    it("锁定态汇聚点外位移不下发", () => {
        setup();
        setLocked(true);
        document.body.dispatchEvent(mouseEvent("mousemove", { movementX: 5, movementY: 5 }));
        expect(packets).toHaveLength(0);
    });

    it("GUI 态位移按显示视口归一化", () => {
        setup();
        setViewportRect();
        enterGuiMode();
        target.dispatchEvent(mouseEvent("mousemove", { x: 100, y: 165 }));
        expect(packets).toEqual([{ type: 5, x: 8192, y: 13107 }]);
    });

    it("GUI 态落在 letterbox 黑边上的坐标钳到画面边缘", () => {
        setup();
        setViewportRect();
        enterGuiMode();
        target.dispatchEvent(mouseEvent("mousemove", { x: 400, y: 10 }));
        expect(packets).toEqual([{ type: 5, x: 32_768, y: 0 }]);
        target.dispatchEvent(mouseEvent("mousemove", { x: 900, y: 300 }));
        expect(packets.at(-1)).toEqual({ type: 5, x: 65_535, y: 32_768 });
    });

    it("IDLE 态位移全静默", () => {
        setup();
        target.dispatchEvent(mouseEvent("mousemove", { movementX: 5, movementY: 5, x: 100, y: 100 }));
        expect(packets).toHaveLength(0);
    });

    it("GUI 态按下阻断默认行为并成对下发按键", () => {
        setup();
        enterGuiMode();
        const down = mouseEvent("mousedown");
        target.dispatchEvent(down);
        expect(down.defaultPrevented).toBe(true);
        expect(packets).toEqual([{ type: 2, button: 0, state: 1 }]);
        target.dispatchEvent(mouseEvent("mouseup"));
        expect(packets.at(-1)).toEqual({ type: 2, button: 0, state: 0 });
    });

    it("锁定态按下/抬起成对下发，键值随事件", () => {
        setup();
        setLocked(true);
        target.dispatchEvent(mouseEvent("mousedown", { button: 2 }));
        target.dispatchEvent(mouseEvent("mouseup", { button: 2 }));
        expect(packets).toEqual([
            { type: 2, button: 2, state: 1 },
            { type: 2, button: 2, state: 0 },
        ]);
    });

    it("起点落在 UI 上：整个拖拽周期不透传", () => {
        setup();
        setLocked(true);
        document.body.dispatchEvent(mouseEvent("mousedown"));
        expect(packets).toHaveLength(0);
        target.dispatchEvent(mouseEvent("mousemove", { movementX: 5, movementY: 5 }));
        expect(packets).toHaveLength(0);
        document.body.dispatchEvent(mouseEvent("mouseup"));
        expect(packets).toHaveLength(0);
        // 抬起后阻断解除
        target.dispatchEvent(mouseEvent("mousemove", { movementX: 5, movementY: 5 }));
        expect(packets).toEqual([{ type: 1, dx: 5, dy: 5 }]);
    });

    it("IDLE 态按下仅作为回锁手势", () => {
        setup();
        target.dispatchEvent(mouseEvent("mousedown"));
        expect(target.requestPointerLock).toHaveBeenCalledWith({ unadjustedMovement: true });
        expect(packets).toHaveLength(0);
    });

    it("滚轮方向随 deltaY", () => {
        setup();
        setLocked(true);
        target.dispatchEvent(wheelEvent({ deltaY: -120 }));
        expect(packets).toEqual([{ type: 4, sy: 1 }]);
        target.dispatchEvent(wheelEvent({ deltaY: 120 }));
        expect(packets.at(-1)).toEqual({ type: 4, sy: -1 });
    });

    it("IDLE 态与汇聚点外滚轮不下发", () => {
        setup();
        target.dispatchEvent(wheelEvent({ deltaY: -120 }));
        document.body.dispatchEvent(wheelEvent({ deltaY: -120 }));
        expect(packets).toHaveLength(0);
    });
});

// ---------- 键盘管线 ----------

describe("键盘管线", () => {
    it("锁定态键码直通，按下/抬起与自动重复语义齐全", () => {
        setup();
        setLocked(true);
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyW", key: "w" }));
        expect(packets).toEqual([{ type: 3, code: 26, state: 1 }]);
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyW", key: "w", repeat: true }));
        expect(packets.at(-1)).toEqual({ type: 3, code: 26, state: 2 });
        document.body.dispatchEvent(keyEvent("keyup", { code: "KeyW", key: "w" }));
        expect(packets.at(-1)).toEqual({ type: 3, code: 26, state: 0 });
    });

    it("未映射键码不下发", () => {
        setup();
        setLocked(true);
        const e = keyEvent("keydown", { code: "F13", key: "F13" });
        document.body.dispatchEvent(e);
        expect(packets).toHaveLength(0);
        expect(e.defaultPrevented).toBe(true);
    });

    it("F11 归浏览器：不拦截也不下发", () => {
        setup();
        setLocked(true);
        const e = keyEvent("keydown", { code: "F11", key: "F11" });
        document.body.dispatchEvent(e);
        expect(packets).toHaveLength(0);
        expect(e.defaultPrevented).toBe(false);
    });

    it("IDLE 态键盘属页面，不下发", () => {
        setup();
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyW", key: "w" }));
        document.body.dispatchEvent(keyEvent("keyup", { code: "KeyW", key: "w" }));
        expect(packets).toHaveLength(0);
    });

    it("弹窗打开时键码全静默", () => {
        setup();
        setLocked(true);
        controller.setModalOpen(true);
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyW", key: "w" }));
        document.body.dispatchEvent(keyEvent("keyup", { code: "KeyW", key: "w" }));
        expect(packets).toHaveLength(0);
    });

    it("GUI 态可打印字符叠加文本通道", () => {
        setup();
        enterGuiMode();
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyA", key: "a" }));
        expect(packets).toEqual([{ type: 3, code: 4, state: 1 }]);
        expect(textSpy).toHaveBeenCalledWith("a");
    });

    it("GUI 态组合键不进文本通道", () => {
        setup();
        enterGuiMode();
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyA", key: "a", ctrlKey: true }));
        expect(packets).toHaveLength(1);
        expect(textSpy).not.toHaveBeenCalled();
    });

    it("GUI 态 Ctrl+V 吞键码交由 paste 通道", () => {
        setup();
        enterGuiMode();
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyV", key: "v", ctrlKey: true }));
        expect(packets).toHaveLength(0);
        expect(textSpy).not.toHaveBeenCalled();
    });

    it("锁定态 Ctrl+V 照常透传键码", () => {
        setup();
        setLocked(true);
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyV", key: "v", ctrlKey: true }));
        expect(packets).toEqual([{ type: 3, code: 25, state: 1 }]);
    });

    it("ESC 自动重复只认首次按下", () => {
        setup();
        setLocked(true);
        document.body.dispatchEvent(keyEvent("keydown", { code: "Escape", key: "Escape" }));
        document.body.dispatchEvent(keyEvent("keydown", { code: "Escape", key: "Escape", repeat: true }));
        expect(packets).toEqual([{ type: 3, code: 41, state: 1 }]);
    });

    it("窗口失焦释放全部按下键并清空手势", () => {
        setup();
        setLocked(true);
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyW", key: "w" }));
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        packets.length = 0;
        window.dispatchEvent(new Event("blur"));
        expect(packets).toEqual([{ type: 3, code: 26, state: 0 }]);
        expect(controller.touchGesture).toBeNull();
        expect(controller.touchWheel).toBeNull();
    });

    it("曾下发游戏的键在 IDLE 态抬起仍补发，防卡键", () => {
        setup();
        controller.pressedKeys.add(26);
        document.body.dispatchEvent(keyEvent("keyup", { code: "KeyW", key: "w" }));
        expect(packets).toEqual([{ type: 3, code: 26, state: 0 }]);
    });

    it("非可编辑目标的粘贴透传文本通道", () => {
        setup();
        const e = pasteEvent(document.body, "你好");
        expect(e.defaultPrevented).toBe(true);
        expect(textSpy).toHaveBeenCalledWith("你好");
    });

    it("页面可编辑元素与空剪贴板放行原生粘贴", () => {
        setup();
        const input = document.createElement("input");
        document.body.appendChild(input);
        pasteEvent(input, "text");
        pasteEvent(document.body, "");
        expect(textSpy).not.toHaveBeenCalled();
    });

    it("IME 组合中抑制键码，确认文本经文本通道注入后清空收集器", () => {
        setup();
        setLocked(true);
        imeCollector.dispatchEvent(new Event("compositionstart"));
        document.body.dispatchEvent(keyEvent("keydown", { code: "KeyW", key: "w" }));
        expect(packets).toHaveLength(0);
        imeCollector.value = "你好";
        const end = new Event("compositionend");
        Object.defineProperty(end, "data", { value: "你好" });
        imeCollector.dispatchEvent(end);
        expect(textSpy).toHaveBeenCalledWith("你好");
        expect(imeCollector.value).toBe("");
    });
});

// ---------- 指针锁状态机 ----------

describe("指针锁状态机", () => {
    it("锁定/解锁事件通知订阅者，浏览器侧退锁补发 ESC", () => {
        setup();
        setLocked(true);
        expect(lockSpy).toHaveBeenLastCalledWith(true);
        setLocked(false);
        expect(lockSpy).toHaveBeenLastCalledWith(false);
        // 补发的 ESC 按下帧立即下发，抬起帧延迟 50ms
        expect(packets).toEqual([{ type: 3, code: 41, state: 1 }]);
        vi.advanceTimersByTime(50);
        expect(packets).toEqual([
            { type: 3, code: 41, state: 1 },
            { type: 3, code: 41, state: 0 },
        ]);
    });

    it("主动退锁（GUI 打开）不补发 ESC", () => {
        setup();
        setLocked(true);
        controller.releaseLock();
        expect(document.exitPointerLock).toHaveBeenCalledTimes(1);
        setLocked(false);
        vi.advanceTimersByTime(100);
        expect(packets).toHaveLength(0);
    });

    it("IDLE 点击画面申请原始输入回锁", () => {
        setup();
        target.dispatchEvent(mouseEvent("click", { x: 10, y: 10 }));
        expect(target.requestPointerLock).toHaveBeenCalledWith({ unadjustedMovement: true });
    });

    it("平台不支持原始输入时降级为普通锁定", async () => {
        setup();
        target.requestPointerLock = vi
            .fn()
            .mockRejectedValueOnce(Object.assign(new Error("x"), { name: "NotSupportedError" }));
        target.dispatchEvent(mouseEvent("click", { x: 10, y: 10 }));
        await flush();
        expect(target.requestPointerLock).toHaveBeenCalledTimes(2);
        expect(target.requestPointerLock.mock.calls[1][0]).toBeUndefined();
    });

    it("其他拒绝静默不重试", async () => {
        setup();
        target.requestPointerLock = vi
            .fn()
            .mockRejectedValueOnce(Object.assign(new Error("x"), { name: "OtherError" }));
        target.dispatchEvent(mouseEvent("click", { x: 10, y: 10 }));
        await flush();
        expect(target.requestPointerLock).toHaveBeenCalledTimes(1);
    });

    it("同步抛错静默不外泄", () => {
        setup();
        target.requestPointerLock = vi.fn(() => {
            throw new Error("sync");
        });
        expect(() => target.dispatchEvent(mouseEvent("click"))).not.toThrow();
        expect(target.requestPointerLock).toHaveBeenCalledTimes(1);
    });

    it("GUI 关闭后的自动回锁避开 ESC 手势窗口", () => {
        setup();
        enterGuiMode();
        document.body.dispatchEvent(keyEvent("keydown", { code: "Escape", key: "Escape" }));
        document.body.dispatchEvent(keyEvent("keyup", { code: "Escape", key: "Escape" }));
        controller.setCursorState(false);
        expect(target.requestPointerLock).not.toHaveBeenCalled();
        vi.advanceTimersByTime(160);
        expect(target.requestPointerLock).toHaveBeenCalledTimes(1);
    });

    it("ESC 按住未抬时持续让窗口，抬起落定后再锁", () => {
        setup();
        enterGuiMode();
        document.body.dispatchEvent(keyEvent("keydown", { code: "Escape", key: "Escape" }));
        controller.setCursorState(false);
        vi.advanceTimersByTime(155);
        expect(target.requestPointerLock).not.toHaveBeenCalled();
        document.body.dispatchEvent(keyEvent("keyup", { code: "Escape", key: "Escape" }));
        vi.advanceTimersByTime(160);
        expect(target.requestPointerLock).toHaveBeenCalledTimes(1);
    });

    it("从未按过 ESC 时立即回锁", () => {
        setup();
        enterGuiMode();
        controller.setCursorState(false);
        expect(target.requestPointerLock).toHaveBeenCalledTimes(1);
    });

    it("触摸轻点合成的 click 在 500ms 窗口内不触发锁定", () => {
        setup();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        vi.advanceTimersByTime(100);
        target.dispatchEvent(touchEvent("pointerup", { pointerId: 1, x: 100, y: 100 }));
        target.dispatchEvent(mouseEvent("click", { x: 100, y: 100 }));
        expect(target.requestPointerLock).not.toHaveBeenCalled();
        vi.advanceTimersByTime(600);
        target.dispatchEvent(mouseEvent("click", { x: 100, y: 100 }));
        expect(target.requestPointerLock).toHaveBeenCalledTimes(1);
    });

    it("pointerlockerror 静默不外泄", () => {
        setup();
        expect(() => document.dispatchEvent(new Event("pointerlockerror"))).not.toThrow();
    });
});

// ---------- 触摸手势状态机 ----------

describe("触摸手势状态机", () => {
    it("游戏态单指拖拽逐帧下发相对位移", () => {
        setup();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 1, x: 110, y: 105 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 1, x: 120, y: 115 }));
        target.dispatchEvent(touchEvent("pointerup", { pointerId: 1, x: 120, y: 115 }));
        expect(packets).toEqual([
            { type: 1, dx: 10, dy: 5 },
            { type: 1, dx: 10, dy: 10 },
        ]);
    });

    it("轻点下发左键对", () => {
        setup();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        vi.advanceTimersByTime(100);
        target.dispatchEvent(touchEvent("pointerup", { pointerId: 1, x: 100, y: 100 }));
        expect(packets).toEqual([
            { type: 2, button: 0, state: 1 },
            { type: 2, button: 0, state: 0 },
        ]);
    });

    it("超时轻点不算轻点", () => {
        setup();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        vi.advanceTimersByTime(260);
        target.dispatchEvent(touchEvent("pointerup", { pointerId: 1, x: 100, y: 100 }));
        expect(packets).toHaveLength(0);
    });

    it("超出位移阈值的快速滑动不算轻点", () => {
        setup();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 1, x: 130, y: 100 }));
        target.dispatchEvent(touchEvent("pointerup", { pointerId: 1, x: 130, y: 100 }));
        expect(packets.every((p) => p.type !== 2)).toBe(true);
        expect(packets.some((p) => p.type === 1)).toBe(true);
    });

    it("双指上滑按中点位移步进滚轮上滚，不产生视角位移", () => {
        setup();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 2, x: 100, y: 100 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 1, x: 100, y: 80 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 2, x: 100, y: 80 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 1, x: 100, y: 60 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 2, x: 100, y: 60 }));
        const wheels = packets.filter((p) => p.type === 4);
        expect(wheels).toEqual([{ type: 4, sy: 1 }]);
        expect(packets.some((p) => p.type === 1)).toBe(false);
    });

    it("双指下滑产生滚轮下滚", () => {
        setup();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 2, x: 100, y: 100 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 1, x: 100, y: 120 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 2, x: 100, y: 120 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 1, x: 100, y: 140 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 2, x: 100, y: 140 }));
        const wheels = packets.filter((p) => p.type === 4);
        expect(wheels).toEqual([{ type: 4, sy: -1 }]);
    });

    it("抬起一指即终结滚轮模式", () => {
        setup();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 2, x: 100, y: 100 }));
        target.dispatchEvent(touchEvent("pointerup", { pointerId: 2, x: 100, y: 100 }));
        expect(controller.touchWheel).toBeNull();
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 1, x: 100, y: 60 }));
        expect(packets).toHaveLength(0);
    });

    it("pointercancel 不算轻点且清场", () => {
        setup();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 1, x: 105, y: 105 }));
        target.dispatchEvent(touchEvent("pointercancel", { pointerId: 1, x: 105, y: 105 }));
        expect(packets.every((p) => p.type !== 2)).toBe(true);
        expect(controller.touchGesture).toBeNull();
        expect(controller.touchWheel).toBeNull();
    });

    it("弹窗中途打开时抬起强制清场", () => {
        setup();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        controller.setModalOpen(true);
        target.dispatchEvent(touchEvent("pointerup", { pointerId: 1, x: 100, y: 100 }));
        expect(controller.touchGesture).toBeNull();
        expect(packets).toHaveLength(0);
    });

    it("弹窗打开期间忽略触摸按下", () => {
        setup();
        controller.setModalOpen(true);
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        expect(controller.touchGesture).toBeNull();
        expect(packets).toHaveLength(0);
    });

    it("GUI 态触摸光标绝对跟随，无相对位移", () => {
        setup();
        setViewportRect();
        enterGuiMode();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 165 }));
        expect(packets).toEqual([{ type: 5, x: 8192, y: 13107 }]);
        target.dispatchEvent(touchEvent("pointermove", { pointerId: 1, x: 110, y: 165 }));
        expect(packets.at(-1)).toEqual({ type: 5, x: 9011, y: 13107 });
        expect(packets.every((p) => p.type !== 1)).toBe(true);
    });

    it("非触摸指针事件不进手势状态机", () => {
        setup();
        target.dispatchEvent(mousePointerEvent("pointerdown", { x: 100, y: 100 }));
        target.dispatchEvent(mousePointerEvent("pointermove", { x: 110, y: 110 }));
        expect(controller.touchGesture).toBeNull();
        expect(packets).toHaveLength(0);
    });

    it("汇聚点外的触摸按下被忽略", () => {
        setup();
        document.body.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        expect(controller.touchGesture).toBeNull();
        expect(packets).toHaveLength(0);
    });
});

// ---------- 解锁提示 ----------

describe("解锁提示", () => {
    it("初始 IDLE 态满 500ms 提示回锁", () => {
        setup();
        vi.advanceTimersByTime(500);
        expect(hintSpy).toHaveBeenCalledWith(true);
    });

    it("GUI 态不发提示", () => {
        setup();
        controller.setCursorState(true);
        vi.advanceTimersByTime(1000);
        expect(hintSpy).not.toHaveBeenCalled();
    });

    it("提示已展示后 GUI 打开撤销提示", () => {
        setup();
        vi.advanceTimersByTime(500);
        expect(hintSpy).toHaveBeenCalledWith(true);
        controller.setCursorState(true);
        expect(hintSpy).toHaveBeenLastCalledWith(false);
    });

    it("出现过触摸即永久豁免", () => {
        setup();
        target.dispatchEvent(touchEvent("pointerdown", { pointerId: 1, x: 100, y: 100 }));
        target.dispatchEvent(touchEvent("pointerup", { pointerId: 1, x: 100, y: 100 }));
        vi.advanceTimersByTime(1000);
        expect(hintSpy).not.toHaveBeenCalled();
    });

    it("锁定态静默，解锁后重新起表", () => {
        setup();
        setLocked(true);
        vi.advanceTimersByTime(1000);
        expect(hintSpy).not.toHaveBeenCalled();
        setLocked(false);
        vi.advanceTimersByTime(500);
        expect(hintSpy).toHaveBeenCalledWith(true);
    });
});
