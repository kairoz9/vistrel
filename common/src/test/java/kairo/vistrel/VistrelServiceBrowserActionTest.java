package kairo.vistrel;

import kairo.vistrel.config.VistrelConfig;
import kairo.vistrel.streaming.FakeSessionServer;
import kairo.vistrel.streaming.ClientEvent;
import java.net.URI;
import java.util.List;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

/**
 * 浏览器动作透传门面测试：URI 白名单、剪贴板长度过滤与串流状态门控 (0 JNI, 0 端口占用)
 */
public class VistrelServiceBrowserActionTest {
    private FakeSessionServer fakeServer;

    @BeforeEach
    void startService() {
        fakeServer = new FakeSessionServer();
        VistrelService.start(
                () -> new VistrelConfig(30808, "testPassword123", 128, false, true),
                fakeServer,
                new FakeSessionServer.FakeEncoderChannel(),
                new FakeSessionServer.FakeEncoderChannel()
        );
    }

    @AfterEach
    void stopService() {
        VistrelService.stop();
    }

    @Test
    void testNoForwardWhenNotStreaming() {
        Assertions.assertFalse(VistrelService.isStreaming(), "前置条件：无远程会话");
        Assertions.assertFalse(VistrelService.forwardOpenUrl(URI.create("https://example.com")));
        VistrelService.onClipboardWritten("hello");
        Assertions.assertTrue(fakeServer.getEvents().isEmpty(), "未串流时不得透传任何动作");
    }

    @Test
    void testOpenUrlOnlyHttpSchemeForwarded() {
        fakeServer.simulateClientConnected(1280, 720, 60);

        Assertions.assertTrue(VistrelService.forwardOpenUrl(URI.create("https://example.com/mc?a=1")));
        Assertions.assertTrue(VistrelService.forwardOpenUrl(URI.create("HTTP://EXAMPLE.COM")));
        Assertions.assertFalse(VistrelService.forwardOpenUrl(URI.create("file:///tmp/mods")));
        Assertions.assertFalse(VistrelService.forwardOpenUrl(URI.create("javascript:alert(1)")));
        Assertions.assertFalse(VistrelService.forwardOpenUrl(URI.create("data:text/plain,hi")));
        Assertions.assertFalse(VistrelService.forwardOpenUrl(URI.create("mailto:a@example.com")));
        Assertions.assertFalse(VistrelService.forwardOpenUrl(null));

        Assertions.assertEquals(
                List.of(
                        ClientEvent.OpenUrl.of("https://example.com/mc?a=1"),
                        ClientEvent.OpenUrl.of("HTTP://EXAMPLE.COM")
                ),
                fakeServer.getEvents()
        );
    }

    @Test
    void testClipboardForwardFilters() {
        fakeServer.simulateClientConnected(1280, 720, 60);

        VistrelService.onClipboardWritten(null);
        VistrelService.onClipboardWritten("");
        VistrelService.onClipboardWritten("长".repeat(8193));
        Assertions.assertTrue(fakeServer.getEvents().isEmpty(), "空串与超限文本必须静默丢弃");

        VistrelService.onClipboardWritten("hello vistrel");
        Assertions.assertEquals(List.of(ClientEvent.ClipboardWrite.of("hello vistrel")), fakeServer.getEvents());
    }

    @Test
    void testSilentAfterClientDisconnected() {
        fakeServer.simulateClientConnected(1280, 720, 60);
        fakeServer.simulateClientDisconnected();

        Assertions.assertFalse(VistrelService.forwardOpenUrl(URI.create("https://example.com")));
        VistrelService.onClipboardWritten("hello");
        Assertions.assertTrue(fakeServer.getEvents().isEmpty(), "会话断开后应恢复空闲静默");
    }
}
