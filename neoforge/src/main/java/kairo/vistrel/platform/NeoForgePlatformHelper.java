package kairo.vistrel.platform;

import net.neoforged.fml.ModList;
import net.neoforged.fml.loading.FMLPaths;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.nio.file.Path;

public class NeoForgePlatformHelper implements PlatformHelper {
    @Override
    public String getPlatformName() {
        return "NeoForge";
    }

    @Override
    public Path getConfigDir() {
        return FMLPaths.CONFIGDIR.get();
    }

    @Override
    public boolean isModLoaded(String modId) {
        return ModList.get().isLoaded(modId);
    }

    @Override
    public boolean isDevelopmentEnvironment() {
        try {
            Class<?> fmlLoader = Class.forName("net.neoforged.fml.loading.FMLLoader");
            Method isProduction = fmlLoader.getMethod("isProduction");
            if (Modifier.isStatic(isProduction.getModifiers())) {
                return !(boolean) isProduction.invoke(null);
            }
            // FML 10+ isProduction 为实例方法，经静态 getCurrent() 取单例
            Object current = fmlLoader.getMethod("getCurrent").invoke(null);
            return !(boolean) isProduction.invoke(current);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException("FML 生产环境判定失败", e);
        }
    }
}
