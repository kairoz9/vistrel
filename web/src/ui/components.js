/**
 * Vistrel Web Client 通用 UI 组件
 * 收纳从 UIManager 拆出的同构行为：呼吸色点、指标行、模态框、离散档位滑条
 */

/**
 * HUD 指标格：一个标签加一个值，值由调用方算好传入
 */
export class Metric {
    /**
     * @param {HTMLElement|null} valueEl 值节点
     * @param {number} [minCh] 值的最小宽度（字符数）；位数变化时不推移同排其他列
     */
    constructor(valueEl, minCh = 0) {
        this.valueEl = valueEl;
        if (valueEl && minCh > 0) valueEl.style.minWidth = `${minCh}ch`;
    }

    /** @param {string} text 已格式化好的值文案 */
    set(text) {
        if (this.valueEl) this.valueEl.textContent = text;
    }
}

/**
 * HUD 指标行：按配置建出各格并具名持有，加一个指标只需配置里加一行
 */
export class MetricRow {
    /**
     * @param {HTMLElement|null} rowEl 整行容器
     * @param {Object<string, {label: string, value?: string, minCh?: number}>} configs 各格配置，键名即行上可访问的属性名
     */
    constructor(rowEl, configs) {
        this.rowEl = rowEl;
        this.slots = [];
        if (rowEl) rowEl.replaceChildren();
        for (const [key, config] of Object.entries(configs)) {
            const block = document.createElement("div");
            block.className = "metric";
            const labelEl = document.createElement("span");
            labelEl.className = "metric-label";
            labelEl.textContent = config.label;
            const valueEl = document.createElement("span");
            valueEl.className = "metric-value";
            valueEl.textContent = config.value ?? "--";
            block.append(labelEl, valueEl);
            if (rowEl) rowEl.append(block);
            this[key] = new Metric(valueEl, config.minCh);
            this.slots.push(this[key]);
        }
    }
}

/**
 * 状态指示：一枚呼吸色点加一段短文案，色点色相与呼吸节奏由状态名对应的 CSS 类给出
 */
export class StatusDot {
    /**
     * @param {HTMLElement|null} element 容器；点与文案由本组件生成
     */
    constructor(element) {
        this.dotEl = null;
        this.textEl = null;
        if (!element) return;
        element.replaceChildren();
        this.dotEl = document.createElement("span");
        this.dotEl.className = "status-dot";
        this.textEl = document.createElement("span");
        element.append(this.dotEl, this.textEl);
    }

    /**
     * @param {string} tone 状态名，对应 `.status-dot.<tone>` 规则
     * @param {string} text 短文案
     */
    set(tone, text) {
        if (this.dotEl) this.dotEl.className = ["status-dot", tone].filter(Boolean).join(" ");
        if (this.textEl) this.textEl.textContent = text;
    }
}

/**
 * 模态框：单个遮罩的显隐、幂等开关、自身翻转通知，以及可选的遮罩空白处点击关闭
 */
/**
 * 带类名的文本节点：所有文案经 textContent 赋值，不做 HTML 注入
 * @param {string} tag 标签名
 * @param {string} className 类名
 * @param {string} text 文本
 */
function textEl(tag, className, text) {
    const el = document.createElement(tag);
    el.className = className;
    el.textContent = text;
    return el;
}

/**
 * 模态框：按配置建出卡片壳层（眉标/标题/描述/自定义内容/动作行），
 * 可变内容以节点形式传入
 */
export class Modal {
    /**
     * @param {object} options
     * @param {string} options.id 遮罩元素 id（组件新建并挂到 host 下）
     * @param {HTMLElement} [options.host] 遮罩挂载点，默认 document.body
     * @param {string} [options.eyebrow] 标题上方的小标签
     * @param {string} [options.title] 标题
     * @param {string[]} [options.desc] 描述段落
     * @param {Node|null} [options.content] 插在描述与动作之间的自定义内容（表单、富文本）
     * @param {Node|null} [options.footer] 插在动作行之后的次要内容（如辅助链接）
     * @param {{label: string, primary?: boolean, onClick: (button: Button) => void}[]} [options.actions] 动作行按钮
     * @param {boolean} [options.actionsAlignCenter] 动作行居中
     * @param {boolean} [options.closeOnBackdrop] 点击遮罩空白处关闭；卡片内部点击不触发
     * @param {(visible: boolean) => void} [options.onChange] 自身可见性回调
     */
    constructor({
        id,
        host = document.body,
        eyebrow = "",
        title = "",
        desc = [],
        content = null,
        footer = null,
        actions = [],
        actionsAlignCenter = false,
        closeOnBackdrop = false,
        onChange = null,
    }) {
        this.element = this.createOverlay(id, host);
        this.onChange = onChange;
        this.buttons = [];
        this.buildCard({ eyebrow, title, desc, content, footer, actions, actionsAlignCenter });
        if (closeOnBackdrop) {
            // 关闭动作先于冒泡执行并阻断冒泡：外层点击处理器不会在「已无弹窗」后把鼠标锁回画面
            this.element.addEventListener("click", (e) => {
                if (e.target !== this.element) return;
                e.stopPropagation();
                this.hide();
            });
        }
    }

    /**
     * 新建遮罩外壳：容器样式与初始隐藏态由组件自己持有，页面标记不必手写
     * 同 id 的旧遮罩先移除，重复构造不会留下两个同 id 元素
     * @param {string} id 遮罩 id
     * @param {HTMLElement} host 挂载点
     */
    createOverlay(id, host) {
        document.getElementById(id)?.remove();
        const overlay = document.createElement("div");
        overlay.id = id;
        overlay.className = "modal-overlay hidden";
        host.append(overlay);
        return overlay;
    }

    buildCard({ eyebrow, title, desc, content, footer, actions, actionsAlignCenter }) {
        const card = document.createElement("div");
        card.className = "modal-card";
        card.setAttribute("role", "dialog");
        card.setAttribute("aria-modal", "true");
        if (title) card.setAttribute("aria-label", title);
        if (eyebrow) card.append(textEl("span", "modal-eyebrow", eyebrow));
        if (title) card.append(textEl("div", "modal-title", title));
        for (const line of desc) card.append(textEl("p", "modal-desc", line));
        if (content) card.append(content);
        if (actions.length > 0) {
            const row = document.createElement("div");
            row.className = actionsAlignCenter ? "modal-actions modal-actions--center" : "modal-actions";
            for (const action of actions) {
                const button = new Button({
                    variant: action.primary === false ? "ghost" : "primary",
                    text: action.label,
                    onClick: action.onClick,
                });
                this.buttons.push(button);
                row.append(button.element);
            }
            card.append(row);
        }
        if (footer) card.append(footer);
        this.element.replaceChildren(card);
    }

    /** 是否可见：以遮罩自身的 .hidden 为唯一依据 */
    get visible() {
        return !!this.element && !this.element.classList.contains("hidden");
    }

    /** 显示；已可见时幂等，不产生通知 */
    show() {
        if (!this.element || this.visible) return;
        this.element.classList.remove("hidden");
        if (this.onChange) this.onChange(true);
    }

    /** 隐藏；已隐藏时幂等，不产生通知 */
    hide() {
        if (!this.element || !this.visible) return;
        this.element.classList.add("hidden");
        if (this.onChange) this.onChange(false);
    }
}

/** 离散档位滑条：拖动即时刷新标签与选中回调，松手（change）触发提交回调 */
/**
 * 离散档位滑条
 *
 * 档位表是唯一事实源：刻度数量、刻度文案、滑条 max 与当前值标签都由它派生。
 * 对外只认档位对象本身（引用即身份），不回传序号。
 */
export class DiscreteSlider {
    /**
     * @param {object} options
     * @param {HTMLElement} options.host 挂载点（在上追加一组 .slider-group）
     * @param {string} options.label 行首标签
     * @param {Array<*>} options.options 档位表，顺序即滑条顺序
     * @param {*} [options.value] 初值（按引用在 options 中定位，缺席则取首档）
     * @param {(option: *) => string} [options.format] 当前值文案
     * @param {(option: *) => string} [options.tickLabel] 刻度文案
     * @param {(option: *) => void} [options.onCommit] 松手换档回调（拖动途中不触发）
     */
    constructor({
        host,
        label,
        options,
        value = options[0],
        format = String,
        tickLabel = (option) => option?.label ?? String(option),
        onCommit = null,
    }) {
        this.options = options;
        this.format = format;
        this.onCommit = onCommit;

        const group = document.createElement("div");
        group.className = "slider-group";

        const labelRow = document.createElement("div");
        labelRow.className = "slider-label-row";
        this.valueLabel = textEl("span", "slider-cur-val", "");
        labelRow.append(textEl("span", "slider-label", label), this.valueLabel);

        this.input = document.createElement("input");
        this.input.type = "range";
        this.input.className = "discrete-slider";
        this.input.min = "0";
        this.input.max = String(options.length - 1);
        this.input.step = "1";
        this.input.setAttribute("aria-label", label);

        const ticks = document.createElement("div");
        ticks.className = "slider-ticks";
        ticks.setAttribute("aria-hidden", "true");
        ticks.style.setProperty("--last", options.length - 1);
        options.forEach((option, index) => {
            const tick = document.createElement("span");
            tick.dataset.label = tickLabel(option);
            tick.style.setProperty("--i", index);
            ticks.append(tick);
        });

        const body = document.createElement("div");
        body.className = "slider-body";
        body.append(this.input, ticks);
        group.append(labelRow, body);
        host.append(group);

        this.input.addEventListener("input", () => this.render());
        this.input.addEventListener("change", () => {
            if (this.onCommit) this.onCommit(this.value);
        });

        this.select(value);
    }

    get index() {
        return parseInt(this.input.value, 10) || 0;
    }

    get value() {
        return this.options[this.index];
    }

    /** 按引用选中档位并刷新 DOM；不在档位表里的值静默忽略（协商回包可能给出未知档位） */
    select(option) {
        const idx = this.options.indexOf(option);
        if (idx === -1) return;
        this.input.value = String(idx);
        this.render();
    }

    render() {
        const text = this.format(this.value);
        this.valueLabel.textContent = text;
        this.input.setAttribute("aria-valuetext", text);
    }
}

// 生效外观类名：录制中、开关打开等"按下/生效"状态统一走它
const ACTIVE_CLASS = "is-active";

// 外观三档：调用方只说语义（主操作/次要操作、大小、方形），具体类名归本组件
const BASE_CLASS = "btn";
const VARIANT_CLASSES = { primary: "btn--primary", ghost: "btn--ghost", bare: "btn--bare" };
const SIZE_CLASSES = { md: "btn--md", sm: "btn--sm", xs: "btn--xs" };
// 形状三选：矩形（默认，宽度随文本）、正方形、圆形（图标钮各 30×30）
const SHAPE_CLASSES = { rect: null, square: "btn--icon", circle: "btn--circle" };
const DEFAULT_VARIANT = "primary";
const DEFAULT_SIZE = "md";
const DEFAULT_SHAPE = "rect";

/** 把 .svg 源文本解析成节点，图标文件因此可以只写标记 */
function parseSvg(raw) {
    const template = document.createElement("template");
    template.innerHTML = raw.trim();
    const svg = template.content.firstElementChild;
    if (!svg || svg.tagName.toLowerCase() !== "svg") {
        throw new Error("Button: 图标源文本必须是 <svg>");
    }
    return svg;
}

/**
 * 通用按钮：包装页面里现成的 <button>，或按类名与文案现建一个。
 * 只管视觉交互：可见文本（外加可选图标）、悬停提示、禁用与生效外观。
 */
export class Button {
    /**
     * @param {object} [options]
     * @param {HTMLButtonElement|null} [options.element] 现成按钮；省略则新建
     * @param {'primary'|'ghost'|'bare'} [options.variant] 外观档；包装现成按钮时须同时给 size 才生效
     * @param {'md'|'sm'|'xs'} [options.size] 尺寸档，省略则新建按钮取 md
     * @param {'rect'|'square'|'circle'} [options.shape] 按钮形状，省略取矩形
     * @param {string} [options.text] 可见文本
     * @param {SVGElement|string|null} [options.icon] 图标：SVG 节点、SVG 源文本，或 public/ 下的静态路径
     * @param {string|null} [options.tooltip] 悬停提示
     * @param {(button: Button) => void} [options.onClick] 点击回调，收到本实例
     */
    constructor({
        id,
        variant = null,
        size = null,
        shape = null,
        text = "",
        icon = null,
        tooltip = null,
        onClick = null,
    } = {}) {
        const element = id && document.getElementById(id);
        this.element = element ? element : document.createElement("button");
        this.textEl = null;
        this.iconUrl = null;
        if (!element) {
            if (id) this.element.id = id;
            this.element.type = "button";
        }
        // 包装纯布局按钮（收起钮、开关、文字链接）时不动标记，只有给了外观参数才落类名
        if (variant || size || shape) {
            this.applyLook({
                variant: variant || DEFAULT_VARIANT,
                size: size || DEFAULT_SIZE,
                shape: shape || DEFAULT_SHAPE,
            });
        }
        if (icon) this.setIcon(icon);
        if (text) this.setText(text);
        if (tooltip) this.setTooltip(tooltip);
        if (onClick) this.bindClick(onClick);
    }

    getElement() {
        return this.element;
    }

    /** 落外观档类名：未知档位当场报错，免得静默生成一个没有样式的按钮 */
    applyLook({ variant = DEFAULT_VARIANT, size = DEFAULT_SIZE, shape = DEFAULT_SHAPE } = {}) {
        const variantClass = VARIANT_CLASSES[variant];
        if (!variantClass) throw new Error(`Button: 未知的 variant "${variant}"`);
        const sizeClass = SIZE_CLASSES[size];
        if (!sizeClass) throw new Error(`Button: 未知的 size "${size}"`);
        if (!(shape in SHAPE_CLASSES)) throw new Error(`Button: 未知的 shape "${shape}"`);
        this.element.classList.add(BASE_CLASS, variantClass, sizeClass);
        const shapeClass = SHAPE_CLASSES[shape];
        if (shapeClass) this.element.classList.add(shapeClass);
    }

    /** 绑定点击；一个按钮只应有一个主行为 */
    bindClick(handler) {
        this.element.addEventListener("click", () => handler(this));
    }

    /** 改可见文本：按钮里已有图标时文本住进 span，否则 textContent 会把图标一起抹掉 */
    setText(text) {
        if (!this.textEl) {
            if (this.element.querySelector("svg")) {
                // 清掉标记里缩进留下的空白文本节点，只留图标与文本
                for (const node of [...this.element.childNodes]) {
                    if (node.nodeType === Node.TEXT_NODE) node.remove();
                }
                const span = document.createElement("span");
                span.className = "btn-text";
                this.element.appendChild(span);
                this.textEl = span;
            } else {
                this.textEl = this.element;
            }
        }
        this.textEl.textContent = text ?? "";
    }

    /** 设置图标：排在文本之前，重复设置替换旧图标 */
    setIcon(icon) {
        if (typeof icon !== "string") {
            this.replaceIcon(icon);
        } else if (icon.trim().startsWith("<")) {
            this.replaceIcon(parseSvg(icon));
        } else {
            this.loadIcon(icon);
        }
    }

    /** 静态路径的图标按需拉取；图标已被换掉时丢弃过期响应 */
    async loadIcon(url) {
        this.iconUrl = url;
        try {
            const response = await fetch(url);
            if (!response.ok) throw new Error(`HTTP ${response.status}`);
            const svg = parseSvg(await response.text());
            if (this.iconUrl !== url) return;
            this.replaceIcon(svg);
        } catch (error) {
            console.error(`Button: 图标加载失败 "${url}"`, error);
        }
    }

    replaceIcon(node) {
        this.iconUrl = null;
        const old = this.element.querySelector(":scope > svg");
        if (old) old.remove();
        this.element.insertBefore(node, this.element.firstChild);
    }

    setTooltip(text) {
        if (text) this.element.title = text;
        else this.element.removeAttribute("title");
    }

    get disabled() {
        return this.element.disabled;
    }

    setDisabled(disabled) {
        this.element.disabled = Boolean(disabled);
    }

    /** 生效外观：录制中、开关打开等 */
    setActive(active) {
        this.element.classList.toggle(ACTIVE_CLASS, Boolean(active));
    }
}
