/**
 * 可拖动面板：指针拖动、视口夹取、停靠象限、位置持久化，以及面板自身的收起/展开。
 * 收起是几何机制（面板变小后按当前停靠侧钉住边缘），因此归这里；收起的业务后果
 * 面板里放什么、收起如何呈现与本组件无关。
 */

// 交互控件不参与拖动：结构性判定，避免在组件里列举带业务语义的类名
const INTERACTIVE_SELECTOR = 'button, a, input, textarea, select, [role="switch"]';
// 位移小于该阈值视为轻点而非拖动
const DRAG_THRESHOLD_PX = 4;
const COLLAPSED_CLASS = "is-collapsed";

export class DraggablePanel {
    /**
     * @param {Object} options
     * @param {HTMLElement|null} options.element 面板根节点
     * @param {string|null} [options.storageKey] 位置持久化键，省略则不持久化
     * @param {string} [options.skipSelector] 不参与拖动的元素选择器
     */
    constructor({ element, storageKey = null, skipSelector = INTERACTIVE_SELECTOR }) {
        this.element = element;
        this.storageKey = storageKey;
        this.skipSelector = skipSelector;
        this.collapsed = false;
        this.position = null;
        this.drag = null;
        if (!this.element) return;

        this.element.addEventListener("pointerdown", (e) => this.beginDrag(e));
        this.element.addEventListener("pointermove", (e) => this.moveDrag(e));
        this.element.addEventListener("pointerup", (e) => this.endDrag(e));
        this.element.addEventListener("pointercancel", (e) => this.endDrag(e));
        window.addEventListener("resize", () => this.clamp());
        this.restore();
    }

    beginDrag(e) {
        if (e.button !== 0) return;
        if (e.target instanceof Element && e.target.closest(this.skipSelector)) return;
        const rect = this.element.getBoundingClientRect();
        this.drag = { id: e.pointerId, x: e.clientX, y: e.clientY, left: rect.left, top: rect.top, moved: false };
        this.element.setPointerCapture(e.pointerId);
    }

    moveDrag(e) {
        const drag = this.drag;
        if (!drag || e.pointerId !== drag.id) return;
        const dx = e.clientX - drag.x;
        const dy = e.clientY - drag.y;
        if (!drag.moved && Math.abs(dx) + Math.abs(dy) < DRAG_THRESHOLD_PX) return;
        drag.moved = true;
        this.element.classList.add("is-dragging");
        this.moveTo(drag.left + dx, drag.top + dy);
    }

    endDrag(e) {
        const drag = this.drag;
        if (!drag || e.pointerId !== drag.id) return;
        this.drag = null;
        this.element.classList.remove("is-dragging");
        if (this.element.hasPointerCapture(e.pointerId)) {
            this.element.releasePointerCapture(e.pointerId);
        }
        if (drag.moved) {
            this.save();
        } else if (this.collapsed) {
            // 收起态下轻点面板等于展开；展开态轻点不做事
            this.setCollapsed(false);
        }
    }

    /** 收起/展开：幂等，尺寸变化后按当前停靠侧重钉边缘 */
    setCollapsed(collapsed) {
        const el = this.element;
        if (!el || this.collapsed === collapsed) return;

        const before = el.getBoundingClientRect();
        this.collapsed = collapsed;
        el.classList.toggle(COLLAPSED_CLASS, collapsed);
        if (this.position) {
            // 贴着哪一侧就钉住哪一侧：靠右则右缘不动（向左展开），靠下则下缘不动
            const after = el.getBoundingClientRect();
            const keepRight = before.left + before.width / 2 > window.innerWidth / 2;
            const keepBottom = before.top + before.height / 2 > window.innerHeight / 2;
            this.moveTo(
                keepRight ? before.right - after.width : before.left,
                keepBottom ? before.bottom - after.height : before.top,
            );
        }
    }

    toggle() {
        this.setCollapsed(!this.collapsed);
    }

    /** 定位到视口内的 (left, top)，并据中心点刷新停靠象限 */
    moveTo(left, top) {
        const el = this.element;
        const maxLeft = Math.max(0, window.innerWidth - el.offsetWidth);
        const maxTop = Math.max(0, window.innerHeight - el.offsetHeight);
        const l = Math.round(Math.min(Math.max(0, left), maxLeft));
        const t = Math.round(Math.min(Math.max(0, top), maxTop));
        this.position = { left: l, top: t };
        // 用 transform 而非 left/top 定位：绝对定位元素的 shrink-to-fit 宽度依赖 left，
        // 写 left 会把盒子压窄而面板仍然溢出，offsetWidth 随之失真导致夹取失效
        el.style.transform = `translate(${l - el.offsetLeft}px, ${t - el.offsetTop}px)`;
        // 停靠侧决定收起按钮的翻边方向等呈现
        el.classList.toggle("is-docked-right", l + el.offsetWidth / 2 > window.innerWidth / 2);
    }

    /** 视口变化后重新夹取（未拖动过的面板保持 CSS 默认锚点） */
    clamp() {
        if (!this.element || !this.position) return;
        this.moveTo(this.position.left, this.position.top);
    }

    save() {
        if (!this.storageKey || !this.position) return;
        try {
            localStorage.setItem(this.storageKey, JSON.stringify(this.position));
        } catch (_) {
            // 隐私模式下写入失败，仅本次会话内生效
        }
    }

    restore() {
        if (!this.storageKey) return;
        try {
            const raw = localStorage.getItem(this.storageKey);
            if (!raw) return;
            const pos = JSON.parse(raw);
            if (typeof pos.left === "number" && typeof pos.top === "number") {
                this.moveTo(pos.left, pos.top);
            }
        } catch (_) {
            // 数据损坏时保持默认位置
        }
    }
}
