package com.kairo.vistrel.mixin;

import com.kairo.vistrel.Constants;
import com.kairo.vistrel.VistrelService;
import com.kairo.vistrel.capture.AudioCaptureScheduler;
import com.llamalad7.mixinextras.injector.wrapoperation.Operation;
import com.llamalad7.mixinextras.injector.wrapoperation.WrapOperation;
import com.mojang.blaze3d.audio.Library;
import org.lwjgl.openal.ALC10;
import org.lwjgl.openal.SOFTLoopback;
import org.lwjgl.system.MemoryStack;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;
import java.nio.ByteBuffer;
import java.nio.IntBuffer;
import java.util.OptionalLong;

/**
 * 注入 Minecraft blaze3d 音频库的初始化流程
 *
 * <p>把物理声卡的打开接管为 OpenAL Soft 内存环回设备 (ALC_SOFT_loopback)，
 * 并为其装配 48kHz 立体声 16-bit PCM 上下文；Minecraft 原生的通道池分配、
 * HRTF 空间化与 EFX 混响扩展探测不受影响。
 */
@Mixin(Library.class)
public abstract class MixinLibrary {
    /**
     * 拦截设备打开 (tryOpenDevice HEAD)：
     * OpenAL 运行时支持 ALC_SOFT_loopback 扩展时创建内存环回设备并接管
     */
    @Inject(method = "tryOpenDevice", at = @At("HEAD"), cancellable = true)
    private static void vistrel$tryOpenLoopbackDevice(String deviceName, CallbackInfoReturnable<OptionalLong> cir) {
        try {
            if (ALC10.alcIsExtensionPresent(0L, "ALC_SOFT_loopback")) {
                long device = SOFTLoopback.alcLoopbackOpenDeviceSOFT((ByteBuffer) null);
                if (device != 0L) {
                    AudioCaptureScheduler.LoopbackBinding.open(device);
                    Constants.LOG.info("Vistrel 成功接管 OpenAL 音频设备 (ALC_SOFT_loopback: 0x{})", Long.toHexString(device));
                    cir.setReturnValue(OptionalLong.of(device));
                    return;
                } else {
                    Constants.LOG.warn("alcLoopbackOpenDeviceSOFT 返回空句柄，降级回退原生物理声卡");
                }
            } else {
                Constants.LOG.warn("当前 OpenAL 运行时不支持 ALC_SOFT_loopback 扩展，音频推流降级为静音模式");
            }
        } catch (Throwable t) {
            Constants.LOG.warn("探测或创建 OpenAL 环回设备异常，降级回退原生物理声卡: {}", t.getMessage());
        }
    }

    /**
     * 拦截 Context 创建 (alcCreateContext)：
     * 为环回设备替换 48kHz Stereo 16-bit PCM 属性表（属性组装在 AudioCaptureScheduler）
     */
    @WrapOperation(
            method = "init",
            at = @At(
                    value = "INVOKE",
                    target = "Lorg/lwjgl/openal/ALC10;" + "alcCreateContext(JLjava/nio/IntBuffer;)J",
                    remap = false
            )
    )
    private long vistrel$wrapCreateContext(long device, IntBuffer originalAttrs, Operation<Long> original) {
        AudioCaptureScheduler.LoopbackBinding binding = AudioCaptureScheduler.LoopbackBinding.current();
        if (binding.isBound() && device == binding.device()) {
            int hrtf = AudioCaptureScheduler.extractHrtfPreference(originalAttrs);
            try (MemoryStack stack = MemoryStack.stackPush()) {
                return original.call(device, AudioCaptureScheduler.loopbackContextAttributes(stack, hrtf));
            }
        }
        return original.call(device, originalAttrs);
    }

    /**
     * 在音频库完整初始化完成后激活音频采集 (确保 context 已创建并激活为 current)
     */
    @Inject(method = "init", at = @At("TAIL"))
    private void vistrel$onAudioInit(CallbackInfo ci) {
        if (AudioCaptureScheduler.LoopbackBinding.current().isBound()) {
            AudioCaptureScheduler.LoopbackBinding.markContextReady();
            AudioCaptureScheduler audioScheduler = VistrelService.getAudioScheduler();
            if (audioScheduler != null) {
                audioScheduler.onContextReady();
            }
        }
    }

    /**
     * 在音频库清理阶段作废绑定正本 (赶在 Minecraft 销毁 context 之前执行)，
     * 并等在途环回采样退出
     */
    @Inject(method = "cleanup", at = @At("HEAD"))
    private void vistrel$onAudioCleanup(CallbackInfo ci) {
        AudioCaptureScheduler.LoopbackBinding.clear();
        AudioCaptureScheduler audioScheduler = VistrelService.getAudioScheduler();
        if (audioScheduler != null) {
            audioScheduler.awaitRenderIdle();
        }
    }
}
