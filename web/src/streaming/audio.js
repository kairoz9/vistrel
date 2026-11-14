/**
 * Vistrel Web Client 音频管理模块
 * 负责规避浏览器 Autoplay 策略拦截并在用户首个手势（点击/锁定）时解锁音频输出
 */

export function tryUnmuteAudio(audioElement) {
    if (!audioElement) return;
    audioElement.muted = false;
    audioElement.volume = 1.0;
    audioElement.play().catch((e) => {
        console.debug("Vistrel: Audio autoplay deferred to user interaction:", e);
    });
}
