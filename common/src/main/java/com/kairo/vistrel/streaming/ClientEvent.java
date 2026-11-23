package com.kairo.vistrel.streaming;

/**
 * 下行控制事件：游戏实例 → 远程浏览器，经信令 WS 文本帧承载
 *
 * <p>type 组件即线格式类型常量，与网页端 protocol.js 手工对齐；
 * 只收小型可 JSON 化控制事件，二进制载荷（如截图）不在此通道。
 */
public sealed interface ClientEvent {
    /**
     * 线格式消息类型，Gson 按组件名原样序列化
     */
    String type();

    /**
     * 光标状态：GUI 态客户端 overlay 光标的可见性与归一化坐标
     */
    record CursorState(String type, boolean visible, int x, int y) implements ClientEvent {
        public static CursorState of(boolean visible, int x, int y) {
            return new CursorState("cursor_state", visible, x, y);
        }
    }

    /**
     * 浏览器动作透传：请求远程浏览器打开 http/https 网址
     */
    record OpenUrl(String type, String value) implements ClientEvent {
        public static OpenUrl of(String url) {
            return new OpenUrl("open_url", url);
        }
    }

    /**
     * 浏览器动作透传：请求写入远程浏览器剪贴板
     */
    record ClipboardWrite(String type, String value) implements ClientEvent {
        public static ClipboardWrite of(String text) {
            return new ClipboardWrite("clipboard_write", text);
        }
    }

    /**
     * 当前生效的显示几何：远程画面可见区域的宽高（不含编码对齐补齐边）
     *
     * <p>
     * 解码帧可能因编码器输入对齐更大（如 AV1 的 64×64），网页端据此裁掉右/下补齐边并换算
     * 输入坐标。初值随 Answer 下发，换档落账后由信令层推送变化值（等值不重发）。
     */
    record DisplayGeometry(String type, int width, int height) implements ClientEvent {
        public static DisplayGeometry of(int width, int height) {
            return new DisplayGeometry("display_geometry", width, height);
        }
    }
}
