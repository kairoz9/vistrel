import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { copyTextWithFallback, fileStamp, formatDuration, formatSize } from "./utils.js";

describe("fileStamp", () => {
    beforeEach(() => {
        vi.useFakeTimers({ toFake: ["Date"] });
    });

    afterEach(() => {
        vi.useRealTimers();
    });

    it("按 YYYY-MM-DD_HH-MM-SS 形态输出", () => {
        vi.setSystemTime(new Date(2026, 9, 31, 18, 6, 28));
        expect(fileStamp()).toBe("2026-10-31_18-06-28");
    });

    it("个位数月/日/时/分/秒一律补零", () => {
        vi.setSystemTime(new Date(2026, 0, 5, 3, 7, 9));
        expect(fileStamp()).toBe("2026-01-05_03-07-09");
    });
});

describe("formatDuration", () => {
    it("秒转 分:秒", () => {
        expect(formatDuration(0)).toBe("00:00");
        expect(formatDuration(59)).toBe("00:59");
        expect(formatDuration(60)).toBe("01:00");
        expect(formatDuration(3599)).toBe("59:59");
    });

    it("超一小时转为 时:分:秒", () => {
        expect(formatDuration(3600)).toBe("1:00:00");
        expect(formatDuration(3661)).toBe("1:01:01");
    });

    it("负数与小数向下收敛", () => {
        expect(formatDuration(-5)).toBe("00:00");
        expect(formatDuration(0.9)).toBe("00:00");
        expect(formatDuration(60.9)).toBe("01:00");
    });
});

describe("formatSize", () => {
    it("KB 段取整", () => {
        expect(formatSize(0)).toBe("0 KB");
        expect(formatSize(511)).toBe("0 KB");
        expect(formatSize(1024)).toBe("1 KB");
        expect(formatSize(2048)).toBe("2 KB");
    });

    it("MB 段一位小数", () => {
        expect(formatSize(1024 ** 2)).toBe("1.0 MB");
        expect(formatSize(1.5 * 1024 ** 2)).toBe("1.5 MB");
    });

    it("GB 段两位小数", () => {
        expect(formatSize(1024 ** 3)).toBe("1.00 GB");
        expect(formatSize(1.5 * 1024 ** 3)).toBe("1.50 GB");
    });

    it("负数收敛到 0 KB", () => {
        expect(formatSize(-100)).toBe("0 KB");
    });
});

describe("copyTextWithFallback", () => {
    afterEach(() => {
        vi.restoreAllMocks();
        vi.unstubAllGlobals();
    });

    /** 控制两条写入路径的可用性：安全上下文开关 + Clipboard API 存否 */
    function setClipboardEnv({ secure, clipboardWriteText }) {
        Object.defineProperty(window, "isSecureContext", { configurable: true, value: secure });
        const writeText = clipboardWriteText === null ? undefined : vi.fn(clipboardWriteText);
        Object.defineProperty(navigator, "clipboard", {
            configurable: true,
            value: writeText ? { writeText } : undefined,
        });
        const execCommand = vi.fn(() => true);
        document.execCommand = execCommand;
        return { writeText, execCommand };
    }

    it("安全上下文优先走异步 Clipboard API", async () => {
        const { writeText, execCommand } = setClipboardEnv({ secure: true, clipboardWriteText: async () => {} });
        const onOk = vi.fn();
        copyTextWithFallback("内容", onOk);
        await Promise.resolve();
        expect(writeText).toHaveBeenCalledWith("内容");
        expect(onOk).toHaveBeenCalledTimes(1);
        expect(execCommand).not.toHaveBeenCalled();
    });

    it("Clipboard API 被拒时回退 execCommand", async () => {
        const { writeText, execCommand } = setClipboardEnv({
            secure: true,
            clipboardWriteText: () => Promise.reject(new Error("denied")),
        });
        const onOk = vi.fn();
        copyTextWithFallback("内容", onOk);
        await Promise.resolve();
        await Promise.resolve();
        expect(writeText).toHaveBeenCalled();
        expect(execCommand).toHaveBeenCalledTimes(1);
        expect(onOk).toHaveBeenCalledTimes(1);
    });

    it("非安全上下文直接走离屏 textarea 兜底", () => {
        setClipboardEnv({ secure: false, clipboardWriteText: null });
        const onOk = vi.fn();
        copyTextWithFallback("内容", onOk);
        expect(document.querySelector(".clipboard-scratch")).toBeNull();
        expect(onOk).toHaveBeenCalledTimes(1);
    });

    it("两条路径都物理不可行时回调 onFailure", () => {
        setClipboardEnv({ secure: false, clipboardWriteText: null });
        document.execCommand = vi.fn(() => false);
        const onOk = vi.fn();
        const onFail = vi.fn();
        copyTextWithFallback("内容", onOk, onFail);
        expect(onOk).not.toHaveBeenCalled();
        expect(onFail).toHaveBeenCalledTimes(1);
    });

    it("execCommand 抛错按失败处置", () => {
        setClipboardEnv({ secure: false, clipboardWriteText: null });
        document.execCommand = vi.fn(() => {
            throw new Error("blocked");
        });
        const onFail = vi.fn();
        copyTextWithFallback("内容", vi.fn(), onFail);
        expect(onFail).toHaveBeenCalledTimes(1);
    });
});
