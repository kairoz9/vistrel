package kairo.vistrel;

import kairo.vistrel.config.VistrelConfig;
import kairo.vistrel.platform.Services;

/**
 * Common 跨平台共享入口点
 */
public class CommonBootstrap {
    /**
     * 模组启动入口（各加载器客户端初始化钩子调用）：加载全局配置快照；
     * 开发环境额外自动启动推流服务，生产环境不自动开启
     */
    public static void init() {
        Constants.LOG.info(
                "Initializing Vistrel on {} (Env: {})",
                Services.PLATFORM.getPlatformName(),
                Services.PLATFORM.getEnvironmentName()
        );

        VistrelConfig.load();
        // 生产环境不自动开启
        if (Services.PLATFORM.isDevelopmentEnvironment()) {
            VistrelService.start();
        }
    }
}
