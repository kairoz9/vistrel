import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/** 动态重导入拿干净单例：notice.js 的实例藏在模块闭包里，无 reset 入口 */
async function freshNotice() {
    vi.resetModules();
    return await import("./notice.js");
}

beforeEach(() => {
    document.body.innerHTML = "";
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    // 关闭按钮的图标按需 fetch，测试环境无静态服务：桩成返回合法 SVG 走成功路径
    vi.stubGlobal(
        "fetch",
        vi.fn(async () => ({ ok: true, text: async () => "<svg xmlns='http://www.w3.org/2000/svg'/>" })),
    );
});

afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
});

describe("通知单例", () => {
    it("toast 挂载卡片，超时后渐隐收起", async () => {
        const { toast } = await freshNotice();
        toast("你好");
        const el = document.querySelector(".toast");
        expect(el).not.toBeNull();
        expect(el.textContent).toContain("你好");
        expect(el.classList.contains("fade-hidden")).toBe(false);
        vi.advanceTimersByTime(3000);
        expect(el.classList.contains("fade-hidden")).toBe(true);
    });

    it("单槽位复用：连续 toast 不叠加卡片，换条窗口后展示新内容", async () => {
        const { toast } = await freshNotice();
        toast("第一条");
        toast("第二条");
        expect(document.querySelectorAll(".toast")).toHaveLength(1);
        vi.advanceTimersByTime(160);
        const el = document.querySelector(".toast");
        expect(el.textContent).toContain("第二条");
        expect(el.classList.contains("fade-hidden")).toBe(false);
    });

    it("pin 常驻内容：被一次性提示遮蔽，其收起后自动放回", async () => {
        const { pin, toast } = await freshNotice();
        pin("点击画面锁定鼠标");
        expect(document.querySelector(".toast").textContent).toContain("点击画面锁定鼠标");
        toast("动作反馈");
        vi.advanceTimersByTime(160);
        expect(document.querySelector(".toast").textContent).toContain("动作反馈");
        vi.advanceTimersByTime(3000);
        expect(document.querySelector(".toast").textContent).toContain("点击画面锁定鼠标");
        expect(document.querySelector(".toast").classList.contains("fade-hidden")).toBe(false);
    });

    it("unpin：一次性内容正在展示时让它自然播完，不立刻杀掉也不放回", async () => {
        const { pin, toast, unpin } = await freshNotice();
        pin("常驻");
        toast("一次性");
        vi.advanceTimersByTime(160);
        unpin();
        expect(document.querySelector(".toast").classList.contains("fade-hidden")).toBe(false);
        vi.advanceTimersByTime(3000);
        expect(document.querySelector(".toast").classList.contains("fade-hidden")).toBe(true);
    });

    it("unpin：常驻内容正占着浮层时立即收起且不放回", async () => {
        const { pin, unpin } = await freshNotice();
        pin("常驻");
        unpin();
        expect(document.querySelector(".toast").classList.contains("fade-hidden")).toBe(true);
        vi.advanceTimersByTime(5000);
        expect(document.querySelector(".toast").classList.contains("fade-hidden")).toBe(true);
    });

    it("关闭按钮：收起一次性内容则放回常驻，收起常驻内容即视为放弃", async () => {
        const { pin, toast } = await freshNotice();
        pin("常驻");
        toast("一次性", { timeoutMs: 0 });
        vi.advanceTimersByTime(160);
        document.querySelector(".toast button").click();
        expect(document.querySelector(".toast").textContent).toContain("常驻");

        document.querySelector(".toast button").click();
        expect(document.querySelector(".toast").classList.contains("fade-hidden")).toBe(true);
        vi.advanceTimersByTime(5000);
        expect(document.querySelector(".toast").classList.contains("fade-hidden")).toBe(true);
    });

    it("multiline 以只读文本域承载长文本", async () => {
        const { notice } = await freshNotice();
        notice({ text: "长文本", multiline: true, timeoutMs: 0 });
        const textarea = document.querySelector(".toast textarea");
        expect(textarea).not.toBeNull();
        expect(textarea.readOnly).toBe(true);
        expect(textarea.value).toBe("长文本");
        // timeoutMs <=0 常驻
        vi.advanceTimersByTime(10_000);
        expect(document.querySelector(".toast").classList.contains("fade-hidden")).toBe(false);
    });

    it("动作按钮点击回调收到本条标题与内容", async () => {
        const { notice } = await freshNotice();
        const action = vi.fn();
        notice({ text: "内容", title: "标题", actions: [{ label: "执行", onClick: action }], timeoutMs: 0 });
        const button = [...document.querySelectorAll(".toast-actions button")].at(-1);
        button.click();
        expect(action).toHaveBeenCalledWith("标题", "内容");
    });
});
