package kairo.vistrel;

import kairo.vistrel.capture.AudioCaptureScheduler.LoopbackBinding;
import kairo.vistrel.capture.VideoCaptureScheduler;
import kairo.vistrel.config.VistrelConfig;
import kairo.vistrel.streaming.FakeSessionServer;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

/**
 * VistrelService 统一门面与生命周期协调测试 (0 JNI, 0 端口占用)
 */
public class VistrelServiceTest {
    @BeforeEach
    @AfterEach
    void resetServiceState() {
        VistrelService.stop();
        // 测试间按 MC 销毁音频库的语义复位绑定正本
        LoopbackBinding.clear();
    }

    @Test
    void testServiceLifecycleWithFakeServer() {
        Assertions.assertFalse(VistrelService.isRunning(), "初始状态应未运行");
        Assertions.assertFalse(VistrelService.isStreaming(), "初始状态应无活跃推流");

        FakeSessionServer fakeServer = new FakeSessionServer();
        FakeSessionServer.FakeEncoderChannel fakeChannel = new FakeSessionServer.FakeEncoderChannel();
        fakeChannel.vramDirectSupported = false;

        VistrelConfig config = new VistrelConfig(30808, "testPassword123", 128, false, true);
        // 启动服务 (注入冻结配置视图、控制面与数据面)
        VistrelService.start(() -> config, fakeServer, fakeChannel, fakeChannel);

        Assertions.assertTrue(VistrelService.isRunning(), "启动后应处于运行状态");
        Assertions.assertSame(fakeServer, VistrelService.getServer());
        Assertions.assertSame(fakeChannel, VistrelService.getChannel());
        Assertions.assertEquals("testPassword123", fakeServer.getPassword());
        Assertions.assertEquals(128, fakeServer.getAudioBitrate());
        // 模拟远程客户端连入
        fakeServer.simulateClientConnected(1280, 720, 30);
        Assertions.assertTrue(VistrelService.isStreaming(), "客户端接入后应当处于推流活跃状态");
        // 模拟客户端断开
        fakeServer.simulateClientDisconnected();
        Assertions.assertFalse(VistrelService.isStreaming(), "客户端断开后应退出活跃推流状态");
        // 平稳停机
        VistrelService.stop();
        Assertions.assertFalse(VistrelService.isRunning(), "停机后应恢复未运行状态");
        Assertions.assertFalse(VistrelService.isStreaming(), "停机后推流状态应为 false");
        Assertions.assertNull(VistrelService.getServer(), "停机后 server 引用应清空");
        Assertions.assertNull(VistrelService.getChannel(), "停机后 channel 引用应清空");
        // 幂等停机测试
        Assertions.assertDoesNotThrow(VistrelService::stop, "重复调用 stop() 必须幂等安全");
    }

    @Test
    void testStopClearsTextInputSink() {
        FakeSessionServer fakeServer = new FakeSessionServer();
        FakeSessionServer.FakeEncoderChannel fakeChannel = new FakeSessionServer.FakeEncoderChannel();
        fakeChannel.vramDirectSupported = false;

        VistrelConfig config = new VistrelConfig(30808, "testPassword123", 128, false, true);
        VistrelService.start(() -> config, fakeServer, fakeChannel, fakeChannel);
        Assertions.assertTrue(fakeServer.emitTextInput("停机前"), "运行中 sink 已接线，文本照常送达");

        VistrelService.stop();
        Assertions.assertFalse(fakeServer.emitTextInput("停机后"), "停服后 sink 已摘除，僵尸连接的文本必须死在信令层");
    }

    @Test
    void testDuplicateStartIgnored() {
        FakeSessionServer server1 = new FakeSessionServer();
        FakeSessionServer server2 = new FakeSessionServer();
        FakeSessionServer.FakeEncoderChannel channel1 = new FakeSessionServer.FakeEncoderChannel();
        FakeSessionServer.FakeEncoderChannel channel2 = new FakeSessionServer.FakeEncoderChannel();

        VistrelConfig config1 = new VistrelConfig(30808, "pass1", 128, false, true);
        VistrelConfig config2 = new VistrelConfig(30808, "pass2", 128, false, true);

        VistrelService.start(() -> config1, server1, channel1, channel1);
        Assertions.assertSame(server1, VistrelService.getServer());
        Assertions.assertSame(channel1, VistrelService.getChannel());
        // 重复启动应被忽略
        VistrelService.start(() -> config2, server2, channel2, channel2);
        Assertions.assertSame(server1, VistrelService.getServer(), "重复启动不应替换现有活跃 server");
        Assertions.assertSame(channel1, VistrelService.getChannel());

        VistrelService.stop();
    }

    @Test
    void testStallAutoRestartRebuildsService() {
        FakeSessionServer server1 = new FakeSessionServer();
        FakeSessionServer.FakeEncoderChannel channel1 = new FakeSessionServer.FakeEncoderChannel();
        channel1.vramDirectSupported = false;
        VistrelConfig config = new VistrelConfig(30808, "restartPass123", 128, false, true);
        VistrelService.start(() -> config, server1, channel1, channel1);
        Assertions.assertTrue(VistrelService.isRunning(), "自愈前置：服务应处于运行状态");
        // 编码停滞自愈：拆除落地后按同一套注入实例重启（origin 取当前活跃调度器）
        VistrelService.restartEngine(VistrelService.getVideoCaptureScheduler());
        Assertions.assertTrue(VistrelService.isRunning(), "自愈重启后服务必须重新可用");
        Assertions.assertSame(server1, VistrelService.getServer(), "自愈按注入约定复用同一套控制面实例");
        Assertions.assertSame(channel1, VistrelService.getChannel(), "数据面通道同样复用");
        // 冷却期内的第二次触发直接跳过，不重复拆装
        VistrelService.restartEngine(VistrelService.getVideoCaptureScheduler());
        Assertions.assertTrue(VistrelService.isRunning(), "冷却期跳过不得影响运行状态");
        Assertions.assertSame(server1, VistrelService.getServer(), "冷却期跳过不得更换实例");
        // 旧实例的停滞信号在停机换代后已过期：不得拉起服务
        VideoCaptureScheduler stalled = VistrelService.getVideoCaptureScheduler();
        VistrelService.stop();
        VistrelService.restartEngine(stalled);
        Assertions.assertFalse(VistrelService.isRunning(), "未运行时触发心跳超时不得拉起服务");
    }

    @Test
    void testRestartEngineKeepsRuntimePasswordChange() {
        FakeSessionServer server1 = new FakeSessionServer();
        FakeSessionServer.FakeEncoderChannel channel1 = new FakeSessionServer.FakeEncoderChannel();
        channel1.vramDirectSupported = false;
        // 生产同构的可变视图（生产即 VistrelConfig::current）：改密 = 视图更新 + 推活实例
        AtomicReference<VistrelConfig> view =
                new AtomicReference<>(new VistrelConfig(30808, "initialPass123", 128, false, true));
        VistrelService.start(view::get, server1, channel1, channel1);
        view.set(view.get().withPassword("changedPass456"));
        server1.setPassword("changedPass456");
        // 清冷却账目保证自愈真实拆装（不受其他用例的冷却残留影响）
        VistrelService.resetRestartCooldownForTest();
        VistrelService.restartEngine(VistrelService.getVideoCaptureScheduler());
        Assertions.assertEquals(2, server1.startCount.get(), "自愈必须真实重启一次");
        Assertions.assertTrue(VistrelService.isRunning(), "自愈重启后服务必须重新可用");
        Assertions.assertSame(server1, VistrelService.getServer(), "自愈复用同一套注入实例");
        Assertions.assertEquals("changedPass456", server1.getPassword(), "重启重读视图，运行期改密不得被写回");
    }

    @Test
    void testLoopbackBindingIndependentOfServiceLifecycle() {
        LoopbackBinding.open(0x1234L);
        LoopbackBinding.markContextReady();
        FakeSessionServer server1 = new FakeSessionServer();
        FakeSessionServer.FakeEncoderChannel channel1 = new FakeSessionServer.FakeEncoderChannel();
        channel1.vramDirectSupported = false;
        VistrelConfig config = new VistrelConfig(30808, "audioPass123", 128, false, true);
        VistrelService.start(() -> config, server1, channel1, channel1);
        VistrelService.stop();
        VistrelService.start(
                () -> config,
                new FakeSessionServer(),
                new FakeSessionServer.FakeEncoderChannel(),
                new FakeSessionServer.FakeEncoderChannel()
        );
        Assertions.assertTrue(VistrelService.isRunning());
        // 绑定正本随 MC 音频库存亡：服务启停既不清空也无需回填，重启窗口不再丢句柄与就绪位
        Assertions.assertEquals(0x1234L, LoopbackBinding.current().device(), "服务重启不得丢失环回设备句柄");
        Assertions.assertTrue(LoopbackBinding.current().contextReady(), "服务重启不得丢失 context 就绪位");
        // MC 销毁音频库是唯一的作废路径
        LoopbackBinding.clear();
        Assertions.assertFalse(LoopbackBinding.current().isBound());
    }
}
