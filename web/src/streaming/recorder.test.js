import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CaptureController, RecordingController, ReplayClipController, keepAll, windowOf } from "./recorder.js";

// ---------- 假媒体环境：MediaRecorder / MediaStream / 轨道 ----------

let supportedTypes;
let typeSupportQueries;
let recorders;

class FakeMediaRecorder {
    static isTypeSupported(type) {
        typeSupportQueries.push(type);
        return supportedTypes.has(type);
    }

    constructor(stream, options) {
        this.stream = stream;
        this.options = options;
        this.state = "inactive";
        this.timeslice = null;
        this.stopCalls = 0;
        recorders.push(this);
    }

    start(timeslice) {
        this.state = "recording";
        this.timeslice = timeslice;
    }

    stop() {
        this.stopCalls++;
        if (this.state === "inactive") return;
        this.state = "inactive";
        if (this.onstop) this.onstop();
    }
}

class FakeMediaStream {
    constructor(tracks) {
        this.tracks = tracks;
    }
}

/** 安装假体后才能构造 CaptureController：构造期即探测 MIME */
function installMediaStub({ supported = [] } = {}) {
    supportedTypes = new Set(supported);
    typeSupportQueries = [];
    recorders = [];
    vi.stubGlobal("MediaRecorder", FakeMediaRecorder);
    vi.stubGlobal("MediaStream", FakeMediaStream);
}

function makeTrack(kind = "video") {
    const listeners = new Map();
    return {
        kind,
        addEventListener(type, handler) {
            if (!listeners.has(type)) listeners.set(type, new Set());
            listeners.get(type).add(handler);
        },
        removeEventListener(type, handler) {
            listeners.get(type)?.delete(handler);
        },
        emit(type) {
            for (const handler of listeners.get(type) ?? []) handler();
        },
    };
}

function chunk(text) {
    return new Blob([text]);
}

function makeCapture({ retention = keepAll(), inbound = null, videoTrack = makeTrack(), audioTrack = null } = {}) {
    const chunkCalls = [];
    const stopped = [];
    const capture = new CaptureController({
        getTracks: () => ({ videoTrack, audioTrack }),
        getInboundBitrate: () => inbound,
        retention,
        onChunk: () => chunkCalls.push(1),
        onStopped: (blob, cause) => stopped.push({ blob, cause }),
    });
    return { capture, chunkCalls, stopped, videoTrack, audioTrack };
}

beforeEach(() => {
    installMediaStub({ supported: ['video/mp4;codecs="avc1.640028,mp4a.40.2"'] });
});

afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    localStorage.clear();
});

// ---------- CaptureController ----------

describe("CaptureController 码率派生与 MIME 选择", () => {
    it("无估算或非法估算回退 12 Mbps，音频固定 128 kbps", () => {
        for (const inbound of [null, NaN, 0, -5, Infinity]) {
            const { capture } = makeCapture({ inbound });
            capture.start();
            const recorder = recorders.at(-1);
            expect(recorder.options.videoBitsPerSecond).toBe(12_000_000);
            expect(recorder.options.audioBitsPerSecond).toBe(128_000);
        }
    });

    it("估算 ×1.2 后夹取 [6, 32] Mbps 区间", () => {
        for (const [inbound, expected] of [
            [5_000_000, 6_000_000],
            [10_500_000, 12_600_000],
            [40_000_000, 32_000_000],
        ]) {
            const { capture } = makeCapture({ inbound });
            capture.start();
            expect(recorders.at(-1).options.videoBitsPerSecond).toBe(expected);
        }
    });

    it("按候选顺序取首个受支持的录制串", () => {
        const { capture } = makeCapture();
        capture.start();
        expect(recorders.at(-1).options.mimeType).toBe('video/mp4;codecs="avc1.640028,mp4a.40.2"');
        // 只支持靠后的候选时按序探测直到命中
        installMediaStub({ supported: ["video/mp4;codecs=avc1", "video/mp4"] });
        const second = makeCapture();
        second.capture.start();
        expect(recorders.at(-1).options.mimeType).toBe("video/mp4;codecs=avc1");
        expect(typeSupportQueries.length).toBeGreaterThanOrEqual(4);
    });

    it("全不支持时报告 unsupported 且不构造录制器", () => {
        installMediaStub({ supported: [] });
        const { capture, stopped } = makeCapture();
        expect(capture.start()).toEqual({ reason: "unsupported" });
        expect(recorders).toHaveLength(0);
        expect(stopped).toHaveLength(0);
    });

    it("无视频轨时报告 no-video", () => {
        const { capture } = makeCapture({ videoTrack: null });
        expect(capture.start()).toEqual({ reason: "no-video" });
        expect(recorders).toHaveLength(0);
    });

    it("重复 start 幂等：不重复构造录制器", () => {
        const { capture } = makeCapture();
        expect(capture.start()).toBeNull();
        expect(capture.start()).toBeNull();
        expect(recorders).toHaveLength(1);
        expect(capture.isActive).toBe(true);
    });
});

describe("CaptureController 分片与停止路径", () => {
    it("分片进入保留策略并通知 onChunk", () => {
        const { capture, chunkCalls } = makeCapture();
        capture.start();
        recorders[0].ondataavailable({ data: chunk("a") });
        recorders[0].ondataavailable({ data: chunk("b") });
        expect(chunkCalls).toHaveLength(2);
        const blob = capture.takeBlob();
        expect(blob.size).toBe(2);
        expect(blob.type).toBe('video/mp4;codecs="avc1.640028,mp4a.40.2"');
    });

    it("空分片与无数据分片直接丢弃", () => {
        const { capture, chunkCalls } = makeCapture();
        capture.start();
        recorders[0].ondataavailable({ data: null });
        recorders[0].ondataavailable({ data: new Blob([]) });
        expect(chunkCalls).toHaveLength(0);
    });

    it("停止交付合成 Blob 后幂等：多路触发只收尾一次", () => {
        const { capture, stopped } = makeCapture();
        capture.start();
        recorders[0].ondataavailable({ data: chunk("a") });
        recorders[0].ondataavailable({ data: chunk("b") });
        capture.stop("user");
        capture.stop("user");
        capture.stop("error");
        expect(stopped).toHaveLength(1);
        expect(stopped[0].cause).toBe("user");
        expect(stopped[0].blob.size).toBe(2);
        expect(capture.isActive).toBe(false);
        expect(recorders[0].stopCalls).toBe(1);
    });

    it("收尾后迟到的分片被丢弃", () => {
        const { capture, chunkCalls, stopped } = makeCapture();
        capture.start();
        capture.stop("user");
        recorders[0].ondataavailable({ data: chunk("late") });
        expect(chunkCalls).toHaveLength(0);
        expect(stopped).toHaveLength(1);
    });

    it("轨道 ended 触发兜底停止并摘除句柄", () => {
        const { capture, stopped, videoTrack } = makeCapture();
        capture.start();
        videoTrack.emit("ended");
        expect(stopped[0].cause).toBe("ended");
        // 停止后轨道再 ended 不再触发
        videoTrack.emit("ended");
        expect(stopped).toHaveLength(1);
    });

    it("录制器错误触发 error 停止", () => {
        const { capture, stopped } = makeCapture();
        capture.start();
        recorders[0].onerror();
        expect(stopped[0].cause).toBe("error");
    });

    it("录制器已 inactive 时停止走直接收尾", () => {
        const { capture, stopped } = makeCapture();
        capture.start();
        recorders[0].state = "inactive";
        capture.stop("error");
        expect(stopped[0].cause).toBe("error");
        expect(recorders[0].stopCalls).toBe(0);
    });

    it("takeBlob 非破坏：连续取用不消耗缓冲", () => {
        const { capture } = makeCapture();
        capture.start();
        recorders[0].ondataavailable({ data: chunk("a") });
        expect(capture.takeBlob().size).toBe(1);
        recorders[0].ondataavailable({ data: chunk("b") });
        expect(capture.takeBlob().size).toBe(2);
    });
});

// ---------- 留存策略 ----------

describe("keepAll 留存策略", () => {
    it("全量累积，take 返回副本，reset 清空", () => {
        const keeper = keepAll();
        keeper.push(chunk("a"));
        keeper.push(chunk("b"));
        expect(keeper.bytes).toBe(2);
        const taken = keeper.take();
        expect(taken).toHaveLength(2);
        taken.push(chunk("c"));
        expect(keeper.take()).toHaveLength(2);
        keeper.reset();
        expect(keeper.take()).toHaveLength(0);
        expect(keeper.bytes).toBe(0);
    });
});

describe("windowOf 留存策略", () => {
    it("首个分片视为 init segment 永久保留并置于最前", () => {
        const keeper = windowOf(60_000);
        keeper.push(chunk("init"));
        keeper.push(chunk("c1"));
        keeper.push(chunk("c2"));
        expect(keeper.ready).toBe(true);
        expect(keeper.take()).toHaveLength(3);
        expect(keeper.bytes).toBe(8);
    });

    it("仅有 init segment 时未就绪", () => {
        const keeper = windowOf(60_000);
        keeper.push(chunk("init"));
        expect(keeper.ready).toBe(false);
        expect(keeper.take()).toEqual([]);
    });

    it("超出墙钟窗口的分片被逐出且字节数随之递减", () => {
        vi.useFakeTimers({ toFake: ["Date"] });
        try {
            const keeper = windowOf(60_000);
            keeper.push(chunk("init")); // 4B
            keeper.push(chunk("abcdef")); // 6B
            expect(keeper.bytes).toBe(10);
            vi.advanceTimersByTime(70_000);
            keeper.push(chunk("xy")); // 2B，此时 c1 已过期逐出
            expect(keeper.take()).toHaveLength(2);
            expect(keeper.bytes).toBe(6);
        } finally {
            vi.useRealTimers();
        }
    });

    it("reset 清空 init 与窗口", () => {
        const keeper = windowOf(60_000);
        keeper.push(chunk("init"));
        keeper.push(chunk("c1"));
        keeper.reset();
        expect(keeper.ready).toBe(false);
        expect(keeper.take()).toEqual([]);
        expect(keeper.bytes).toBe(0);
    });
});

// ---------- RecordingController ----------

describe("RecordingController", () => {
    beforeEach(() => {
        vi.useFakeTimers({ toFake: ["Date"] });
        vi.setSystemTime(new Date(2026, 9, 31, 18, 6, 28));
    });

    afterEach(() => {
        vi.useRealTimers();
    });

    function makeRecorderController() {
        const states = [];
        const saved = [];
        const errors = [];
        const controller = new RecordingController({
            getTracks: () => ({ videoTrack: makeTrack(), audioTrack: null }),
            getInboundBitrate: () => null,
            onStateChange: (active, text) => states.push({ active, text }),
            onSaved: (result) => saved.push(result),
            onError: (msg) => errors.push(msg),
        });
        return { controller, states, saved, errors };
    }

    it("停止后落盘：文件名带时间戳，保存回调带体积与原因", () => {
        const clickSpy = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
        const { controller, states, saved, errors } = makeRecorderController();
        expect(controller.start()).toBeUndefined();
        recorders.at(-1).ondataavailable({ data: chunk("ab") });
        expect(states.at(-1).active).toBe(true);
        expect(states.at(-1).text).toMatch(/^REC /);
        controller.stop("user");
        expect(states.at(-1).active).toBe(false);
        expect(errors).toHaveLength(0);
        expect(saved).toEqual([{ sizeText: "0 KB", reason: "user" }]);
        const anchor = clickSpy.mock.instances[0];
        expect(anchor.download).toBe("vistrel_2026-10-31_18-06-28.mp4");
    });

    it("未录制到内容时不落盘并报错", () => {
        const clickSpy = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
        const { controller, saved, errors } = makeRecorderController();
        controller.start();
        controller.stop("user");
        expect(errors).toEqual(["未录制到内容，未生成文件"]);
        expect(saved).toHaveLength(0);
        expect(clickSpy).not.toHaveBeenCalled();
    });

    it("会话拆除按 interrupted 落盘", () => {
        const { controller, saved } = makeRecorderController();
        controller.start();
        recorders.at(-1).ondataavailable({ data: chunk("ab") });
        controller.handleSessionTornDown();
        expect(saved[0].reason).toBe("interrupted");
    });

    it("轨道 ended 在用户未喊停时按 interrupted 归因", () => {
        const { controller, saved } = makeRecorderController();
        controller.start();
        recorders.at(-1).ondataavailable({ data: chunk("ab") });
        recorders.at(-1).stream.tracks[0].emit("ended");
        expect(saved[0].reason).toBe("interrupted");
    });

    it("录制中拦截页面卸载，空闲时放行", () => {
        const { controller } = makeRecorderController();
        const guardEvent = new Event("beforeunload", { cancelable: true });
        const preventSpy = vi.spyOn(guardEvent, "preventDefault");
        window.dispatchEvent(guardEvent);
        expect(preventSpy).not.toHaveBeenCalled();
        controller.start();
        const activeEvent = new Event("beforeunload", { cancelable: true });
        const activePreventSpy = vi.spyOn(activeEvent, "preventDefault");
        window.dispatchEvent(activeEvent);
        expect(activePreventSpy).toHaveBeenCalledTimes(1);
    });
});

// ---------- ReplayClipController ----------

describe("ReplayClipController", () => {
    beforeEach(() => {
        vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    });

    afterEach(() => {
        vi.useRealTimers();
    });

    function makeClipController() {
        const videoTrack = makeTrack();
        const bufferStates = [];
        const notes = [];
        const controller = new ReplayClipController({
            getTracks: () => ({ videoTrack, audioTrack: null }),
            getInboundBitrate: () => null,
            onBufferActive: (active, reason) => bufferStates.push({ active, reason }),
            onNote: (msg, isError) => notes.push({ msg, isError }),
        });
        return { controller, videoTrack, bufferStates, notes };
    }

    it("默认开启且初始不可用（等待画面接入）", () => {
        const { controller, bufferStates } = makeClipController();
        expect(controller.isEnabled()).toBe(true);
        controller.notifyAvailability();
        expect(bufferStates).toEqual([{ active: false, reason: "等待画面接入" }]);
        // 同态重复通知不重复回调
        controller.notifyAvailability();
        expect(bufferStates).toHaveLength(1);
    });

    it("视频轨到达经 1s 宽限起录，init 与窗口分片齐备后转为可用", () => {
        const { controller, videoTrack, bufferStates } = makeClipController();
        controller.handleVideoTrack(videoTrack);
        expect(recorders).toHaveLength(0);
        vi.advanceTimersByTime(1000);
        expect(recorders).toHaveLength(1);
        expect(recorders[0].timeslice).toBe(1000);
        recorders[0].ondataavailable({ data: chunk("init") });
        expect(bufferStates.at(-1)).toEqual({ active: false, reason: "回放缓冲就绪中…" });
        recorders[0].ondataavailable({ data: chunk("c1") });
        expect(bufferStates.at(-1)).toEqual({ active: true, reason: "" });
    });

    it("音频轨到达时提前唤醒宽限等待", () => {
        const { controller, videoTrack } = makeClipController();
        controller.handleVideoTrack(videoTrack);
        controller.handleAudioTrack(makeTrack("audio"));
        expect(recorders).toHaveLength(1);
    });

    it("关闭开关立即弃置缓冲并持久化偏好", () => {
        const { controller, videoTrack, notes } = makeClipController();
        controller.handleVideoTrack(videoTrack);
        controller.handleAudioTrack(makeTrack("audio"));
        expect(controller.capture.isActive).toBe(true);
        const clickSpy = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
        controller.setEnabled(false);
        expect(controller.isEnabled()).toBe(false);
        expect(localStorage.getItem("vistrel_replay_enabled")).toBe("0");
        expect(controller.capture.isActive).toBe(false);
        expect(clickSpy).not.toHaveBeenCalled();
        // 关闭态保存剪辑：缓冲已弃置，提示未就绪
        controller.saveClip();
        expect(notes.at(-1)).toEqual({ msg: "回放缓冲尚未就绪", isError: true });
    });

    it("会话拆除静默弃置：不合成、不下载", () => {
        const clickSpy = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
        const { controller, videoTrack, notes } = makeClipController();
        controller.handleVideoTrack(videoTrack);
        controller.handleAudioTrack(makeTrack("audio"));
        recorders[0].ondataavailable({ data: chunk("init") });
        recorders[0].ondataavailable({ data: chunk("c1") });
        controller.handleSessionTornDown();
        expect(controller.capture.isActive).toBe(false);
        expect(clickSpy).not.toHaveBeenCalled();
        expect(notes).toHaveLength(0);
    });

    it("缓冲关闭后重新开启即恢复就绪通知", () => {
        const { controller, bufferStates } = makeClipController();
        controller.setEnabled(false);
        expect(localStorage.getItem("vistrel_replay_enabled")).toBe("0");
        controller.setEnabled(true);
        expect(bufferStates.at(-1)).toEqual({ active: false, reason: "等待画面接入" });
    });
});
