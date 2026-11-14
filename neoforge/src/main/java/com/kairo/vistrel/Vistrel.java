package com.kairo.vistrel;

import com.kairo.vistrel.command.VistrelCommands;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.client.event.ClientTickEvent;
import net.neoforged.neoforge.client.event.RegisterClientCommandsEvent;
import net.neoforged.neoforge.common.NeoForge;

@Mod(Constants.MOD_ID)
public class Vistrel {
    public Vistrel(IEventBus eventBus) {
        CommonBootstrap.init();
        // 注册游戏内服务控制命令：客户端命令在 game bus（非 mod bus），树体在 common
        NeoForge.EVENT_BUS.addListener(this::onRegisterClientCommands);
        // 内容采样随客户端 tick（20Hz）
        NeoForge.EVENT_BUS.addListener(ClientTickEvent.Post.class, event -> VistrelService.onClientTick());
    }

    private void onRegisterClientCommands(RegisterClientCommandsEvent event) {
        event.getDispatcher().register(VistrelCommands.tree());
    }
}
