package com.kairo.vistrel.streaming;

import com.google.gson.Gson;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * 下行控制事件线格式锁：record 组件名与顺序变更会破坏网页端手工对齐的协议，必须显式失败
 */
public class ClientEventTest {
    private final Gson gson = new Gson();

    @Test
    void testCursorStateWireShape() {
        Assertions.assertEquals(
                "{\"type\":\"cursor_state\",\"visible\":true,\"x\":128,\"y\":64}",
                gson.toJson(ClientEvent.CursorState.of(true, 128, 64))
        );
    }

    @Test
    void testOpenUrlWireShape() {
        Assertions.assertEquals(
                "{\"type\":\"open_url\",\"value\":\"https://example.com\"}",
                gson.toJson(ClientEvent.OpenUrl.of("https://example.com"))
        );
    }

    @Test
    void testClipboardWriteWireShape() {
        Assertions.assertEquals(
                "{\"type\":\"clipboard_write\",\"value\":\"hello\"}",
                gson.toJson(ClientEvent.ClipboardWrite.of("hello"))
        );
    }

    @Test
    void testDisplayGeometryWireShape() {
        Assertions.assertEquals(
                "{\"type\":\"display_geometry\",\"width\":1920,\"height\":1080}",
                gson.toJson(ClientEvent.DisplayGeometry.of(1920, 1080))
        );
    }
}
