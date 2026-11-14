package com.kairo.vistrel.command;

import com.kairo.vistrel.config.VistrelConfig;
import com.kairo.vistrel.VistrelService;
import com.kairo.vistrel.streaming.FakeSessionServer;
import com.kairo.vistrel.util.Addresses;
import net.minecraft.network.chat.Style;
import net.minecraft.util.StringDecomposer;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
//? if <26.3 {
/*import net.minecraft.SharedConstants;
import net.minecraft.server.Bootstrap;
import org.junit.jupiter.api.BeforeAll;*/
//? }
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * 密码规则与应用路径测试：trim 后非空且长度 ≤64；
 * 合法值落配置快照，运行中即时生效，服务停止时不触达 server
 */
class VistrelCommandsTest {
    /**
     * 1.20/1.21 系的 HoverEvent 静态初始化会拉起 ItemStack，进而要求内置注册表就绪；
     * 不引导则渲染用例在 <26.3 上失败，26.3 的实现不再有这条静态链，无需引导
     */
    //? if <26.3 {
    /*@BeforeAll
    static void bootstrapMinecraft() {
        SharedConstants.tryDetectVersion();
        Bootstrap.bootStrap();
    }*/
    //? }
    @Test
    void testBlankPasswordRejected() {
        Assertions.assertNotNull(VistrelCommands.validatePassword(null), "null 必须拒绝");
        Assertions.assertNotNull(VistrelCommands.validatePassword(""), "空串必须拒绝");
        Assertions.assertNotNull(VistrelCommands.validatePassword("   "), "全空白必须拒绝");
    }

    @Test
    void testOverlongPasswordRejected() {
        Assertions.assertNotNull(VistrelCommands.validatePassword("a".repeat(65)), "65 字符必须拒绝");
    }

    @Test
    void testTooShortPasswordRejected() {
        Assertions.assertNotNull(VistrelCommands.validatePassword("a".repeat(5)), "不足最小长度必须拒绝");
    }

    @Test
    void testValidPasswordAccepted() {
        Assertions.assertNull(VistrelCommands.validatePassword(" abc123 "), "trim 后非空即合法");
        Assertions.assertNull(VistrelCommands.validatePassword("a".repeat(64)), "64 字符为上限内");
        Assertions.assertNull(VistrelCommands.validatePassword("a".repeat(6)), "最小长度为下限内");
    }

    @Test
    void testApplyPasswordChangeUpdatesConfigAndRunningServer() {
        String original = VistrelConfig.current().password();
        VistrelService.stop();
        FakeSessionServer fakeServer = new FakeSessionServer();
        VistrelService.start(
                () -> new VistrelConfig(30808, "old123", 128, false, true),
                fakeServer,
                new FakeSessionServer.FakeEncoderChannel(),
                new FakeSessionServer.FakeEncoderChannel()
        );
        try {
            String message = VistrelCommands.applyPasswordChange(" new456 ");
            Assertions.assertTrue(message.contains("new456"), "反馈须携带新密码");
            Assertions.assertEquals("new456", VistrelConfig.current().password(), "配置快照必须更新");
            Assertions.assertEquals("new456", fakeServer.getPassword(), "运行中的 server 必须即时生效");

            VistrelService.stop();
            VistrelCommands.applyPasswordChange("quiet789");
            Assertions.assertEquals("quiet789", VistrelConfig.current().password(), "服务停止时仅落配置");
            Assertions.assertEquals("new456", fakeServer.getPassword(), "服务停止后不得触达 server");
        } finally {
            VistrelService.stop();
            VistrelConfig.update(VistrelConfig.current().withPassword(original));
        }
    }

    @Test
    void testAccessUrlFollowsBoundAddress() {
        Assertions.assertEquals(
                "http://192.168.1.20:30808",
                Addresses.accessUrl("192.168.1.20", 30808),
                "具体地址必须原样拼进 URL"
        );
        // IPv6 回环只压短写法，不改地址族：应为 http://[::1]:30808
        Assertions.assertEquals("http://[::1]:30808", Addresses.accessUrl("::1", 30808), "IPv6 回环不得改写成 IPv4 回环");
        // 非回环 IPv6 保留方括号写法
        String ipv6 = Addresses.accessUrl("2001:db8::1", 30808);
        Assertions.assertTrue(ipv6.startsWith("http://[") && ipv6.endsWith("]:30808"), ipv6);
        // 通配绑定：取本机站点本地 IPv4，取不到落回环，两种情况都得是合法 URL
        String lan = Addresses.accessUrl("0.0.0.0", 30808);
        Assertions.assertTrue(lan.startsWith("http://") && lan.endsWith(":30808"), lan);
        Assertions.assertTrue(
                Addresses.accessUrl("no-such-host.invalid", 30808).endsWith(":30808"),
                "无法解析时也必须给出合法的回环 URL"
        );
    }

    /**
     * 访问地址读启动期缓存：解析只在启动线程做一次，status 反映启动配置，
     * 不跟随运行期配置快照变化；停机即清缓存，回落回环地址
     */
    @Test
    void testAccessUrlServedFromStartupCache() {
        VistrelService.stop();
        VistrelService.start(
                () -> new VistrelConfig(30808, "old123", 128, false, true),
                new FakeSessionServer(),
                new FakeSessionServer.FakeEncoderChannel(),
                new FakeSessionServer.FakeEncoderChannel()
        );
        int originalPort = VistrelConfig.current().port();
        try {
            Assertions.assertEquals(
                    Addresses.accessUrl(VistrelConfig.DEFAULT_BIND_ADDRESS, 30808),
                    VistrelService.getAccessUrl(),
                    "缓存必须是启动配置当刻的解析结果"
            );
            // 启动后改全局配置端口，已启动服务的展示地址不得跟着变：读缓存而非实时解析
            VistrelConfig.update(VistrelConfig.current().withPort(39999));
            String status = VistrelCommands.statusMessage().getString();
            Assertions.assertTrue(status.contains(":30808"), status);
            Assertions.assertFalse(status.contains("39999"), status);
        } finally {
            VistrelConfig.update(VistrelConfig.current().withPort(originalPort));
            VistrelService.stop();
        }
        Assertions.assertTrue(VistrelService.getAccessUrl().startsWith("http://127.0.0.1:"), "停机后回落回环地址");
    }

    /**
     * 状态行布局：统一两字标签、可点击地址与打码密码；
     * 停止态只留状态与启动提示，不显示地址密码
     */
    @Test
    void testStatusMessageLayoutHidesPasswordAndOffersActions() {
        VistrelService.stop();
        VistrelService.start(
                () -> new VistrelConfig(30808, "old123", 128, false, true),
                new FakeSessionServer(),
                new FakeSessionServer.FakeEncoderChannel(),
                new FakeSessionServer.FakeEncoderChannel()
        );
        try {
            String running = VistrelCommands.statusMessage().getString();
            Assertions.assertTrue(running.contains("▍") && running.contains("运行中"), running);
            Assertions.assertTrue(running.contains("地址"), running);
            Assertions.assertTrue(running.contains("http://"), running);
            // 地址与密码尾部不带文字后缀，但必须仍是可点击的富文本
            Assertions.assertFalse(running.contains("[打开]"), running);
            Assertions.assertFalse(running.contains("[复制]"), running);
            AtomicBoolean clickable = new AtomicBoolean(false);
            StringDecomposer.iterateFormatted(VistrelCommands.statusMessage(), Style.EMPTY, (index, style, codePoint) -> {
                clickable.set(clickable.get() || style.getClickEvent() != null);
                return true;
            });
            Assertions.assertTrue(clickable.get(), "地址与密码必须挂上点击事件");
            // 密码只该出现在悬停提示，不进聊天文本
            Assertions.assertFalse(running.contains(VistrelConfig.current().password()), running);
        } finally {
            VistrelService.stop();
            String stopped = VistrelCommands.statusMessage().getString();
            Assertions.assertTrue(stopped.contains("已停止"), stopped);
            Assertions.assertTrue(stopped.contains("/vistrel start"), stopped);
            Assertions.assertFalse(stopped.contains("http://"), stopped);
            // 未启动时没有编码能力可报，不该出现编码行
            Assertions.assertFalse(stopped.contains("编码"), stopped);
        }
    }

    /**
     * 密码打码必须是 obfuscated 样式（渲染期随机换字形），不是普通星号文本
     */
    @Test
    void testPasswordRenderedWithObfuscation() {
        VistrelService.stop();
        VistrelService.start(
                () -> new VistrelConfig(30808, "old123", 128, false, true),
                new FakeSessionServer(),
                new FakeSessionServer.FakeEncoderChannel(),
                new FakeSessionServer.FakeEncoderChannel()
        );
        try {
            AtomicBoolean obfuscated = new AtomicBoolean(false);
            StringDecomposer.iterateFormatted(VistrelCommands.statusMessage(), Style.EMPTY, (index, style, codePoint) -> {
                if (codePoint == '*') {
                    obfuscated.set(style.isObfuscated());
                }
                return true;
            });
            Assertions.assertTrue(obfuscated.get(), "密码必须以乱码样式渲染");
        } finally {
            VistrelService.stop();
        }
    }
}
