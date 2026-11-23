package kairo.vistrel;

import kairo.vistrel.command.VistrelCommands;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.client.command.v2.ClientCommandRegistrationCallback;
import net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientTickEvents;

/**
 * Fabric 引导入口
 *
 * <p>environment 声明为 client，专用服务端在 ModDiscoverer 阶段即丢弃整个模组，
 * 因此本类可直接引用客户端 API。
 */
public class Vistrel implements ModInitializer {
    @Override
    public void onInitialize() {
        CommonBootstrap.init();
        // 注册游戏内服务控制命令：树体在 common，此处仅薄适配
        ClientCommandRegistrationCallback.EVENT.register((dispatcher, buildContext) -> dispatcher.register(VistrelCommands.tree()
        ));
        // 内容采样随客户端 tick（20Hz）
        ClientTickEvents.END_CLIENT_TICK.register(client -> VistrelService.onClientTick());
    }
}
