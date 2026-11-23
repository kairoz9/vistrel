package kairo.vistrel.capture;

//? if gpu_device {
//? if vulkan_backend {
import com.mojang.renderpearl.api.GpuFormat;
import com.mojang.renderpearl.api.pipeline.BindGroupLayout;
//? } else {
/*import com.mojang.blaze3d.textures.TextureFormat;*/
//? }
import com.mojang.renderpearl.api.pipeline.ColorTargetState;
import com.mojang.renderpearl.api.pipeline.RenderPipeline;
import com.mojang.renderpearl.api.vertex.VertexFormat;
import net.minecraft.client.renderer.RenderPipelines;
import net.minecraft.resources.Identifier;
import java.util.List;
import java.util.Optional;

/**
 * 以 vanilla {@code RenderPipelines.TRACY_BLIT} 为模板装配模组自有的 blit 管线。
 *
 * <p>
 * 绑定组、推送常量大小、光栅状态、顶点绑定与拓扑必须与模板一致：GL 与 Vulkan 的管线布局都从
 * 这些字段推导，少一项在 Vulkan 上就是「查找或加载管线失败」。vanilla 由
 * {@code RenderPipelines.GLOBALS_SNIPPET} 提供的 GLOBALS 绑定组在模板 build 时已折进
 * {@code getBindGroupLayouts()}，整表复制即已包含，无需再显式传 snippet。
 */
public final class BlitPipelines {
    private static final RenderPipeline TEMPLATE = RenderPipelines.TRACY_BLIT;
    /**
     * 以下 GLSL 与 vanilla core 着色器同版本约定（#version 330 + 分离着色器对象），以字符串内联、
     * 经设备源码注入接口编译
     */
    static final String NV12_Y_FRAGMENT =
            """
        #version 330
        #extension GL_ARB_separate_shader_objects : require

        uniform sampler2D InSampler;

        layout(location = 0) in vec2 texCoord;

        layout(location = 0) out vec4 fragColor;

        // 亮度平面：BT.709
        void main() {
            // 线格式翻转为顶行在前：源纹理行序为底行在前，V 反转采样
            vec3 rgb = texture(InSampler, vec2(texCoord.x, 1.0 - texCoord.y)).rgb;
            float luma = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
            fragColor = vec4((16.0 + luma * 219.0) / 255.0, 0.0, 0.0, 1.0);
        }
        """;
    //? if vulkan_backend {
    static final String NV12_UV_FRAGMENT =
            """
        #version 330
        #extension GL_ARB_separate_shader_objects : require

        uniform sampler2D InSampler;

        layout(location = 0) in vec2 texCoord;

        layout(location = 0) out vec4 fragColor;

        // 色度平面：每目标纹素取源画面 2×2 盒式平均，再按 BT.709 有限范围算 Cb/Cr。
        // 目标为半分辨率时 texCoord 落在每个 2×2 块的中心，故偏移半个源纹素即可命中四个源纹素
        void main() {
            vec2 halfTexel = 0.5 / vec2(textureSize(InSampler, 0));
            // 线格式翻转为顶行在前：源行序底行在前，V 反转后四采样命中镜像 2x2 块
            vec2 flipped = vec2(texCoord.x, 1.0 - texCoord.y);
            vec3 rgb = texture(InSampler, flipped + vec2(-halfTexel.x, -halfTexel.y)).rgb
                     + texture(InSampler, flipped + vec2( halfTexel.x, -halfTexel.y)).rgb
                     + texture(InSampler, flipped + vec2(-halfTexel.x,  halfTexel.y)).rgb
                     + texture(InSampler, flipped + vec2( halfTexel.x,  halfTexel.y)).rgb;
            rgb *= 0.25;

            float luma = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
            float cb = (rgb.b - luma) / 1.8556;
            float cr = (rgb.r - luma) / 1.5748;

            // Cb 写入 R 通道、Cr 写入 G 通道
            fragColor = vec4((128.0 + cb * 224.0) / 255.0, (128.0 + cr * 224.0) / 255.0, 0.0, 1.0);
        }
        """;
    //? } else {
    /*static final String NV12_UV_FRAGMENT =
            """
        #version 330
        #extension GL_ARB_separate_shader_objects : require

        uniform sampler2D InSampler;

        layout(location = 0) out vec4 fragColor;

        // 26.1 缺 RG8：目标为宽翻倍的 R8 交错面（w × h/2），偶数目标列写 Cb、奇数列写 Cr
        // （NV12 的 UV 交错序），整面拷贝回读恰为 NV12 色度面。每目标纹素对应 chromel
        // （x/2, y），取源画面该 chromel 的 2×2 盒式平均，再按 BT.709 有限范围算 Cb/Cr
        void main() {
            vec2 srcSize = vec2(textureSize(InSampler, 0));
            // chromel 中心在源纹理中的 UV：目标宽度跨两个 chromel 列，以纹素坐标直接取 chromel
            vec2 chromel = vec2(int(gl_FragCoord.x) / 2, int(gl_FragCoord.y));
            vec2 center = (chromel + 0.5) / (srcSize * 0.5);
            // 线格式翻转为顶行在前：源行序底行在前，V 反转后四采样命中镜像 2x2 块
            vec2 flipped = vec2(center.x, 1.0 - center.y);
            vec2 halfTexel = 0.5 / srcSize;
            vec3 rgb = texture(InSampler, flipped + vec2(-halfTexel.x, -halfTexel.y)).rgb
                     + texture(InSampler, flipped + vec2( halfTexel.x, -halfTexel.y)).rgb
                     + texture(InSampler, flipped + vec2(-halfTexel.x,  halfTexel.y)).rgb
                     + texture(InSampler, flipped + vec2( halfTexel.x,  halfTexel.y)).rgb;
            rgb *= 0.25;

            float luma = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
            float cb = (128.0 + (rgb.b - luma) / 1.8556 * 224.0) / 255.0;
            float cr = (128.0 + (rgb.r - luma) / 1.5748 * 224.0) / 255.0;

            // UV 交错：偶数目标列承载 Cb，奇数目标列承载 Cr
            bool isU = int(gl_FragCoord.x) % 2 == 0;
            fragColor = vec4(isU ? cb : cr, 0.0, 0.0, 1.0);
        }
        """;*/
    //? }
    static final String BLIT_FLIP_FRAGMENT =
            """
        #version 330
        #extension GL_ARB_separate_shader_objects : require

        uniform sampler2D InSampler;

        layout(location = 0) in vec2 texCoord;

        layout(location = 0) out vec4 fragColor;

        // 显存槽位专用翻转 blit：mainTarget 行序为底行在前（GL 语义，与渲染后端无关），
        // 行序翻正在此一次完成——槽位内容自此恒为顶行在前，下游 NVENC ABGR 直编与
        // Vulkan Video CSC（恒等采样）都按顶行在前消费，勿再翻
        void main() {
            fragColor = texture(InSampler, vec2(texCoord.x, 1.0 - texCoord.y));
        }
        """;

    /**
     * vistrel 内嵌 fragment 源码查找：path 为着色器 id 的路径（core/&lt;name&gt;），未知路径快速失败
     */
    static String inlineFragment(String path) {
        return switch (path) {
            case "core/nv12_y" -> NV12_Y_FRAGMENT;
            case "core/nv12_uv" -> NV12_UV_FRAGMENT;
            case "core/blit_flip" -> BLIT_FLIP_FRAGMENT;
            default -> throw new IllegalStateException("未知着色器: " + path);
        };
    }

    private BlitPipelines() {
    }

    /**
     * 全屏三角 blit 管线：顶点着色器固定为 {@code minecraft:core/screenquad}
     *
     * @param name 管线与 fragment shader 短名：{@code vistrel:pipeline/<name>} 与 {@code vistrel:core/<name>}
     * @param format 颜色附件格式
     * @param writeMask 颜色附件写掩码
     */
    static RenderPipeline screenquad(String name, GpuFormat format, int writeMask) {
        //? if vulkan_backend {
        RenderPipeline.Builder builder = RenderPipeline
            .builder()
            .withLocation(Identifier.fromNamespaceAndPath("vistrel", "pipeline/" + name))
            .withVertexShader(Identifier.fromNamespaceAndPath("minecraft", "core/screenquad"))
            .withFragmentShader(Identifier.fromNamespaceAndPath("vistrel", "core/" + name))
            .withPrimitiveTopology(TEMPLATE.getPrimitiveTopology())
            .withCull(TEMPLATE.isCull())
            .withPolygonMode(TEMPLATE.getPolygonMode())
            .withDepthStencilState(Optional.ofNullable(TEMPLATE.getDepthStencilState()));
        //? if renderpearl_era {
        builder.withPushConstantSize(TEMPLATE.pushConstantSize());
        //? }
        for (BindGroupLayout layout : TEMPLATE.getBindGroupLayouts()) {
            builder.withBindGroupLayout(layout);
        }
        //? if renderpearl_era {
        List<VertexFormat> vertexBindings = TEMPLATE.getVertexFormatBindings();
        for (int i = 0; i < vertexBindings.size(); i++) {
            builder.withVertexBinding(i, vertexBindings.get(i));
        }
        //? } else {
        /*VertexFormat[] vertexBindings = TEMPLATE.getVertexFormatBindings();
        for (int i = 0; i < vertexBindings.length; i++) {
            builder.withVertexBinding(i, vertexBindings[i]);
        }*/
        //? }
        return builder.withColorTargetState(new ColorTargetState(Optional.empty(), format, writeMask)).build();
        //? } else {
        /*// 26.1 为上一代管线构建层：采样器/Uniform 按名字声明，顶点格式单数
        RenderPipeline.Builder builder = RenderPipeline
            .builder()
            .withLocation(Identifier.fromNamespaceAndPath("vistrel", "pipeline/" + name))
            .withVertexShader(Identifier.fromNamespaceAndPath("minecraft", "core/screenquad"))
            .withFragmentShader(Identifier.fromNamespaceAndPath("vistrel", "core/" + name))
            .withPolygonMode(TEMPLATE.getPolygonMode())
            .withCull(TEMPLATE.isCull())
            .withDepthStencilState(Optional.ofNullable(TEMPLATE.getDepthStencilState()))
            .withVertexFormat(TEMPLATE.getVertexFormat(), TEMPLATE.getVertexFormatMode())
            .withSampler("InSampler");
        for (var uniform : TEMPLATE.getUniforms()) {
            builder.withUniform(uniform.name(), uniform.type(), uniform.textureFormat());
        }
        return builder.withColorTargetState(new ColorTargetState(Optional.empty(), writeMask)).build();*/
        //? }
    }

    /**
     * NV12 采集用的两条自建渲染管线：全分辨率亮度平面 + 色度平面（4:2:0 采样）。
     *
     * <p>
     * 以 vanilla 的 {@code RenderPipelines.TRACY_BLIT} 为模板构造，只替换 fragment shader 与颜色
     * 附件的格式/写掩码；顶点着色器仍用 {@code core/screenquad}（纯 gl_VertexIndex 全屏三角）。
     * 两条管线不向 vanilla 的 pipeline 表注册，由消费管线的专属编译缓存以内嵌源按需编译。
     */
    static final class Nv12Blit {
        /**
         * 亮度平面管线：全分辨率，只写 R 通道
         */
        static final RenderPipeline LUMA = build("nv12_y", GpuFormat.R8_UNORM, ColorTargetState.WRITE_RED);
        /**
         * 色度平面管线：产出 Cb/Cr（采样几何见 NV12_UV_FRAGMENT 着色器注释）。承载面因后端而异：
         * Vulkan 后端为半分辨率 RG8（R=Cb、G=Cr）；26.1 的 TextureFormat 缺 RG8，以宽翻倍的
         * R8 交错面承载（偶数目标列 Cb、奇数列 Cr），整面拷贝回读恰为 NV12 色度面
         */
        //? if vulkan_backend {
        static final RenderPipeline CHROMA =
                build("nv12_uv", GpuFormat.RG8_UNORM, ColorTargetState.WRITE_RED | ColorTargetState.WRITE_GREEN);

        //? } else {
        /*static final RenderPipeline CHROMA =
                build("nv12_uv", GpuFormat.R8_UNORM, ColorTargetState.WRITE_RED);*/
        //? }
        private Nv12Blit() {
        }

        /**
         * 模板装配见 {@link BlitPipelines#screenquad}，只换 fragment shader 与颜色附件
         */
        private static RenderPipeline build(String name, GpuFormat format, int writeMask) {
            return BlitPipelines.screenquad(name, format, writeMask);
        }
    }

    /**
     * 显存槽位专用的行序翻转 blit 管线；行序契约（为何翻转、翻一次）见
     * {@code NV12_Y_FRAGMENT} 着色器内注释。
     *
     * <p>
     * 照抄 vanilla {@code RenderPipelines.TRACY_BLIT} 的管线描述，只把 fragment 换成模组
     * 自有的翻转采样版（顶点着色器仍用 {@code core/screenquad}）；不向 vanilla 管线表注册，
     * 由消费管线的专属编译缓存以内嵌源按需编译。
     */
    static final class FlipBlit {
        /**
         * 翻转 blit 管线：RGBA8 全通道写
         */
        static final RenderPipeline FLIP = build();

        private FlipBlit() {
        }

        /**
         * 翻转 blit 管线：RGBA8 全通道写
         */
        private static RenderPipeline build() {
            return BlitPipelines.screenquad("blit_flip", GpuFormat.RGBA8_UNORM, ColorTargetState.WRITE_ALL);
        }
    }
}
//? }
