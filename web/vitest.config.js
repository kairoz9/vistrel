import { defineConfig } from "vitest/config";

export default defineConfig({
    test: {
        // 输入控制器构造即挂 DOM 监听器，单元层统一跑 happy-dom；
        // 浏览器专属复合行为（真触摸管线、真布局）不在单元层覆盖范围
        environment: "happy-dom",
        include: ["src/**/*.test.js"],
    },
});
