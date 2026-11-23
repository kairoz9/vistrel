/**
 * 显示视口几何：把解码帧的显示区域映射到可见游戏区。
 *
 * 解码帧可能大于当前显示几何：编码尺寸按后端的 `pictureAccessGranularity` 向上对齐
 * （AV1 本机为 64×64），多出的行/列在右/下边缘；显示区域恒为左上角的 display 尺寸，
 * 补齐边由 `#videoViewport` 裁掉。
 */

/**
 * 允许当作补齐裁掉的单轴最大差值，与 `rust-native/src/encode.rs` 的 `MAX_CODED_ALIGN`
 * 同一上界（后者变更时同步）。超过它说明显示几何与解码帧不属于同一次协商，按整帧显示——
 * 宁可多显示一条边，也不裁掉真实画面。
 */
const MAX_CODING_PADDING = 64;

/**
 * 解析解码帧的显示区域与放大比。
 *
 * @param {number} videoWidth 解码帧宽度
 * @param {number} videoHeight 解码帧高度
 * @param {number} displayWidth 当前生效的显示宽度（0 = 尚未获得；Answer 初值或推送）
 * @param {number} displayHeight 当前生效的显示高度（0 = 尚未获得；Answer 初值或推送）
 * @returns {{displayWidth: number, displayHeight: number, scaleX: number, scaleY: number} | null}
 *   显示区域尺寸与视频元素相对显示视口的放大比；解码帧尺寸未知时返回 null
 */
export function resolveFrameGeometry(videoWidth, videoHeight, displayWidth, displayHeight) {
    if (!(videoWidth > 0) || !(videoHeight > 0)) return null;

    const paddingX = videoWidth - displayWidth;
    const paddingY = videoHeight - displayHeight;
    const croppable =
        displayWidth > 0 &&
        displayHeight > 0 &&
        paddingX >= 0 &&
        paddingY >= 0 &&
        paddingX <= MAX_CODING_PADDING &&
        paddingY <= MAX_CODING_PADDING;

    if (!croppable) {
        return { displayWidth: videoWidth, displayHeight: videoHeight, scaleX: 1, scaleY: 1 };
    }
    return {
        displayWidth,
        displayHeight,
        scaleX: videoWidth / displayWidth,
        scaleY: videoHeight / displayHeight,
    };
}

/**
 * 计算显示视口在 game-surface 内的尺寸（CSS 像素）：显示区域的等比缩放，
 * 裁剪只发生在视口内部（视频元素的右/下补齐边）；cover 下视口大于 surface，
 * 由 surface 的 `overflow: hidden` 居中裁切；none 按原始像素 1:1。
 *
 * @param {number} surfaceWidth 可见游戏区宽度
 * @param {number} surfaceHeight 可见游戏区高度
 * @param {number} displayWidth 显示区域宽度
 * @param {number} displayHeight 显示区域高度
 * @param {string} fitMode 显示适配模式：contain / cover / none
 * @returns {{width: number, height: number} | null} 视口尺寸；任一输入非正时返回 null
 */
export function computeViewportSize(surfaceWidth, surfaceHeight, displayWidth, displayHeight, fitMode) {
    if (!(surfaceWidth > 0 && surfaceHeight > 0 && displayWidth > 0 && displayHeight > 0)) return null;
    if (fitMode === "none") return { width: displayWidth, height: displayHeight };

    const scaleX = surfaceWidth / displayWidth;
    const scaleY = surfaceHeight / displayHeight;
    const scale = fitMode === "cover" ? Math.max(scaleX, scaleY) : Math.min(scaleX, scaleY);
    return { width: displayWidth * scale, height: displayHeight * scale };
}
