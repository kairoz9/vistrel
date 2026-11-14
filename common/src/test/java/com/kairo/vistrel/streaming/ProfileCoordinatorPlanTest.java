package com.kairo.vistrel.streaming;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;
import com.kairo.vistrel.streaming.ProfileCoordinator.ContentSnapshot;
import com.kairo.vistrel.streaming.SessionServer.SessionState;
import org.junit.jupiter.api.Test;

/**
 * 档位规划规则：合并规则（等比视口上限、Unlimited 映射、VSync 夹低、回退档位）逐条钉住。
 * 快照经协调器内部状态喂入（渲染线程同款路径），无独立静态入口
 */
class ProfileCoordinatorPlanTest {
    private static ContentSnapshot snapshot(int w, int h, int fps, boolean vsync, int refresh) {
        return new ContentSnapshot(w, h, fps, vsync, refresh);
    }

    private static SessionState plan(ContentSnapshot snapshot, int vpW, int vpH, double bpp) {
        ProfileCoordinator worker = new ProfileCoordinator(null, null);
        worker.onContentSnapshot(snapshot, 0L);
        return worker.plan(vpW, vpH, bpp);
    }

    @Test
    void testContentFollowsFramebufferWithoutLift() {
        // 800×600 原样串流：服务端绝不放大发假细节
        SessionState t = plan(snapshot(800, 600, 60, false, 0), 0, 0, 0.1);
        assertEquals(new SessionState(SessionState.Phase.STREAMING, 800, 600, 60, 0.1, "None"), t);
    }

    @Test
    void testViewportCapsPhysicalPixelsPreservingAspect() {
        // 4K 内容落在 1920×1080 的物理视口：等比缩到 1920×1080
        SessionState t = plan(snapshot(3840, 2160, 60, false, 0), 1920, 1080, 0.1);
        assertEquals(1920, t.width());
        assertEquals(1080, t.height());
        // HiDPI 视口上限 1280×720（CSS 640×360 × DPR 2 已由网页端算好）
        t = plan(snapshot(2560, 1440, 60, false, 0), 1280, 720, 0.1);
        assertEquals(1280, t.width());
        assertEquals(720, t.height());
        // 更紧的一边决定缩放比：5120×1440 超宽落进 1920 宽视口 → 1920×540
        t = plan(snapshot(5120, 1440, 60, false, 0), 1920, 1080, 0.1);
        assertEquals(1920, t.width());
        assertEquals(540, t.height());
        // 视口大于内容：只缩不放
        t = plan(snapshot(800, 600, 60, false, 0), 3840, 2160, 0.1);
        assertEquals(800, t.width());
        assertEquals(600, t.height());
    }

    @Test
    void testInvalidViewportIsIgnored() {
        // 无视口上报（0）与非法视口在规划器里同为「无上限」
        SessionState t = plan(snapshot(1920, 1080, 60, false, 0), 0, 0, 0.1);
        assertEquals(1920, t.width());
        assertEquals(1080, t.height());
    }

    @Test
    void testUnlimitedMapsToStreamCeilingAndVsyncCapsToRefresh() {
        // Unlimited(260) → 240
        SessionState t = plan(snapshot(1920, 1080, 260, false, 0), 0, 0, 0.1);
        assertEquals(ProfileCoordinator.STREAM_MAX_FPS, t.fps());
        // VSync 开启：再被显示器刷新率夹低
        t = plan(snapshot(1920, 1080, 260, true, 144), 0, 0, 0.1);
        assertEquals(144, t.fps());
        // VSync 关闭：配置 144 就发 144，即使显示器只有 60（高于刷新率能降低输入延迟）
        t = plan(snapshot(1920, 1080, 144, false, 60), 0, 0, 0.1);
        assertEquals(144, t.fps());
        // 任意整数帧率不吸档（离散档位退役）
        t = plan(snapshot(1920, 1080, 73, false, 0), 0, 0, 0.1);
        assertEquals(73, t.fps());
    }

    @Test
    void testEmptySnapshotFallsBackTo1080p60() {
        SessionState t = plan(ContentSnapshot.EMPTY, 0, 0, 0.2);
        assertEquals(new SessionState(SessionState.Phase.STREAMING, 1920, 1080, 60, 0.2, "None"), t);
    }

    @Test
    void testBppClampAndRange() {
        assertEquals(0.1, ProfileCoordinator.clampBpp(Double.NaN), 1e-9);
        assertEquals(0.1, ProfileCoordinator.clampBpp(0.0), 1e-9);
        assertEquals(0.3, ProfileCoordinator.clampBpp(0.5), 1e-9);
        assertEquals(0.13, ProfileCoordinator.clampBpp(0.126), 1e-9);
        assertFalse(ProfileCoordinator.isBppInRange(0.05));
        assertFalse(ProfileCoordinator.isBppInRange(Double.NaN));
        assertTrue(ProfileCoordinator.isBppInRange(0.3));
    }
}
