package kairo.vistrel.platform;

import java.nio.file.Path;

public interface PlatformHelper {
    /**
     * 当前平台名称
     */
    String getPlatformName();

    /**
     * 获取模组配置目录
     *
     * @return 配置目录路径（不保证已存在）
     */
    Path getConfigDir();

    /**
     * 指定模组是否已加载
     *
     * @param modId 模组 ID
     * @return true 表示该模组已加载
     */
    boolean isModLoaded(String modId);

    /**
     * 是否处于开发环境
     */
    boolean isDevelopmentEnvironment();

    /**
     * 环境类型名称 (development / production)
     */
    default String getEnvironmentName() {
        return isDevelopmentEnvironment() ? "development" : "production";
    }
}
