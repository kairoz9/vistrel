import { describe, expect, it } from "vitest";
import { computeViewportSize, resolveFrameGeometry } from "./video-geometry.js";

describe("resolveFrameGeometry", () => {
    it("1080p AV1：编码 1920×1088，显示区仍是 1920×1080，只放大纵向", () => {
        const geometry = resolveFrameGeometry(1920, 1088, 1920, 1080);
        expect(geometry).not.toBeNull();
        expect(geometry.displayWidth).toBe(1920);
        expect(geometry.displayHeight).toBe(1080);
        expect(geometry.scaleX).toBe(1);
        expect(geometry.scaleY).toBeCloseTo(1088 / 1080, 10);
    });

    it("两轴都有补齐：显示区取左上角，两个方向分别放大", () => {
        const geometry = resolveFrameGeometry(1536, 832, 1528, 818);
        expect(geometry.displayWidth).toBe(1528);
        expect(geometry.displayHeight).toBe(818);
        expect(geometry.scaleX).toBeCloseTo(1536 / 1528, 10);
        expect(geometry.scaleY).toBeCloseTo(832 / 818, 10);
    });

    it("无补齐：显示区与解码帧一致，不放大", () => {
        expect(resolveFrameGeometry(1280, 720, 1280, 720)).toEqual({
            displayWidth: 1280,
            displayHeight: 720,
            scaleX: 1,
            scaleY: 1,
        });
    });

    it("显示几何尚未获得：整帧显示", () => {
        expect(resolveFrameGeometry(1920, 1088, 0, 0)).toEqual({
            displayWidth: 1920,
            displayHeight: 1088,
            scaleX: 1,
            scaleY: 1,
        });
    });

    it("解码帧小于显示几何：不反向放大，整帧显示", () => {
        // 显示几何陈旧或解码器已自行裁剪时，真实内容只可能更小，不能按比例放大
        expect(resolveFrameGeometry(1280, 720, 1920, 1080)).toEqual({
            displayWidth: 1280,
            displayHeight: 720,
            scaleX: 1,
            scaleY: 1,
        });
    });

    it("差值超出一个补齐量级：视为换档在途，整帧显示", () => {
        // 1920×1152 与 1920×1080 差 72 > 64：不是对齐补齐，不能裁
        expect(resolveFrameGeometry(1920, 1152, 1920, 1080)).toEqual({
            displayWidth: 1920,
            displayHeight: 1152,
            scaleX: 1,
            scaleY: 1,
        });
        // 换档前视频仍是 1920×1088、显示几何已是 1280×720：绝不能按 1.5 倍裁
        expect(resolveFrameGeometry(1920, 1088, 1280, 720)).toEqual({
            displayWidth: 1920,
            displayHeight: 1088,
            scaleX: 1,
            scaleY: 1,
        });
    });

    it("解码帧尺寸未知：返回 null", () => {
        expect(resolveFrameGeometry(0, 0, 1920, 1080)).toBeNull();
    });
});

describe("computeViewportSize", () => {
    it("contain：按显示比例塞进可见区并留边", () => {
        expect(computeViewportSize(800, 600, 1600, 900, "contain")).toEqual({ width: 800, height: 450 });
    });

    it("contain：可见区大于显示几何时等比放大", () => {
        expect(computeViewportSize(1920, 1080, 1280, 720, "contain")).toEqual({ width: 1920, height: 1080 });
    });

    it("cover：等比铺满可见区，超出部分交给外层裁剪", () => {
        const size = computeViewportSize(800, 600, 1600, 900, "cover");
        expect(size.width).toBeCloseTo(1600 * (600 / 900), 10);
        expect(size.height).toBe(600);
    });

    it("none：显示区域原始像素 1:1", () => {
        expect(computeViewportSize(800, 600, 1600, 900, "none")).toEqual({ width: 1600, height: 900 });
    });

    it("任一尺寸非正：返回 null", () => {
        expect(computeViewportSize(0, 600, 1600, 900, "contain")).toBeNull();
        expect(computeViewportSize(800, 600, 0, 0, "contain")).toBeNull();
    });
});
