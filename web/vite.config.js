import { defineConfig } from "vite";

export default defineConfig({
    server: {
        // dev 页面与生产同源行为对齐：信令与口令接口转发到本机游戏端信令服务器
        proxy: {
            "/ws": { target: "http://localhost:30808", ws: true },
            "/api": { target: "http://localhost:30808" },
        },
    },
});
