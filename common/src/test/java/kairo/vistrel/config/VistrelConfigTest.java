package kairo.vistrel.config;

import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * 配置加载测试
 */
class VistrelConfigTest {
    @TempDir
    Path tempDir;

    @Test
    void testMissingPasswordGeneratesSixCharCode() throws IOException {
        Files.writeString(tempDir.resolve("vistrel.properties"), "port=38081\naudioBitrate=128\n");

        try {
            VistrelConfig.load(tempDir);

            Assertions.assertEquals(6, VistrelConfig.current().password().length(), "无密码可用时自动生成 6 位随机码");
        } finally {
            VistrelConfig.load();
        }
    }

    @Test
    void testVramDirectReadFromConfigAndWrittenBack() throws IOException {
        // 缺键时按默认值 true 补全并回写；显式 false 必须生效
        Files.writeString(tempDir.resolve("vistrel.properties"), "port=38081\naudioBitrate=128\n");
        try {
            VistrelConfig.load(tempDir);

            Assertions.assertTrue(VistrelConfig.current().vramDirect(), "缺键时默认开启显存直通");
            Assertions.assertTrue(
                    Files.readString(tempDir.resolve("vistrel.properties")).contains("vramDirect=true"),
                    "缺键时必须回写默认值"
            );
        } finally {
            VistrelConfig.load();
        }

        Files.writeString(tempDir.resolve("vistrel.properties"), "vramDirect=false\n");
        try {
            VistrelConfig.load(tempDir);
            Assertions.assertFalse(VistrelConfig.current().vramDirect(), "vramDirect=false 必须关闭显存直通");
        } finally {
            VistrelConfig.load();
        }
    }

    @Test
    void testAutoPasswordRotatesEachLaunchAndNeverPersists() throws IOException {
        Path file = tempDir.resolve("vistrel.properties");
        Files.writeString(file, "port=38081\n");
        try {
            VistrelConfig.load(tempDir);
            Assertions.assertTrue(VistrelConfig.current().ephemeralPassword(), "缺键应生成启动期临时口令");
            Assertions.assertTrue(Files.readString(file).contains("password=auto"), "临时口令不得明文落盘，应写回哨兵值 auto");
            String first = VistrelConfig.current().password();

            VistrelConfig.load(tempDir);
            Assertions.assertNotEquals(first, VistrelConfig.current().password(), "每次启动必须换新口令");
        } finally {
            VistrelConfig.load();
        }
    }

    @Test
    void testBindAddressDefaultsToAllInterfacesAndWrittenBack() throws IOException {
        Path file = tempDir.resolve("vistrel.properties");
        Files.writeString(file, "port=38081\n");
        try {
            VistrelConfig.load(tempDir);
            Assertions.assertEquals(
                    VistrelConfig.DEFAULT_BIND_ADDRESS,
                    VistrelConfig.current().bindAddress(),
                    "缺键时默认监听全部网卡"
            );
            Assertions.assertTrue(Files.readString(file).contains("bindAddress=0.0.0.0"), "缺键时必须回写默认值");
        } finally {
            VistrelConfig.load();
        }
    }

    @Test
    void testBindAddressReadFromConfigAndSurvivesWithPort() throws IOException {
        Files.writeString(tempDir.resolve("vistrel.properties"), "port=38081\nbindAddress=192.168.1.20\n");
        try {
            VistrelConfig.load(tempDir);
            Assertions.assertEquals("192.168.1.20", VistrelConfig.current().bindAddress(), "显式监听地址必须生效");
            Assertions.assertEquals(
                    "192.168.1.20",
                    VistrelConfig.current().withPort(38082).bindAddress(),
                    "改端口不得丢掉监听地址"
            );
        } finally {
            VistrelConfig.load();
        }
    }

    @Test
    void testExplicitPasswordPinned() throws IOException {
        Files.writeString(tempDir.resolve("vistrel.properties"), "password=abc123\n");
        try {
            VistrelConfig.load(tempDir);
            Assertions.assertEquals("abc123", VistrelConfig.current().password(), "显式口令必须原样生效");
            Assertions.assertFalse(VistrelConfig.current().ephemeralPassword(), "显式口令不属临时口令");
            Assertions.assertFalse(
                    VistrelConfig.current().withPassword("xyz789").ephemeralPassword(),
                    "游戏内显式改口令后必须固定下来"
            );
        } finally {
            VistrelConfig.load();
        }
    }

    @Test
    void testBlankPasswordDisablesAuth() throws IOException {
        Files.writeString(tempDir.resolve("vistrel.properties"), "port=38081\npassword=\n");
        try {
            VistrelConfig.load(tempDir);
            Assertions.assertEquals("", VistrelConfig.current().password(), "空值口令表示禁用密码认证");
            Assertions.assertFalse(VistrelConfig.current().ephemeralPassword(), "空值不是临时口令");
        } finally {
            VistrelConfig.load();
        }
    }

    @Test
    void testTooShortPasswordFallsBackToEphemeralAndKeepsLiteral() throws IOException {
        Path file = tempDir.resolve("vistrel.properties");
        Files.writeString(file, "password=abc\n");
        try {
            VistrelConfig.load(tempDir);
            Assertions.assertTrue(VistrelConfig.current().ephemeralPassword(), "过短字面值不得直接生效");
            Assertions.assertEquals(
                    VistrelConfig.MIN_PASSWORD_LENGTH,
                    VistrelConfig.current().password().length(),
                    "回退临时口令长度必须等于最小长度"
            );
            Assertions.assertTrue(Files.readString(file).contains("password=abc"), "过短字面值保留原值待修正，不得覆盖");
        } finally {
            VistrelConfig.load();
        }
    }
}
