/**
 * Vistrel Web Client 协议常量与编解码器映射规范
 */

// 被新连接抢占时服务端下发的 WebSocket 关闭码 (SignalingServer 的 4001 Preempted 帧)
export const KICKED_CODE = 4001;

// 画质档位（bits per pixel per frame）——BPP 是 Remote Player 唯一的质量旋钮：
// 分辨率与帧率由游戏实例决定（Content Resolution / Streaming Framerate）
export const BPP_TIERS = [];
for (let hundredths = 10; hundredths <= 30; hundredths++) BPP_TIERS.push(hundredths / 100);

// Display Fit Mode 三段：只影响浏览器内的呈现，绝不改变编码帧
export const FIT_MODES = [
    { label: "包含", value: "contain" },
    { label: "原始", value: "none" },
    { label: "裁切", value: "cover" },
];

// 信令词汇表——与 Java SignalingServer.onWsMessage 的消息类型对齐
export const WS_OUT = {
    OFFER: "offer",
    CANDIDATE: "candidate",
    TEXT_INPUT: "text_input",
    // 客户端串流偏好同步：幂等状态快照，尽量携带完整 bpp + viewport（物理像素，UI 层算好）；
    // 不产生回执——显示几何走 Answer/display_geometry 镜像，实测数据由视频元素与 getStats 自观测
    SETTINGS: "settings",
};

// 信令下行词汇表——Java 侧对端：SignalingServer 的 TextSignal 帧（candidate /
// auth_failed / error 由其直接下发）；answer 帧负载是 NativeBridge.negotiateOffer
// 的应答 JSON，字段形状 width/height/fps/bpp/bitrate/codec/backend/backends/sdp，
// 其中 backends 是编码能力清单 [{kind, codecs}]（硬编按服务端优先级序，软件编码收尾）；
// answer 的 width/height 是显示几何初值，变化经 display_geometry 推送
export const WS_IN = {
    ANSWER: "answer",
    CANDIDATE: "candidate",
    AUTH_FAILED: "auth_failed",
    ERROR: "error",
    // 显示几何更新（ClientEvent.DisplayGeometry）：宽高是画面可见区域，不含编码对齐补齐边；
    // 初值随 Answer 下发，服务端只在几何变化时推送
    DISPLAY_GEOMETRY: "display_geometry",
};

// 下行控制事件词汇表（同经信令 WS 文本帧）；与 ClientEvent.java 的下行 record 手工对齐，
// 其中 display_geometry 在 WS_IN——ClientEvent.java 是本表的另一端锚点
export const CONTROL_IN = {
    CURSOR_STATE: "cursor_state",
    OPEN_URL: "open_url",
    CLIPBOARD_WRITE: "clipboard_write",
};
// 数据通道输入协议类型头
export const INPUT_PACKET_TYPE = {
    MOUSE_MOVE: 0x01,
    MOUSE_BUTTON: 0x02,
    KEYBOARD: 0x03,
    MOUSE_WHEEL: 0x04,
    ABSOLUTE_MOUSE_MOVE: 0x05,
};

// USB HID Usage Tables (Page 0x07) 物理扫描码表
export const HID_KEY_MAP = {
    KeyW: 26,
    KeyA: 4,
    KeyS: 22,
    KeyD: 7,
    Space: 44,
    ShiftLeft: 225,
    ShiftRight: 229,
    ControlLeft: 224,
    ControlRight: 228,
    AltLeft: 226,
    AltRight: 230,
    MetaLeft: 227,
    MetaRight: 231,

    KeyB: 5,
    KeyC: 6,
    KeyE: 8,
    KeyF: 9,
    KeyG: 10,
    KeyH: 11,
    KeyI: 12,
    KeyJ: 13,
    KeyK: 14,
    KeyL: 15,
    KeyM: 16,
    KeyN: 17,
    KeyO: 18,
    KeyP: 19,
    KeyQ: 20,
    KeyR: 21,
    KeyT: 23,
    KeyU: 24,
    KeyV: 25,
    KeyX: 27,
    KeyY: 28,
    KeyZ: 29,

    Digit1: 30,
    Digit2: 31,
    Digit3: 32,
    Digit4: 33,
    Digit5: 34,
    Digit6: 35,
    Digit7: 36,
    Digit8: 37,
    Digit9: 38,
    Digit0: 39,

    Enter: 40,
    Escape: 41,
    Backspace: 42,
    Tab: 43,
    Minus: 45,
    Equal: 46,
    BracketLeft: 47,
    BracketRight: 48,
    Backslash: 49,
    Semicolon: 51,
    Quote: 52,
    Grave: 53,
    Comma: 54,
    Period: 55,
    Slash: 56,
    CapsLock: 57,

    F1: 58,
    F2: 59,
    F3: 60,
    F4: 61,
    F5: 62,
    F6: 63,
    F7: 64,
    F8: 65,
    F9: 66,
    F10: 67,
    F11: 68,
    F12: 69,

    PrintScreen: 70,
    ScrollLock: 71,
    Pause: 72,
    Insert: 73,
    Home: 74,
    PageUp: 75,
    Delete: 76,
    End: 77,
    PageDown: 78,
    ArrowRight: 79,
    ArrowLeft: 80,
    ArrowDown: 81,
    ArrowUp: 82,

    NumLock: 83,
    NumpadDivide: 84,
    NumpadMultiply: 85,
    NumpadSubtract: 86,
    NumpadAdd: 87,
    Numpad1: 89,
    Numpad2: 90,
    Numpad3: 91,
    Numpad4: 92,
    Numpad5: 93,
    Numpad6: 94,
    Numpad7: 95,
    Numpad8: 96,
    Numpad9: 97,
    Numpad0: 98,
    NumpadDecimal: 99,
};

/** 从文本解析出 codec；识别不出按 H.264 处理 */
export function parseCodecToken(codec) {
    if (!codec) return "h264";
    // 去点去横线：H.264 / h-265 这类写法都能归一到 h264 / h265
    const s = String(codec).toLowerCase().replace(/[._-]/g, "");
    if (s.includes("av1")) return "av1";
    if (s.includes("h265") || s.includes("hevc") || s.includes("hvc1") || s.includes("hev1")) return "h265";
    if (s.includes("h264") || s.includes("avc") || s.includes("avc1")) return "h264";
    if (s.includes("vp9")) return "vp9";
    if (s.includes("vp8")) return "vp8";
    return "h264";
}
