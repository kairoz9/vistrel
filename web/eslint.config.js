import js from "@eslint/js";
import globals from "globals";
import eslintConfigPrettier from "eslint-config-prettier/flat";

export default [
    // 全局排除：构建产物与构建配置（vitest/playwright 配置同样排除在 lint 外）
    { ignores: ["dist/**", "vite.config.js", "vitest.config.js"] },
    js.configs.recommended,
    {
        languageOptions: {
            ecmaVersion: "latest",
            sourceType: "module",
            globals: globals.browser,
        },
        rules: {
            // 下划线前缀与空 catch 块是“刻意忽略”的习惯写法
            "no-unused-vars": [
                "error",
                { argsIgnorePattern: "^_", varsIgnorePattern: "^_", caughtErrorsIgnorePattern: "^_" },
            ],
            "no-empty": ["error", { allowEmptyCatch: true }],
        },
    },
    eslintConfigPrettier,
];
