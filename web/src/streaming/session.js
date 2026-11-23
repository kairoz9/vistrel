import { WS_IN, WS_OUT, CONTROL_IN, KICKED_CODE, parseCodecToken } from "./protocol.js";

/** 偏好束读取不可用（UI 层异常等）时的兜底：全部按自动档协商 */
const DEFAULT_PREFERENCES = Object.freeze({
    bpp: 0.1,
    encoder: "auto",
    codec: "auto",
});

/**
 * 连接状态
 */
export const ConnectStatus = Object.freeze({
    CONNECTING: "connecting",
    CONNECTED: "connected",
    DISCONNECTED: "disconnected",
    CLOSED: "closed",
});

/**
 * 会话对外事件词汇表：on() 的订阅锚点，emit 端与订阅端同源引用
 */
export const SESSION_EVENTS = Object.freeze({
    STATUS: "status",
    PROFILE: "profile",
    STATS: "stats",
    AUTH: "auth",
    KICKED: "kicked",
    VIDEO_TRACK: "video-track",
    AUDIO_TRACK: "audio-track",
    ACTION: "action",
    TORN_DOWN: "torn-down",
    /** 服务端下发的信令错误（含编码偏好不可满足的拒绝原因），负载为错误文案 */
    ERROR: "error",
    /** 当前生效显示几何 {width, height}：Answer 给初值、服务端推送更新，不含编码对齐补齐边 */
    DISPLAY_GEOMETRY: "display_geometry",
});

/**
 * 页面加载期预生成的共享 DTLS 证书：把密钥生成从连接关键路径挪到页面加载期，
 * 重连复用同一证书；不可用时退回浏览器自动生成
 */
let sharedCertificate = null;
if (typeof RTCPeerConnection !== "undefined" && RTCPeerConnection.generateCertificate) {
    RTCPeerConnection.generateCertificate({ name: "ECDSA", namedCurve: "P-256" })
        .then((cert) => {
            sharedCertificate = cert;
        })
        .catch((e) => {
            console.log(e);
        });
}

/**
 * 静默关闭连接资源：拆除路径上对端可能已死，关闭异常一律吞掉
 */
function closeQuietly(connection) {
    if (connection) {
        try {
            connection.close();
        } catch (_) {}
    }
}

/** 合法显示几何：正有限宽高（Answer 初值与 display_geometry 更新共用同一判定） */
function isValidGeometry(width, height) {
    return Number.isFinite(width) && Number.isFinite(height) && width > 0 && height > 0;
}

/**
 * Vistrel 浏览器侧串流会话：信令协商、PeerConnection 生命周期与统计轮询
 */
export class StreamingSession {
    /**
     * @param {Object} options
     * @param {HTMLVideoElement} options.videoElement
     * @param {HTMLAudioElement} options.audioElement
     * @param {() => {bpp: number, encoder: string, codec: string}} options.getPreferences
     *        串流偏好束（设置面板的持久化选择）的现读快照：每次建连现读，改完偏好在
     *        下次连接生效。显示适配是纯呈现，不属会话偏好
     * @param {() => ({width: number, height: number} | null)} [options.getViewport]
     *        视频显示区物理像素上限（CSS × DPR 已在 UI 层算好）：即时测量非偏好，
     *        每次发送现读，自动上限输入
     */
    constructor({ videoElement, audioElement, getPreferences, getViewport }) {
        this.video = videoElement;
        this.audio = audioElement;
        this.getPreferences = getPreferences || (() => DEFAULT_PREFERENCES);
        this.getViewport = getViewport || (() => null);

        // 对外输出全走订阅：状态是会话事实（存字段并发事件），游戏侧动作是边沿
        this.listeners = new Map();
        this.status = null;
        this.auth = "pending";
        this.profile = {
            codec: parseCodecToken(this.readPreferences().codec),
            backend: "unknown",
            backends: null,
        };

        this.pc = null;
        this.ws = null;
        this.dataChannel = null;
        // answer 到达前收到的服务端候选先存这里，setRemoteDescription 之后再现喂
        this.pendingIceCandidates = [];

        // 接收轨道引用与入流码率估算，供播放流录制消费
        this.videoTrack = null;
        this.audioTrack = null;
        this.inboundBitrateBps = null;
        this.lastInboundBytes = 0;
        this.lastInboundBytesTs = 0;

        this.currentPassword = "";
        this.isKicked = false;
        this.isAuthFailed = false;
        this.offerSent = false;
        this.statsInterval = null;
        this.statsBusyPc = null;
        this.reconnectTimer = null;

        this.lastDecodedFrames = 0;
        this.lastTimestamp = 0;
    }

    /**
     * 订阅会话事件；页面生命周期内只订阅一次，故不提供退订句柄。
     * 事件词汇表见 SESSION_EVENTS
     */
    on(type, handler) {
        const list = this.listeners.get(type) || [];
        list.push(handler);
        this.listeners.set(type, list);
    }

    /** 分发：单个订阅者抛错不影响其他订阅者，也不中断推送方的控制流 */
    emit(type, payload) {
        const list = this.listeners.get(type);
        if (!list) return;
        for (const handler of list) {
            try {
                handler(payload);
            } catch (e) {
                console.error(`Vistrel: handler for '${type}' failed:`, e);
            }
        }
    }

    setStatus(status) {
        this.status = status;
        this.emit(SESSION_EVENTS.STATUS, status);
    }

    /**
     * 协商档案：codec / backend / backends 是 Answer 落定的会话常量；
     * 显示几何不在档案里，由 DISPLAY_GEOMETRY 事件单独携带
     */
    setProfile(codec, backend, backends) {
        this.profile = {
            codec,
            backend: backend || "unknown",
            backends: Array.isArray(backends) ? backends : null,
        };
        this.emit(SESSION_EVENTS.PROFILE, this.profile);
    }

    setAuth(result) {
        this.auth = result.ok ? "ok" : "failed";
        this.emit(SESSION_EVENTS.AUTH, result);
    }

    getInputDataChannel() {
        return this.dataChannel;
    }

    /** 入流视频码率估算 (bps)，无有效样本时为 null */
    getInboundBitrateEstimate() {
        return this.inboundBitrateBps;
    }

    start(password) {
        if (this.isKicked) return;
        this.isAuthFailed = false;
        this.auth = "pending";
        this.offerSent = false;
        this.currentPassword = password;
        this.clearReconnectTimer();

        closeQuietly(this.ws);
        this.ws = null;
        this.cleanupPeer();

        this.setStatus(ConnectStatus.CONNECTING);

        const wsProtocol = location.protocol === "https:" ? "wss:" : "ws:";
        this.ws = new WebSocket(`${wsProtocol}//${location.host}/ws`);

        // WS 就绪后才构造 PC 并生成 Offer：所有发送点天然晚于连接建立，无跨事件竞态
        this.ws.onopen = () => {
            // 此刻尚未认证（认证随 offer 完成），settings 会被服务端忽略——
            // bpp 与视口由 offer 原样携带，连接建立即生效，无需单独同步
            this.createPeerConnection();
        };

        this.ws.onmessage = async (event) => {
            await this.handleSignalingMessage(event.data);
        };

        this.ws.onclose = (event) => {
            if (this.isKicked || this.isAuthFailed) return;
            this.setStatus(ConnectStatus.CLOSED);
            this.cleanupPeer();
            if (event && event.code === KICKED_CODE) {
                this.isKicked = true;
                this.emit(SESSION_EVENTS.KICKED);
                return;
            }
            this.clearReconnectTimer();
            this.reconnectTimer = setTimeout(() => {
                if (!this.isKicked && !this.isAuthFailed) {
                    this.start(this.currentPassword);
                }
            }, 3000);
        };

        this.ws.onerror = (err) => {
            console.error("Vistrel: WebSocket Error:", err);
        };
    }

    trySendOffer() {
        if (this.offerSent) return;
        // 顺序结构下 WS 必已开启，此判断仅防御 onopen 后连接瞬断的窗口
        if (this.ws && this.ws.readyState === WebSocket.OPEN && this.pc && this.pc.localDescription) {
            this.offerSent = true;
            // 分辨率与帧率是服务端从游戏内容推导的事实：客户端只带画质档位、视口上限
            // 与编码偏好，绝不携带 width/height/fps；自动偏好不占字段（缺省即自动）
            const { bpp, encoder, codec } = this.readPreferences();
            const payload = {
                type: WS_OUT.OFFER,
                password: this.currentPassword,
                bpp,
                viewport: this.getViewport(),
                sdp: this.pc.localDescription,
            };
            if (encoder && encoder !== "auto") {
                payload.backend = encoder;
            }
            if (codec && codec !== "auto") {
                payload.codec = codec;
            }
            this.ws.send(JSON.stringify(payload));
        }
    }

    /** 现读偏好束：注入方只做字段读取，字段缺省逐项回退默认值 */
    readPreferences() {
        const merged = { ...DEFAULT_PREFERENCES };
        for (const [key, value] of Object.entries(this.getPreferences() ?? {})) {
            if (value !== undefined) merged[key] = value;
        }
        return merged;
    }

    /**
     * 客户端串流偏好同步：幂等状态快照，尽量携带完整 bpp + viewport。
     * WebSocket 开启、防抖 resize、画质滑杆变更都走这里；
     * 服务端按字段分发（视口只存上限，BPP 触发后台 ask），等值无副作用
     */
    sendSettings() {
        if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
        const payload = { type: WS_OUT.SETTINGS, bpp: this.readPreferences().bpp };
        const viewport = this.getViewport();
        if (viewport) payload.viewport = viewport;
        this.ws.send(JSON.stringify(payload));
    }

    async handleSignalingMessage(rawData) {
        try {
            const msg = JSON.parse(rawData);
            if (msg.type === WS_IN.ANSWER) {
                const rawSdp = msg.sdp && msg.sdp.sdp ? msg.sdp.sdp : typeof msg.sdp === "string" ? msg.sdp : "";
                const ackCodec = parseCodecToken(rawSdp);
                const desc = msg.sdp && msg.sdp.sdp ? msg.sdp : { type: "answer", sdp: msg.sdp || msg };
                await this.pc.setRemoteDescription(desc);

                // 服务端 trickle 的候选可能先于 answer 到达：此刻才能嗂给 addIceCandidate
                for (const candidate of this.pendingIceCandidates.splice(0)) {
                    await this.pc.addIceCandidate(candidate);
                }

                // SDP 生效后才算认证成功：避免 setRemoteDescription 失败时提前持久化凭据
                this.setAuth({ ok: true });
                this.setProfile(ackCodec, msg.backend, msg.backends);
                console.log(`Vistrel: Negotiation complete: ${msg.width}x${msg.height}@${msg.fps} (${ackCodec})`);
                // 显示几何初值随 Answer 下发，后续变化由服务端推送
                if (isValidGeometry(msg.width, msg.height)) {
                    this.emit(SESSION_EVENTS.DISPLAY_GEOMETRY, { width: msg.width, height: msg.height });
                }
            } else if (msg.type === WS_IN.CANDIDATE && msg.candidate) {
                if (this.pc && this.pc.remoteDescription) {
                    await this.pc.addIceCandidate(msg.candidate);
                } else {
                    // 远端描述未就位时 addIceCandidate 会抛 InvalidStateError，先存后喂
                    this.pendingIceCandidates.push(msg.candidate);
                }
            } else if (msg.type === WS_IN.AUTH_FAILED) {
                this.isAuthFailed = true;
                this.currentPassword = "";
                this.clearReconnectTimer();
                this.cleanupPeer();
                closeQuietly(this.ws);
                this.ws = null;
                this.setAuth({ ok: false, message: msg.message || "密码错误，请重新输入" });
            } else if (msg.type === WS_IN.ERROR) {
                const message = msg.message || "unknown";
                console.error("Vistrel: Signaling error from server:", message);
                // 拒绝原因（如编码偏好不可满足）必须上屏：否则硬拒绝只是无声的重连循环
                this.emit(SESSION_EVENTS.ERROR, message);
                this.setStatus(ConnectStatus.DISCONNECTED);
                // 信令错误视为可恢复：关闭信令连接，走 onclose 的 3 秒重连闭环
                if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                    this.ws.close();
                }
            } else if (msg.type === WS_IN.DISPLAY_GEOMETRY) {
                // 换档后的显示几何更新
                if (isValidGeometry(msg.width, msg.height)) {
                    this.emit(SESSION_EVENTS.DISPLAY_GEOMETRY, { width: msg.width, height: msg.height });
                }
            } else if (msg.type === CONTROL_IN.CURSOR_STATE) {
                this.emit(SESSION_EVENTS.ACTION, {
                    type: "cursor",
                    visible: !!msg.visible,
                    x: msg.x || 0,
                    y: msg.y || 0,
                });
            } else if (msg.type === CONTROL_IN.OPEN_URL) {
                if (typeof msg.value === "string") {
                    this.emit(SESSION_EVENTS.ACTION, { type: "open-url", url: msg.value });
                }
            } else if (msg.type === CONTROL_IN.CLIPBOARD_WRITE) {
                if (typeof msg.value === "string") {
                    this.emit(SESSION_EVENTS.ACTION, { type: "clipboard", text: msg.value });
                }
            }
        } catch (e) {
            console.error("Vistrel: Error handling signaling message:", e);
        }
    }

    createPeerConnection() {
        const config = { iceServers: [] };
        if (sharedCertificate) {
            config.certificates = [sharedCertificate];
        }
        this.pc = new RTCPeerConnection(config);

        // codec 偏好随 offer 具名字段上行；SDP 载荷序只承载浏览器解码可行性（默认全量提供）
        this.pc.addTransceiver("video", { direction: "recvonly" });

        this.pc.addTransceiver("audio", { direction: "recvonly" });

        this.pc.onconnectionstatechange = () => {
            if (this.isKicked || this.isAuthFailed || !this.pc) return;
            const state = this.pc.connectionState;
            console.log("Vistrel: PeerConnection state:", state);
            if (state === "connected") {
                this.setStatus(ConnectStatus.CONNECTED);
            } else if (state === "failed" || state === "closed") {
                if (this.isKicked || this.isAuthFailed) return;
                this.setStatus(ConnectStatus.CLOSED);
                if (this.ws && this.ws.readyState === WebSocket.OPEN) {
                    this.ws.close();
                }
            } else if (state === "disconnected") {
                if (this.isKicked || this.isAuthFailed) return;
                this.setStatus(ConnectStatus.DISCONNECTED);
            }
        };

        // 数据通道: 键盘鼠标输入
        this.dataChannel = this.pc.createDataChannel("input", {
            ordered: false,
            maxRetransmits: 0,
        });
        this.dataChannel.binaryType = "arraybuffer";
        this.dataChannel.onopen = () => console.log("Vistrel: Input DataChannel open");

        // 媒体轨道接收：分离音视频渲染管道
        this.pc.ontrack = (event) => {
            console.log("Vistrel: Received remote track:", event.track.kind, event.track);
            if (event.track.kind === "video") {
                this.videoTrack = event.track;
                this.emit(SESSION_EVENTS.VIDEO_TRACK, event.track);
                if (event.streams && event.streams[0]) {
                    this.video.srcObject = event.streams[0];
                } else {
                    this.video.srcObject = new MediaStream([event.track]);
                }
                this.video.play().catch((e) => console.warn("Vistrel: video.play() failed:", e));
            } else if (event.track.kind === "audio") {
                this.audioTrack = event.track;
                this.emit(SESSION_EVENTS.AUDIO_TRACK, event.track);
                if (this.audio) {
                    if (event.streams && event.streams[0]) {
                        this.audio.srcObject = event.streams[0];
                    } else {
                        this.audio.srcObject = new MediaStream([event.track]);
                    }
                    this.audio.play().catch((e) => console.debug("Vistrel: audio.play() deferred:", e));
                }
            }

            const receiver = event.receiver;
            if (receiver && "jitterBufferTarget" in receiver) {
                receiver.jitterBufferTarget = 1;
            }
            if (receiver && "playoutDelayHint" in receiver) {
                receiver.playoutDelayHint = 0;
            }
        };

        this.pc.onicecandidate = (event) => {
            if (event.candidate && this.ws && this.ws.readyState === WebSocket.OPEN) {
                this.ws.send(
                    JSON.stringify({
                        type: WS_OUT.CANDIDATE,
                        candidate: event.candidate,
                    }),
                );
            }
        };

        this.pc
            .createOffer()
            .then((offer) => {
                return this.pc.setLocalDescription(offer);
            })
            .then(() => {
                this.trySendOffer();
                this.startStatsPolling();
            })
            .catch((err) => {
                console.error("Vistrel: Failed to createOffer / setLocalDescription:", err);
            });
    }

    /**
     * GUI 态输入文本经信令 WS 下发：可靠有序 TCP，聊天/文本框输入专用
     * （键码走不可靠 input DataChannel，文本丢字不可接受，故分流）
     */
    sendTextInput(text) {
        if (this.ws && this.ws.readyState === WebSocket.OPEN) {
            this.ws.send(JSON.stringify({ type: WS_OUT.TEXT_INPUT, text: text }));
        }
    }

    startStatsPolling() {
        this.lastDecodedFrames = 0;
        this.lastTimestamp = 0;
        this.lastQpSum = null;
        this.lastFramesDropped = null;

        if (this.statsInterval) {
            clearInterval(this.statsInterval);
        }

        this.statsInterval = setInterval(async () => {
            // 上一轮采样仍在途时跳过本轮：getStats 超窗（主线程饥饿）会把两轮增量交错写坏
            const pc = this.pc;
            if (!pc || this.statsBusyPc === pc) return;
            this.statsBusyPc = pc;
            let stats;
            try {
                stats = await pc.getStats();
            } catch (e) {
                // 采样在途期间 pc 被关闭时 getStats 以 InvalidStateError 拒绝
                console.debug("Vistrel: stats sampling skipped:", e);
                return;
            } finally {
                // 只清理本轮自己的标记：重连后新采样可能已占用标记，旧轮无权清除
                if (this.statsBusyPc === pc) this.statsBusyPc = null;
            }
            // 采样在途期间已拆除或重连重建：旧 pc 的统计作废
            if (pc !== this.pc) return;
            let currentFps = null;
            let currentDiag = null;
            let currentRtt = null;
            let currentQp = null;
            let actualCodecId = null;
            let actualCodec = null;
            stats.forEach((report) => {
                if (report.type === "inbound-rtp" && report.kind === "video") {
                    const timeDiff = this.lastTimestamp > 0 ? (report.timestamp - this.lastTimestamp) / 1000 : 0;
                    let framesDiff = 0;
                    if (timeDiff > 0) {
                        framesDiff = (report.framesDecoded || 0) - this.lastDecodedFrames;
                        currentFps = Math.round(framesDiff / timeDiff);
                    }
                    this.lastDecodedFrames = report.framesDecoded || 0;
                    this.lastTimestamp = report.timestamp;

                    // 收端丢弃/未能解码的帧
                    let droppedDiff = null;
                    if (typeof report.framesDropped === "number") {
                        droppedDiff = 0;
                        if (this.lastFramesDropped !== null) {
                            const delta = report.framesDropped - this.lastFramesDropped;
                            if (delta >= 0) droppedDiff = delta;
                        }
                        this.lastFramesDropped = report.framesDropped;
                    }

                    // 入流码率估算：bytesReceived 增量换算，供录制码率派生
                    if (typeof report.bytesReceived === "number") {
                        if (this.lastInboundBytesTs > 0) {
                            const dtSec = (report.timestamp - this.lastInboundBytesTs) / 1000;
                            const byteDelta = report.bytesReceived - this.lastInboundBytes;
                            // 流重启时计数归零，负增量跳过本轮
                            if (dtSec > 0 && byteDelta >= 0) {
                                this.inboundBitrateBps = Math.round((byteDelta * 8) / dtSec);
                            }
                        }
                        this.lastInboundBytes = report.bytesReceived;
                        this.lastInboundBytesTs = report.timestamp;
                    }

                    // 平均量化参数：qpSum 是逐帧 QP 的累计和，取增量除以帧数
                    if (typeof report.qpSum === "number") {
                        if (this.lastQpSum !== null && framesDiff > 0) {
                            const qpDelta = report.qpSum - this.lastQpSum;
                            // 流重启时 qpSum 归零，增量为负则跳过本轮
                            if (qpDelta >= 0) {
                                currentQp = Math.round(qpDelta / framesDiff);
                            }
                        }
                        this.lastQpSum = report.qpSum;
                    }

                    const lost = report.packetsLost || 0;
                    const received = report.packetsReceived || 0;
                    const total = lost + received;
                    const lossPct = total ? ((lost * 100) / total).toFixed(1) : "0.0";
                    const jbMs = report.jitterBufferEmittedCount
                        ? Math.round((report.jitterBufferDelay / report.jitterBufferEmittedCount) * 1000)
                        : 0;

                    // QP 作为画质指标并入诊断行；qpSum 由浏览器解码端解析码流本地统计，
                    // AV1/硬解等路径不提供时整段省略。
                    const qpPart = currentQp === null ? "" : `QP ${currentQp} · `;
                    const droppedText = droppedDiff === null ? "--" : droppedDiff;
                    currentDiag = `${qpPart}丢包 ${lost} (${lossPct}%) · 丢帧 ${droppedText} · 抖动缓冲 ${jbMs}ms`;
                    if (report.codecId) {
                        actualCodecId = report.codecId;
                    }
                }
                if (report.type === "candidate-pair" && report.currentRoundTripTime) {
                    currentRtt = Math.round(report.currentRoundTripTime * 1000);
                }
            });

            if (actualCodecId) {
                const codecStat = stats.get(actualCodecId);
                if (codecStat) {
                    actualCodec = parseCodecToken(codecStat.mimeType);
                }
            }

            this.emit(SESSION_EVENTS.STATS, {
                rttMs: currentRtt,
                fps: currentFps,
                diagText: currentDiag,
                actualCodec: actualCodec,
            });
        }, 1000);
    }

    clearReconnectTimer() {
        if (this.reconnectTimer) {
            clearTimeout(this.reconnectTimer);
            this.reconnectTimer = null;
        }
    }

    cleanupPeer() {
        // 拆除前先通知：录制器趁轨道仍存活完成收尾保存
        this.emit(SESSION_EVENTS.TORN_DOWN);
        this.clearReconnectTimer();
        if (this.statsInterval) {
            clearInterval(this.statsInterval);
            this.statsInterval = null;
        }
        closeQuietly(this.dataChannel);
        this.dataChannel = null;
        closeQuietly(this.pc);
        this.pc = null;
        this.pendingIceCandidates = [];
        this.videoTrack = null;
        this.audioTrack = null;
        this.inboundBitrateBps = null;
        this.lastInboundBytes = 0;
        this.lastInboundBytesTs = 0;
        if (this.video) {
            this.video.srcObject = null;
        }
        if (this.audio) {
            this.audio.srcObject = null;
        }
    }

    resetKicked() {
        this.isKicked = false;
        this.isAuthFailed = false;
    }
}
