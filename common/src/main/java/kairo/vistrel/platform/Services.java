package kairo.vistrel.platform;

import kairo.vistrel.Constants;
import java.util.ServiceLoader;

// 经 ServiceLoader 加载平台实现：各 loader 在 META-INF/services 声明各自的 PlatformHelper 实现类
public class Services {
    public static final PlatformHelper PLATFORM = load(PlatformHelper.class);

    /**
     * 经 ServiceLoader 加载平台服务实现（各 loader 在 META-INF/services 声明实现类）：
     * SPI 加载边界，实现缺席时 fail-fast，不提供任何回退
     *
     * @param clazz 服务接口类型
     * @param <T>   服务接口类型
     * @return classpath 上注册的首个实现
     * @throws NullPointerException 未注册任何实现时抛出
     */
    public static <T> T load(Class<T> clazz) {
        final T loadedService = ServiceLoader
            .load(clazz, Services.class.getClassLoader())
            .findFirst()
            .orElseThrow(() -> new NullPointerException("Failed to load service for " + clazz.getName()));
        Constants.LOG.debug("Loaded {} for service {}", loadedService, clazz);
        return loadedService;
    }
}
