package com.kairo.vistrel;

import com.kairo.vistrel.command.VistrelCommands;
import net.minecraftforge.client.event.RegisterClientCommandsEvent;
import net.minecraftforge.common.MinecraftForge;
import net.minecraftforge.event.TickEvent;

/**
 * Forge 客户端专属接线
 *
 * <p>类体内符号均来自 {@code net.minecraft.client.*} / {@code net.minecraftforge.client.*}，
 * 专用服务端分布里不存在；拆出本类使 {@link Vistrel} 的字节码不持有这些引用，
 * 服务端不调用 {@link #init()} 即安全。
 */
final class VistrelClientBootstrap {
    private VistrelClientBootstrap() {
    }

    /**
     * 注册客户端命令树与逐 tick 采样回调
     *
     * <p>
     * 只能在物理客户端调用：命令树与 tick 回调都直接构造客户端类型实例。
     */
    static void init() {
        // 注册游戏内服务控制命令：客户端命令在 game bus，树体在 common
        MinecraftForge.EVENT_BUS.addListener(VistrelClientBootstrap::onRegisterClientCommands);
        // 内容采样随客户端 tick（20Hz）
        MinecraftForge.EVENT_BUS.addListener((TickEvent.ClientTickEvent event) -> {
            if (event.phase == TickEvent.Phase.END) {
                VistrelService.onClientTick();
            }
        });
    }

    private static void onRegisterClientCommands(RegisterClientCommandsEvent event) {
        event.getDispatcher().register(VistrelCommands.tree());
    }
}
