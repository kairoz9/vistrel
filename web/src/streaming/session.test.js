import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ConnectStatus, StreamingSession } from "./session.js";

// ---------- 假体：WebSocket / RTCPeerConnection / getStats 报表 ----------

class FakeWebSocket {
    static OPEN = 1;
    static instances = [];

    constructor(url) {
        this.url = url;
        this.readyState = 0;
        this.sent = [];
        this.closeCalls = 0;
        FakeWebSocket.instances.push(this);
    }

    send(data) {
        this.sent.push(data);
    }

    close() {
        this.closeCalls++;
    }
}

class FakeMediaStream {
    constructor(tracks) {
        this.tracks = tracks;
    }
}

class FakeRTCPeerConnection {
    static instances = [];
    constructor(config) {
        this.config = config;
        this.connectionState = "new";
        this.remoteDescription = null;
        this.localDescription = null;
        this.transceivers = [];
        this.dataChannels = [];
        this.setRemoteDescription = vi.fn(async (desc) => {
            this.remoteDescription = desc;
        });
        this.addIceCandidate = vi.fn(async () => {});
        this.createOffer = vi.fn(async () => ({ type: "offer", sdp: "v=0" }));
        this.setLocalDescription = vi.fn(async (desc) => {
            this.localDescription = desc;
        });
        this.getStats = vi.fn(async () => makeStats([]));
        this.close = vi.fn();
        this.addTransceiver = vi.fn((kind, init) => {
            const transceiver = { kind, init, setCodecPreferences: vi.fn() };
            this.transceivers.push(transceiver);
            return transceiver;
        });
        this.createDataChannel = vi.fn((label, opts) => {
            const channel = { label, ...opts, binaryType: null, onopen: null };
            this.dataChannels.push(channel);
            return channel;
        });
        FakeRTCPeerConnection.instances.push(this);
    }
}

function makeStats(reports) {
    const byId = new Map(reports.map((report) => [report.id, report]));
    return { forEach: (callback) => reports.forEach(callback), get: (id) => byId.get(id) };
}

function installStubs() {
    FakeWebSocket.instances = [];
    FakeRTCPeerConnection.instances = [];
    vi.stubGlobal("WebSocket", FakeWebSocket);
    vi.stubGlobal("RTCPeerConnection", FakeRTCPeerConnection);
    vi.stubGlobal("MediaStream", FakeMediaStream);
}

function makeSession({ preferences, getViewport } = {}) {
    const video = document.createElement("video");
    const audio = document.createElement("audio");
    for (const element of [video, audio]) {
        // happy-dom 的 srcObject setter 校验 MediaStream 实例，测试用可写桩位替代
        Object.defineProperty(element, "srcObject", { configurable: true, value: null, writable: true });
        element.play = vi.fn(() => Promise.resolve());
    }
    const session = new StreamingSession({
        videoElement: video,
        audioElement: audio,
        getPreferences: () => ({
            bpp: 0.2,
            encoder: "auto",
            codec: "auto",
            ...preferences,
        }),
        getViewport: getViewport === undefined ? () => ({ width: 1920, height: 1080 }) : getViewport,
    });
    return { session, video, audio };
}

/** 走完整启动路径到 offer 发出：返回会话与其信令通道 */
async function startSession(sessionOptions = {}) {
    const { session, video, audio } = makeSession(sessionOptions);
    session.start("");
    const ws = session.ws;
    ws.readyState = FakeWebSocket.OPEN;
    ws.onopen();
    await flushMicrotasks();
    return { session, video, audio, ws, pc: session.pc };
}

const flushMicrotasks = async () => {
    for (let i = 0; i < 20; i++) await Promise.resolve();
};

beforeEach(() => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
    installStubs();
    // 生产路径的连接日志与错误日志在断言用途外是纯噪声
    vi.spyOn(console, "log").mockImplementation(() => {});
    vi.spyOn(console, "error").mockImplementation(() => {});
    vi.spyOn(console, "warn").mockImplementation(() => {});
    vi.spyOn(console, "debug").mockImplementation(() => {});
});

afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
});

// ---------- 会话启动与 offer 上行 ----------

describe("会话启动", () => {
    it("start 建立 CONNECTING 状态并按 location 推导信令地址", () => {
        const { session } = makeSession();
        const statuses = [];
        session.on("status", (status) => statuses.push(status));
        session.start("");
        expect(statuses).toEqual([ConnectStatus.CONNECTING]);
        expect(session.ws.url).toMatch(/^ws:\/\/[^/]+\/ws$/);
    });

    it("WS 就绪后建 PC、开输入通道并发出 offer", async () => {
        const { session, ws, pc } = await startSession();
        expect(pc.addTransceiver).toHaveBeenCalledWith("video", { direction: "recvonly" });
        expect(pc.addTransceiver).toHaveBeenCalledWith("audio", { direction: "recvonly" });
        expect(pc.createDataChannel).toHaveBeenCalledWith("input", { ordered: false, maxRetransmits: 0 });
        expect(pc.dataChannels[0].binaryType).toBe("arraybuffer");
        expect(pc.setLocalDescription).toHaveBeenCalledTimes(1);
        expect(ws.sent).toHaveLength(1);
        const offer = JSON.parse(ws.sent[0]);
        expect(offer).toEqual({
            type: "offer",
            password: "",
            bpp: 0.2,
            viewport: { width: 1920, height: 1080 },
            sdp: { type: "offer", sdp: "v=0" },
        });
        expect(session.offerSent).toBe(true);
    });

    it("显式 codec 偏好随 offer 携带，自动时不占字段", async () => {
        const { ws } = await startSession({ preferences: { codec: "h265" } });
        expect(JSON.parse(ws.sent[0]).codec).toBe("h265");

        const { ws: autoWs } = await startSession();
        expect(JSON.parse(autoWs.sent[0])).not.toHaveProperty("codec");
    });

    it("SDP 载荷序不再承载偏好：不调用 setCodecPreferences", async () => {
        vi.stubGlobal("RTCRtpReceiver", {
            getCapabilities: () => ({
                codecs: [{ mimeType: "video/H264" }, { mimeType: "video/AV1" }, { mimeType: "video/H265" }],
            }),
        });
        const { pc } = await startSession({ preferences: { codec: "h265" } });
        expect(pc.addTransceiver).toHaveBeenCalledWith("video", { direction: "recvonly" });
        expect(pc.transceivers[0].setCodecPreferences).not.toHaveBeenCalled();
    });
});

// ---------- 信令下行分流 ----------

describe("信令下行分流", () => {
    it("answer（对象形态）生效后认证成功并回填协商档案", async () => {
        const { session } = makeSession();
        session.pc = new FakeRTCPeerConnection({});
        const authEvents = [];
        const profileEvents = [];
        session.on("auth", (result) => authEvents.push(result));
        session.on("profile", (profile) => profileEvents.push(profile));
        await session.handleSignalingMessage(
            JSON.stringify({
                type: "answer",
                sdp: { type: "answer", sdp: "v=0\r\na=rtpmap:96 H265\r\n" },
                backend: "nvenc",
            }),
        );
        expect(session.pc.setRemoteDescription).toHaveBeenCalledWith({ type: "answer", sdp: expect.any(String) });
        expect(authEvents).toEqual([{ ok: true }]);
        expect(profileEvents).toEqual([
            { width: 1920, height: 1080, fps: 60, codec: "h265", backend: "nvenc", backends: null },
        ]);
    });

    it("answer 携带的能力清单原样进入协商档案", async () => {
        const { session } = makeSession();
        session.pc = new FakeRTCPeerConnection({});
        const profileEvents = [];
        session.on("profile", (profile) => profileEvents.push(profile));
        await session.handleSignalingMessage(
            JSON.stringify({
                type: "answer",
                sdp: "v=0\r\na=rtpmap:96 H264\r\n",
                backend: "software",
                backends: [
                    { kind: "nvenc", codecs: ["H264", "H265", "AV1"] },
                    { kind: "software", codecs: ["H264"] },
                ],
            }),
        );
        expect(profileEvents[0]).toEqual({
            width: 1920,
            height: 1080,
            fps: 60,
            codec: "h264",
            backend: "software",
            backends: [
                { kind: "nvenc", codecs: ["H264", "H265", "AV1"] },
                { kind: "software", codecs: ["H264"] },
            ],
        });
    });

    it("answer（字符串形态）与缺省档位回填", async () => {
        const { session } = makeSession();
        session.pc = new FakeRTCPeerConnection({});
        const profileEvents = [];
        session.on("profile", (profile) => profileEvents.push(profile));
        await session.handleSignalingMessage(JSON.stringify({ type: "answer", sdp: "v=0\r\na=rtpmap:96 H264\r\n" }));
        expect(session.pc.setRemoteDescription).toHaveBeenCalledWith({ type: "answer", sdp: expect.any(String) });
        // 应答缺 backend 字段（旧 Native）按未知处理
        expect(profileEvents[0]).toEqual({
            width: 1920,
            height: 1080,
            fps: 60,
            codec: "h264",
            backend: "unknown",
            backends: null,
        });
    });

    it("answer 前到达的候选先存后喂，之后直达", async () => {
        const { session } = makeSession();
        session.pc = new FakeRTCPeerConnection({});
        const early = { candidate: "early" };
        const late = { candidate: "late" };
        await session.handleSignalingMessage(JSON.stringify({ type: "candidate", candidate: early }));
        expect(session.pc.addIceCandidate).not.toHaveBeenCalled();
        await session.handleSignalingMessage(
            JSON.stringify({ type: "answer", sdp: { type: "answer", sdp: "v=0 H264" } }),
        );
        expect(session.pc.addIceCandidate).toHaveBeenCalledWith(early);
        await session.handleSignalingMessage(JSON.stringify({ type: "candidate", candidate: late }));
        expect(session.pc.addIceCandidate).toHaveBeenCalledWith(late);
    });

    it("auth_failed 清凭据、断开资源并停住重连闭环", async () => {
        const { session } = makeSession();
        session.start("pw");
        const ws = session.ws;
        const authEvents = [];
        const tornDown = [];
        session.on("auth", (result) => authEvents.push(result));
        session.on("torn-down", () => tornDown.push(1));
        await session.handleSignalingMessage(JSON.stringify({ type: "auth_failed" }));
        expect(authEvents).toEqual([{ ok: false, message: "密码错误，请重新输入" }]);
        expect(tornDown).toHaveLength(1);
        expect(ws.closeCalls).toBe(1);
        // 通道随后关闭也不触发重连
        ws.onclose({ code: 1006 });
        await vi.advanceTimersByTimeAsync(5000);
        expect(FakeWebSocket.instances).toHaveLength(1);
    });

    it("信令 error 视为可恢复：拒绝原因上事件、转 DISCONNECTED 并关闭信令连接", async () => {
        const { session } = makeSession();
        // 与 main.js 一致：先订阅再启动，首批状态事件不丢
        const statuses = [];
        const errors = [];
        session.on("status", (status) => statuses.push(status));
        session.on("error", (message) => errors.push(message));
        session.start("");
        const ws = session.ws;
        ws.readyState = FakeWebSocket.OPEN;
        await session.handleSignalingMessage(JSON.stringify({ type: "error", message: "指定的 codec 不可行" }));
        expect(errors).toEqual(["指定的 codec 不可行"]);
        expect(statuses).toEqual([ConnectStatus.CONNECTING, ConnectStatus.DISCONNECTED]);
        expect(ws.closeCalls).toBe(1);
    });

    it("控制通道三类动作转 action 事件", async () => {
        const { session } = makeSession();
        const actions = [];
        session.on("action", (action) => actions.push(action));
        await session.handleSignalingMessage(JSON.stringify({ type: "cursor_state", visible: true, x: 5, y: 6 }));
        await session.handleSignalingMessage(JSON.stringify({ type: "open_url", value: "https://example.com" }));
        await session.handleSignalingMessage(JSON.stringify({ type: "clipboard_write", value: "text" }));
        await session.handleSignalingMessage(JSON.stringify({ type: "open_url", value: 42 }));
        expect(actions).toEqual([
            { type: "cursor", visible: true, x: 5, y: 6 },
            { type: "open-url", url: "https://example.com" },
            { type: "clipboard", text: "text" },
        ]);
    });

    it("坏 JSON 吞掉不外泄", async () => {
        const { session } = makeSession();
        const events = [];
        session.on("action", (action) => events.push(action));
        await expect(session.handleSignalingMessage("{bad json")).resolves.toBeUndefined();
        expect(events).toHaveLength(0);
    });
});

// ---------- 上行消息 ----------

describe("上行消息", () => {
    it("trySendOffer 幂等且依赖通道就绪", async () => {
        const { session, ws } = await startSession();
        session.trySendOffer();
        expect(ws.sent).toHaveLength(1);
        // 通道未就绪
        ws.readyState = 0;
        session.offerSent = false;
        session.trySendOffer();
        expect(ws.sent).toHaveLength(1);
    });

    it("非自动的后端偏好随 offer 携带，自动时不占字段", async () => {
        const { ws } = await startSession({ preferences: { encoder: "nvenc" } });
        expect(JSON.parse(ws.sent[0]).backend).toBe("nvenc");

        const { ws: autoWs } = await startSession();
        expect(JSON.parse(autoWs.sent[0])).not.toHaveProperty("backend");
    });

    it("sendSettings 携带 bpp 与视口上限，视口为空则省略", async () => {
        const { session, ws } = await startSession();
        session.sendSettings();
        expect(JSON.parse(ws.sent.at(-1))).toEqual({
            type: "settings",
            bpp: 0.2,
            viewport: { width: 1920, height: 1080 },
        });
        session.getViewport = () => null;
        session.sendSettings();
        const payload = JSON.parse(ws.sent.at(-1));
        expect(payload).not.toHaveProperty("viewport");
    });

    it("偏好束缺 bpp 时 settings 回退 0.1", async () => {
        const { session, ws } = await startSession({
            // 缺键不覆盖：readPreferences 的 DEFAULT 合并兜出 0.1
            preferences: { bpp: undefined },
        });
        session.sendSettings();
        expect(JSON.parse(ws.sent.at(-1)).bpp).toBe(0.1);
    });

    it("sendSettings 与 sendTextInput 在通道未开时静默", () => {
        const { session } = makeSession();
        session.sendSettings();
        session.sendTextInput("hello");
        expect(FakeWebSocket.instances).toHaveLength(0);
    });

    it("sendTextInput 经信令通道下发文本", async () => {
        const { session, ws } = await startSession();
        session.sendTextInput("hello");
        expect(JSON.parse(ws.sent.at(-1))).toEqual({ type: "text_input", text: "hello" });
    });
});

// ---------- 重连与踢出 ----------

describe("重连闭环", () => {
    it("非踢出断开 3 秒后以原凭据重入", async () => {
        const { session } = makeSession();
        const statuses = [];
        session.on("status", (status) => statuses.push(status));
        session.start("pw");
        session.ws.onclose({ code: 1006 });
        expect(statuses).toEqual([ConnectStatus.CONNECTING, ConnectStatus.CLOSED]);
        await vi.advanceTimersByTimeAsync(3000);
        expect(FakeWebSocket.instances).toHaveLength(2);
        expect(statuses.at(-1)).toBe(ConnectStatus.CONNECTING);
    });

    it("4001 被抢占：发 kicked 事件且不再重连", async () => {
        const { session } = makeSession();
        const kicked = [];
        session.on("kicked", () => kicked.push(1));
        session.start("pw");
        session.ws.onclose({ code: 4001 });
        expect(kicked).toHaveLength(1);
        expect(session.isKicked).toBe(true);
        await vi.advanceTimersByTimeAsync(10_000);
        expect(FakeWebSocket.instances).toHaveLength(1);
    });

    it("认证失败后断开不重连", async () => {
        const { session } = makeSession();
        session.start("pw");
        await session.handleSignalingMessage(JSON.stringify({ type: "auth_failed" }));
        session.ws = FakeWebSocket.instances[0];
        session.ws.onclose({ code: 1006 });
        await vi.advanceTimersByTimeAsync(5000);
        expect(FakeWebSocket.instances).toHaveLength(1);
    });
});

// ---------- PeerConnection 状态与轨道 ----------

describe("PeerConnection 状态与轨道", () => {
    it("connected/failed/disconnected 驱动状态机", async () => {
        const { session, ws, pc } = await startSession();
        const statuses = [];
        session.on("status", (status) => statuses.push(status));
        pc.connectionState = "connected";
        pc.onconnectionstatechange();
        pc.connectionState = "disconnected";
        pc.onconnectionstatechange();
        expect(statuses).toEqual([ConnectStatus.CONNECTED, ConnectStatus.DISCONNECTED]);
        pc.connectionState = "failed";
        pc.onconnectionstatechange();
        expect(statuses.at(-1)).toBe(ConnectStatus.CLOSED);
        expect(ws.closeCalls).toBe(1);
    });

    it("视频轨接入画面元素并发事件", async () => {
        const { session, video } = await startSession();
        const tracks = [];
        session.on("video-track", (track) => tracks.push(track));
        const stream = { id: "stream" };
        session.pc.ontrack({ track: { kind: "video" }, receiver: {}, streams: [stream] });
        expect(tracks).toHaveLength(1);
        expect(video.srcObject).toBe(stream);
        expect(video.play).toHaveBeenCalled();
    });

    it("无流轨道回退到自建 MediaStream", async () => {
        const { session, audio } = await startSession();
        const tracks = [];
        session.on("audio-track", (track) => tracks.push(track));
        session.pc.ontrack({ track: { kind: "audio" }, receiver: {}, streams: [] });
        expect(tracks).toHaveLength(1);
        expect(audio.srcObject).toBeInstanceOf(Object);
        expect(audio.srcObject.tracks).toEqual([{ kind: "audio" }]);
    });

    it("cleanupPeer 拆除资源并通知收尾", async () => {
        const { session, pc, video } = await startSession();
        const tornDown = [];
        session.on("torn-down", () => tornDown.push(1));
        session.cleanupPeer();
        expect(tornDown).toHaveLength(1);
        expect(pc.close).toHaveBeenCalled();
        expect(video.srcObject).toBeNull();
        expect(session.pc).toBeNull();
    });
});

// ---------- 统计轮询 ----------

describe("统计轮询", () => {
    /** 逐轮 getStats 报表：帧数/字节/qp 增量推导 fps、码率与 QP */
    function installStatsRounds(session, rounds) {
        let call = 0;
        session.pc.getStats = vi.fn(async () => rounds[Math.min(call++, rounds.length - 1)]);
    }

    const pairReport = { id: "pair", type: "candidate-pair", currentRoundTripTime: 0.05 };
    const codecReport = { id: "codec", type: "codec", mimeType: "video/H265" };

    function inboundReport(overrides) {
        return {
            id: "inbound",
            type: "inbound-rtp",
            kind: "video",
            packetsLost: 2,
            packetsReceived: 500,
            jitterBufferDelay: 0.005,
            jitterBufferEmittedCount: 1,
            codecId: "codec",
            ...overrides,
        };
    }

    it("fps/码率/QP 均由增量推导，流重启的负增量跳过", async () => {
        const { session } = makeSession();
        session.pc = new FakeRTCPeerConnection({});
        installStatsRounds(session, [
            makeStats([
                inboundReport({ timestamp: 1000, framesDecoded: 60, bytesReceived: 100_000, qpSum: 3_000 }),
                pairReport,
                codecReport,
            ]),
            makeStats([
                inboundReport({ timestamp: 2000, framesDecoded: 120, bytesReceived: 200_000, qpSum: 6_000 }),
                pairReport,
                codecReport,
            ]),
            makeStats([
                inboundReport({ timestamp: 3000, framesDecoded: 120, bytesReceived: 50_000, qpSum: 1_000 }),
                pairReport,
                codecReport,
            ]),
        ]);
        const events = [];
        session.on("stats", (stats) => events.push(stats));
        session.startStatsPolling();
        await vi.advanceTimersByTimeAsync(1000);
        await vi.advanceTimersByTimeAsync(1000);
        await vi.advanceTimersByTimeAsync(1000);
        expect(events[0]).toEqual({
            rttMs: 50,
            fps: null,
            diagText: "丢包 2 (0.4%) · 丢帧 -- · 抖动缓冲 5ms",
            actualCodec: "h265",
        });
        expect(events[1]).toEqual({
            rttMs: 50,
            fps: 60,
            diagText: "QP 50 · 丢包 2 (0.4%) · 丢帧 -- · 抖动缓冲 5ms",
            actualCodec: "h265",
        });
        expect(events[2]).toEqual({
            rttMs: 50,
            fps: 0,
            diagText: "丢包 2 (0.4%) · 丢帧 -- · 抖动缓冲 5ms",
            actualCodec: "h265",
        });
        expect(session.getInboundBitrateEstimate()).toBe(800_000);
    });

    it("丢帧增量并入诊断行", async () => {
        const { session } = makeSession();
        session.pc = new FakeRTCPeerConnection({});
        installStatsRounds(session, [
            makeStats([
                inboundReport({ timestamp: 1000, framesDecoded: 60, framesDropped: 0 }),
                pairReport,
                codecReport,
            ]),
            makeStats([
                inboundReport({ timestamp: 2000, framesDecoded: 120, framesDropped: 3 }),
                pairReport,
                codecReport,
            ]),
        ]);
        const events = [];
        session.on("stats", (stats) => events.push(stats));
        session.startStatsPolling();
        await vi.advanceTimersByTimeAsync(1000);
        await vi.advanceTimersByTimeAsync(1000);
        expect(events[0].diagText).toBe("丢包 2 (0.4%) · 丢帧 0 · 抖动缓冲 5ms");
        expect(events[1]).toMatchObject({
            fps: 60,
            diagText: "丢包 2 (0.4%) · 丢帧 3 · 抖动缓冲 5ms",
        });
    });

    it("采样在途时跳过新轮询，拆除后旧采样结果作废", async () => {
        const { session } = makeSession();
        session.pc = new FakeRTCPeerConnection({});
        let release;
        const pending = new Promise((resolve) => {
            release = resolve;
        });
        session.pc.getStats = vi.fn(() => pending);
        const events = [];
        session.on("stats", (stats) => events.push(stats));

        session.startStatsPolling();
        vi.advanceTimersByTime(1000);
        vi.advanceTimersByTime(1000);
        expect(session.pc.getStats).toHaveBeenCalledTimes(1);

        session.cleanupPeer();
        release(makeStats([inboundReport({ timestamp: 1000, framesDecoded: 60 }), pairReport, codecReport]));
        await flushMicrotasks();
        expect(events).toHaveLength(0);
    });
});

// ---------- 事件总线 ----------

describe("事件总线", () => {
    it("单个订阅者抛错不影响其余订阅者", () => {
        const { session } = makeSession();
        const seen = [];
        session.on("status", () => {
            throw new Error("boom");
        });
        session.on("status", (status) => seen.push(status));
        expect(() => session.setStatus(ConnectStatus.CONNECTING)).not.toThrow();
        expect(seen).toEqual([ConnectStatus.CONNECTING]);
    });
});
