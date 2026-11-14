package com.kairo.vistrel;

import com.kairo.vistrel.command.VistrelCommands;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.client.command.v2.ClientCommandRegistrationCallback;
import net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientTickEvents;

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
