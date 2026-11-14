import { Button } from "./components.js";

// 通知浮层渐隐类与本组件换条时的淡出节奏（与 .toast 的 transition 时长对齐）
const FADE_HIDDEN_CLASS = "fade-hidden";
const SWAP_FADE_MS = 160;

/** 让多行提示只占内容高度 */
function fitTextarea(el) {
    el.rows = 1;
    el.style.height = "auto";
    el.style.height = `${el.scrollHeight + el.offsetHeight - el.clientHeight}px`;
}

/**
 * 全局通知单例：任意模块可直接 import 调用，不需要经 UIManager 转发。
 * DOM 绑定推迟到首次使用，导入顺序与模块加载时机都无关；实例唯一，
 * 因此不存在第二处持有浮层可见性/定时器的权威方。
 */
let instance = null;

/** 懒构建：首次调用时才取 DOM 并绑定关闭按钮 */
function shared() {
    if (!instance) {
        instance = new Notice();
        instance.hide();
    }
    return instance;
}

/**
 * 通用通知卡片：标题/内容/动作在 show 时按需构建，新通知替换旧通知
 *
 * @param {object} options 见 Notice.show
 */
export function notice(options) {
    shared().show(options);
}

/**
 * 短提示：默认无标题无动作、3 秒自动收起；给 title 即升级为带头部的卡片
 *
 * @param {string} text 提示文本
 * @param {{title?: string, timeoutMs?: number}} [options]
 */
export function toast(text, options) {
    const { title = "", timeoutMs } = options || {};
    shared().show({ text, title, timeoutMs: timeoutMs ?? 3000 });
}

/**
 * 常驻内容入槽（如解锁提示）：浮层被一次性内容遮蔽时，其结束或被替换后自动放回
 *
 * @param {string} text 提示文本
 */
export function pin(text) {
    shared().pin(text);
}

/** 常驻内容出槽：正占着浮层才收起；一次性内容正在展示时让它自然播完，不再放回 */
export function unpin() {
    shared().unpin();
}

/** 系统要求减少动效时不做过渡等待 */
function prefersReducedMotion() {
    return typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;
}

/**
 * 通知浮层：单槽位通用卡片，标题/内容/动作全部在 show 时按需构建，新通知替换旧通知，
 * 所有文本一律经 textContent/value 赋值，不做 HTML 注入
 */
export class Notice {
    constructor() {
        this.element = document.createElement("div");
        this.element.className = "toast";
        const header = document.createElement("div");
        header.className = "toast-header";
        this.titleEl = document.createElement("span");
        this.titleEl.className = "toast-title";
        this.titleEl.textContent = "提示";
        header.append(
            this.titleEl,
            new Button({
                id: "closeBtn",
                icon: "/icons/close.svg",
                variant: "bare",
                size: "xs",
                shape: "circle",
                tooltip: "关闭",
                onClick: () => this.close(),
            }).getElement(),
        );
        this.bodyEl = document.createElement("div");
        this.bodyEl.className = "toast-body";
        this.actionsEl = document.createElement("div");
        this.actionsEl.className = "toast-actions";
        this.element.append(header, this.bodyEl, this.actionsEl);
        document.body.appendChild(this.element);
        this.timer = null;
        this.swapTimer = null;
        this.pending = null;
        // 双槽：pinned 是常驻内容（槽内载荷对象恒定复用），rendered 是浮层当前条目，
        // 两者同引用即「正显示常驻内容」的判据
        this.pinned = null;
        this.rendered = null;
        // 自持事件边界：卡片内（按钮/文本域）的指针/按键/粘贴不冒泡到 window，
        // 输入层无需知道本组件挂在哪；纯提示形态 pointer-events:none，事件本就穿透到下层
        if (this.element) {
            for (const type of [
                "pointerdown",
                "mousedown",
                "mouseup",
                "mousemove",
                "wheel",
                "click",
                "keydown",
                "keyup",
                "paste",
            ]) {
                this.element.addEventListener(type, (e) => e.stopPropagation());
            }
        }
    }

    /** 浮层当前是否可见：以渐隐类为唯一依据，不外挂状态 */
    get visible() {
        return Boolean(this.element) && !this.element.classList.contains(FADE_HIDDEN_CLASS);
    }

    /** 常驻内容入槽：换条节奏遮蔽当前内容，其结束或被替换后放回 */
    pin(text) {
        if (this.pinned === null) this.pinned = { text, timeoutMs: 0 };
        else this.pinned.text = text;
        this.show(this.pinned);
    }

    /** 常驻内容出槽：正占着浮层才收起；一次性内容正在展示时让它自然播完 */
    unpin() {
        if (this.rendered !== this.pinned) {
            this.pinned = null;
            return;
        }
        this.pinned = null;
        this.hide();
    }

    /** 关闭按钮：正在显示的是常驻内容即视为用户放弃它，不再放回 */
    close() {
        if (this.rendered === this.pinned) this.pinned = null;
        this.hide();
    }

    /**
     * 展示一条内容并重新计时；可见时走换条，被替换条目不产生收起回调
     *
     * @param {object} notice
     * @param {string} notice.text 主文本
     * @param {string} [notice.title] 标题；与无动作时整行收起
     * @param {boolean} [notice.multiline] 以只读文本域展示（长文本便于整段选中）
     * @param {{label: string, onClick: (title: string, text: string) => void}[]} [notice.actions] 追加动作按钮，回调收到本条通知的标题与内容，具体动作由调用方处理
     * @param {number} [notice.timeoutMs] 自动收起延时；<=0 常驻直到下次通知或显式收起
     */
    show(notice) {
        if (!this.element) return;
        this.clearTimer();
        // 可见时换条先淡出再淡入：每条通知都是独立事件，内容相同也要给出“来了新的一条”
        if (this.visible) {
            this.scheduleSwap(notice);
            return;
        }
        // 不可见时直接渲染：丢掉可能还挂着的待提交内容，免得它稍后回来覆盖本条
        this.clearSwap();
        this.pending = null;
        this.render(notice);
    }

    /** 淡出旧内容，停顿后提交新内容；窗口内连发只保留最新一条 */
    scheduleSwap(notice) {
        this.pending = notice;
        this.clearSwap();
        if (prefersReducedMotion()) {
            this.commitSwap();
            return;
        }
        this.element.classList.add(FADE_HIDDEN_CLASS);
        this.swapTimer = setTimeout(() => this.commitSwap(), SWAP_FADE_MS);
    }

    commitSwap() {
        this.clearSwap();
        const notice = this.pending;
        this.pending = null;
        if (notice) this.render(notice);
    }

    clearSwap() {
        if (this.swapTimer) {
            clearTimeout(this.swapTimer);
            this.swapTimer = null;
        }
    }

    /** 就地渲染内容并重新开始自动收起计时 */
    render(notice) {
        const { text, title = "", multiline = false, actions = [], timeoutMs = 4000 } = notice;
        this.rendered = notice;
        // 纯提示形态：无标题无动作，只留一条消息
        this.element.classList.toggle("is-plain", !title && actions.length === 0);
        this.titleEl.textContent = title;

        this.bodyEl.replaceChildren();
        let content;
        if (multiline) {
            content = document.createElement("textarea");
            content.className = "toast-textarea";
            content.readOnly = true;
            content.value = text;
        } else {
            content = document.createElement("div");
            content.className = "toast-text";
            content.textContent = text;
        }
        this.bodyEl.appendChild(content);
        if (multiline) fitTextarea(content);

        this.actionsEl.replaceChildren();
        for (const action of actions) {
            const button = new Button({ text: action.label, size: "sm", onClick: () => action.onClick(title, text) });
            this.actionsEl.append(button.element);
        }
        this.actionsEl.hidden = actions.length === 0;

        this.element.classList.remove(FADE_HIDDEN_CLASS);
        this.timer = timeoutMs > 0 ? setTimeout(() => this.hide(), timeoutMs) : null;
    }

    hide() {
        this.clearTimer();
        this.clearSwap();
        // 淡出途中被显式收起：待渲染的新内容作废，避免收起后自己又弹出来
        this.pending = null;
        if (this.element) this.element.classList.add(FADE_HIDDEN_CLASS);
        // 常驻内容仍在槽里：此刻浮层不可见，直接放回不产生换条动画
        if (this.pinned) this.render(this.pinned);
    }

    clearTimer() {
        if (this.timer) {
            clearTimeout(this.timer);
            this.timer = null;
        }
    }
}
