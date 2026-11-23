package kairo.vistrel;

import net.minecraftforge.fml.common.Mod;
import net.minecraftforge.fml.loading.FMLEnvironment;

/**
 * Forge 引导入口
 *
 * <p>1.20 的 {@code @Mod} 无 dist 参数，故在构造器首行短路服务端；客户端符号全部
 * 下沉至 {@link VistrelClientBootstrap}，使本类字节码不含 {@code net.minecraft.client} 引用。
 */
@Mod(Constants.MOD_ID)
public class Vistrel {
    public Vistrel() {
        if (!FMLEnvironment.dist.isClient()) {
            return;
        }
        CommonBootstrap.init();
        VistrelClientBootstrap.init();
    }
}
