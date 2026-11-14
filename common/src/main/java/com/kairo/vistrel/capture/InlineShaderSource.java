package com.kairo.vistrel.capture;

//? if renderpearl_era {
import com.mojang.renderpearl.api.pipeline.ShaderSource;
import com.mojang.renderpearl.api.pipeline.ShaderType;
import net.minecraft.resources.Identifier;
import net.minecraft.server.packs.resources.ResourceManager;
import java.io.IOException;
import java.io.InputStream;
import java.io.UncheckedIOException;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Map;

/**
 * 内嵌着色器源：vistrel 自有 GLSL 走字符串常量，minecraft:core/screenquad 顶点着色器委托
 * 资源管理器读 vanilla 原件防跨版本漂移。经设备源码注入接口编译，不依赖资源重载后的
 * 管线缓存，启动窗口期即可用
 */
final class InlineShaderSource implements ShaderSource {
    private final ResourceManager resources;
    private final Map<String, String> vanillaCache = new HashMap<>();

    InlineShaderSource(ResourceManager resources) {
        this.resources = resources;
    }

    @Override
    public String getShader(Identifier id, ShaderType type) {
        return load(id, type);
    }

    @Override
    public ShaderSource.CachedIncludeSource getInclude(Identifier id) {
        // 自有着色器与 vanilla screenquad 均无 #include，include 回调恒拒答
        return ShaderSource.CachedIncludeSource.createError("内嵌源不支持 include: " + id);
    }

    @Override
    public void close() {
        // 纯 Java 持有，无原生资源
    }

    private String load(Identifier id, ShaderType type) {
        if ("vistrel".equals(id.getNamespace())) {
            return BlitPipelines.inlineFragment(id.getPath());
        }
        // 缓存键带类型：同 id 的 vsh 与 fsh 是两份不同内容
        return vanillaCache.computeIfAbsent(id + "@" + type, key -> readVanilla(id, type));
    }

    // 管线声明的是裸 id，磁盘资源带 shaders/ 前缀与 vsh/fsh 后缀
    private String readVanilla(Identifier id, ShaderType type) {
        Identifier path =
                Identifier.fromNamespaceAndPath(id.getNamespace(), "shaders/" + id.getPath() + shaderExtension(type));
        try (InputStream in = resources.open(path)) {
            return new String(in.readAllBytes(), StandardCharsets.UTF_8);
        } catch (IOException e) {
            throw new UncheckedIOException("读取 vanilla 着色器失败: " + id, e);
        }
    }

    private static String shaderExtension(ShaderType type) {
        return switch (type) {
            case VERTEX -> ".vsh";
            case FRAGMENT -> ".fsh";
        };
    }
}
//? } elif gpu_device {
/*import com.mojang.blaze3d.shaders.ShaderSource;
import com.mojang.blaze3d.shaders.ShaderType;
import net.minecraft.resources.Identifier;
import net.minecraft.server.packs.resources.ResourceManager;
import java.io.IOException;
import java.io.InputStream;
import java.io.UncheckedIOException;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Map;

// 内嵌着色器源（26.1/26.2 形态）：语义同 26.3 变体，接口为上一代 blaze3d ShaderSource，
// gpu_device 门承载（1.21.x 视图下 BlitPipelines 不存在，勿放宽为 !renderpearl_era）
final class InlineShaderSource implements ShaderSource {
    private final ResourceManager resources;
    private final Map<String, String> vanillaCache = new HashMap<>();

    InlineShaderSource(ResourceManager resources) {
        this.resources = resources;
    }

    @Override
    public String get(Identifier id, ShaderType type) {
        return load(id, type);
    }

    private String load(Identifier id, ShaderType type) {
        if ("vistrel".equals(id.getNamespace())) {
            return BlitPipelines.inlineFragment(id.getPath());
        }
        // 缓存键带类型：同 id 的 vsh 与 fsh 是两份不同内容
        return vanillaCache.computeIfAbsent(id + "@" + type, key -> readVanilla(id, type));
    }

    // 管线声明的是裸 id，磁盘资源带 shaders/ 前缀与 vsh/fsh 后缀
    private String readVanilla(Identifier id, ShaderType type) {
        Identifier path = Identifier.fromNamespaceAndPath(id.getNamespace(), "shaders/" + id.getPath() + shaderExtension(type));
        try (InputStream in = resources.open(path)) {
            return new String(in.readAllBytes(), StandardCharsets.UTF_8);
        } catch (IOException e) {
            throw new UncheckedIOException("读取 vanilla 着色器失败: " + id, e);
        }
    }

    private static String shaderExtension(ShaderType type) {
        return switch (type) {
            case VERTEX -> ".vsh";
            case FRAGMENT -> ".fsh";
        };
    }
}*/
//? }
