import { describe, expect, it } from "vitest";
import {
    BPP_TIERS,
    CONTROL_IN,
    FIT_MODES,
    HID_KEY_MAP,
    INPUT_PACKET_TYPE,
    KICKED_CODE,
    WS_IN,
    WS_OUT,
    parseCodecToken,
} from "./protocol.js";

describe("parseCodecToken", () => {
    it("空值与无法识别的串回退 h264", () => {
        expect(parseCodecToken(undefined)).toBe("h264");
        expect(parseCodecToken(null)).toBe("h264");
        expect(parseCodecToken("")).toBe("h264");
        expect(parseCodecToken("mpeg2")).toBe("h264");
    });

    it("分隔符与大小写归一后识别", () => {
        expect(parseCodecToken("H.264")).toBe("h264");
        expect(parseCodecToken("h-265")).toBe("h265");
        expect(parseCodecToken("H_265")).toBe("h265");
        expect(parseCodecToken("AVC1.640028")).toBe("h264");
        expect(parseCodecToken("HeVc")).toBe("h265");
    });

    it("覆盖全部受支持的编解码器家族", () => {
        expect(parseCodecToken("av1")).toBe("av1");
        expect(parseCodecToken("h265")).toBe("h265");
        expect(parseCodecToken("hvc1")).toBe("h265");
        expect(parseCodecToken("hev1")).toBe("h265");
        expect(parseCodecToken("h264")).toBe("h264");
        expect(parseCodecToken("avc")).toBe("h264");
        expect(parseCodecToken("vp9")).toBe("vp9");
        expect(parseCodecToken("vp8")).toBe("vp8");
    });

    it("SDP 媒体行形态可直接解析", () => {
        expect(parseCodecToken("video/H265")).toBe("h265");
        expect(parseCodecToken("video/AV1")).toBe("av1");
        expect(parseCodecToken("video/H264")).toBe("h264");
    });
});

describe("协议词汇表", () => {
    it("KICKED_CODE 与服务端 4001 Preempted 帧对齐", () => {
        expect(KICKED_CODE).toBe(4001);
    });

    it("输入帧类型头 0x01-0x05 唯一且锚点稳定", () => {
        const values = Object.values(INPUT_PACKET_TYPE);
        expect(new Set(values).size).toBe(values.length);
        expect(INPUT_PACKET_TYPE.MOUSE_MOVE).toBe(0x01);
        expect(INPUT_PACKET_TYPE.MOUSE_BUTTON).toBe(0x02);
        expect(INPUT_PACKET_TYPE.KEYBOARD).toBe(0x03);
        expect(INPUT_PACKET_TYPE.MOUSE_WHEEL).toBe(0x04);
        expect(INPUT_PACKET_TYPE.ABSOLUTE_MOUSE_MOVE).toBe(0x05);
    });

    it("信令上行/下行/控制三张词汇表各自无重复值", () => {
        for (const table of [WS_OUT, WS_IN, CONTROL_IN]) {
            const values = Object.values(table);
            expect(new Set(values).size).toBe(values.length);
        }
    });

    it("BPP_TIERS 为 0.10-0.30 步进 0.01 的 21 档单调序列", () => {
        expect(BPP_TIERS).toHaveLength(21);
        expect(BPP_TIERS[0]).toBeCloseTo(0.1, 10);
        expect(BPP_TIERS[BPP_TIERS.length - 1]).toBeCloseTo(0.3, 10);
        for (let i = 1; i < BPP_TIERS.length; i++) {
            expect(BPP_TIERS[i]).toBeGreaterThan(BPP_TIERS[i - 1]);
            expect(BPP_TIERS[i] - BPP_TIERS[i - 1]).toBeCloseTo(0.01, 10);
        }
    });

    it("FIT_MODES 三档与 CSS object-fit 合法值对齐且带标签", () => {
        expect(FIT_MODES.map((m) => m.value)).toEqual(["contain", "none", "cover"]);
        expect(FIT_MODES.every((m) => typeof m.label === "string" && m.label.length > 0)).toBe(true);
    });
});

describe("HID_KEY_MAP", () => {
    it("全部为 USB HID 0x07 页合法范围内的整数扫描码", () => {
        const values = Object.values(HID_KEY_MAP);
        expect(values.length).toBeGreaterThan(80);
        expect(values.every((v) => Number.isInteger(v) && v >= 4 && v <= 231)).toBe(true);
    });

    it("无重复扫描码（两键同码会让游戏侧无法区分）", () => {
        const values = Object.values(HID_KEY_MAP);
        expect(new Set(values).size).toBe(values.length);
    });

    it("锚点抽查与 USB HID Usage Tables 一致", () => {
        expect(HID_KEY_MAP["KeyA"]).toBe(4);
        expect(HID_KEY_MAP["KeyZ"]).toBe(29);
        expect(HID_KEY_MAP["Digit0"]).toBe(39);
        expect(HID_KEY_MAP["Enter"]).toBe(40);
        expect(HID_KEY_MAP["Escape"]).toBe(41);
        expect(HID_KEY_MAP["Space"]).toBe(44);
        expect(HID_KEY_MAP["F1"]).toBe(58);
        expect(HID_KEY_MAP["F12"]).toBe(69);
        expect(HID_KEY_MAP["ArrowRight"]).toBe(79);
        expect(HID_KEY_MAP["ArrowUp"]).toBe(82);
        expect(HID_KEY_MAP["ShiftLeft"]).toBe(225);
        expect(HID_KEY_MAP["ShiftRight"]).toBe(229);
        expect(HID_KEY_MAP["ControlLeft"]).toBe(224);
        expect(HID_KEY_MAP["MetaRight"]).toBe(231);
    });

    it("核心操作键在表中（WASD/方向/F1-F12/Escape）", () => {
        for (const code of [
            "KeyW",
            "KeyA",
            "KeyS",
            "KeyD",
            "Space",
            "ShiftLeft",
            "Escape",
            "ArrowUp",
            "Tab",
            "Enter",
        ]) {
            expect(HID_KEY_MAP[code]).toBeDefined();
        }
    });
});
