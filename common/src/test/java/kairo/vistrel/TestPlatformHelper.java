package kairo.vistrel;

import kairo.vistrel.platform.PlatformHelper;
import java.nio.file.Path;

// 测试用平台实现：common 自身不带 loader 的 PlatformHelper provider，
// 由 src/test/resources/META-INF/services 注册，保证 Services.PLATFORM 可加载
public class TestPlatformHelper implements PlatformHelper {
    @Override
    public String getPlatformName() {
        return "test";
    }

    @Override
    public Path getConfigDir() {
        return Path.of("config");
    }

    @Override
    public boolean isModLoaded(String modId) {
        return false;
    }

    @Override
    public boolean isDevelopmentEnvironment() {
        return true;
    }
}
