package com.kairo.vistrel;

import com.kairo.vistrel.command.VistrelCommands;
import net.minecraftforge.client.event.RegisterClientCommandsEvent;
import net.minecraftforge.common.MinecraftForge;
import net.minecraftforge.event.TickEvent;
import net.minecraftforge.fml.common.Mod;

@Mod(Constants.MOD_ID)
public class Vistrel {
    public Vistrel() {
        CommonBootstrap.init();
        // 注册游戏内服务控制命令：客户端命令在 game bus，树体在 common
        MinecraftForge.EVENT_BUS.addListener(this::onRegisterClientCommands);
        // 内容采样随客户端 tick（20Hz）
        MinecraftForge.EVENT_BUS.addListener((TickEvent.ClientTickEvent event) -> {
            if (event.phase == TickEvent.Phase.END) {
                VistrelService.onClientTick();
            }
        });
    }

    private void onRegisterClientCommands(RegisterClientCommandsEvent event) {
        event.getDispatcher().register(VistrelCommands.tree());
    }
}
