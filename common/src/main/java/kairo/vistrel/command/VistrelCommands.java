package kairo.vistrel.command;

import kairo.vistrel.VistrelService;
import kairo.vistrel.config.VistrelConfig;
import kairo.vistrel.streaming.EncoderCaps;
import kairo.vistrel.streaming.EncoderChannel;
import kairo.vistrel.streaming.SessionServer;
import com.mojang.brigadier.Command;
import com.mojang.brigadier.arguments.StringArgumentType;
import com.mojang.brigadier.builder.LiteralArgumentBuilder;
import com.mojang.brigadier.builder.RequiredArgumentBuilder;
import net.minecraft.ChatFormatting;
import net.minecraft.client.Minecraft;
import net.minecraft.network.chat.ClickEvent;
import net.minecraft.network.chat.Component;
import net.minecraft.network.chat.HoverEvent;
import net.minecraft.network.chat.MutableComponent;
import net.minecraft.network.chat.Style;
import java.util.ArrayList;
import java.util.List;

/**
 * 游戏内服务控制命令树 /vistrel
 *
 * <p>Brigadier 命令树用类型参数 {@code <S>} 适配各 loader 的客户端命令源
 * （Fabric {@code FabricClientCommandSource}、Forge/NeoForge {@code CommandSourceStack}），
 * 树体本身平台无关，三个 loader 只负责注册。
 *
 * <p>启停动作经 {@link VistrelService#submitControl} 派发到控制面串行队列，
 * 结果经 {@code Minecraft#execute} 回到渲染主线程发聊天消息，不阻塞渲染线程。
 */
public final class VistrelCommands {
    private static final String PREFIX = "§6[Vistrel]§r ";

    private VistrelCommands() {
    }

    /**
     * 构建 /vistrel 命令树根节点；由各 loader 客户端命令注册点以自身命令源类型调用
     *
     * @param <S> Brigadier 命令源类型（各 loader 的客户端命令源）
     * @return 命令树根节点，含 start / stop / status / password set 四个子命令
     */
    public static <S> LiteralArgumentBuilder<S> tree() {
        // 密码子树先以显式类型参数落定，S 才能在泛型工厂调用点绑定到本方法的 S
        RequiredArgumentBuilder<S, String> setPassword = RequiredArgumentBuilder
            .<S, String>argument("password", StringArgumentType.greedyString())
            .executes(ctx -> {
                String raw = StringArgumentType.getString(ctx, "password");
                String error = validatePassword(raw);
                if (error != null) {
                    feedback("§c" + error);
                    return 0;
                }
                submit(() -> feedback(applyPasswordChange(raw)));
                return Command.SINGLE_SUCCESS;
            });

        return LiteralArgumentBuilder
            .<S>literal("vistrel")
            .then(LiteralArgumentBuilder
                .<S>literal("start")
                .executes(ctx -> {
                    submit(() -> {
                        VistrelService.start();
                        if (VistrelService.isRunning()) {
                            // 启动后直接展示与 /vistrel status 同一套状态块，避免两处样式漂移
                            feedback(statusMessage());
                        } else {
                            feedback("§c服务启动失败，详见日志");
                        }
                    });
                    return Command.SINGLE_SUCCESS;
                }))
            .then(LiteralArgumentBuilder
                .<S>literal("stop")
                .executes(ctx -> {
                    submit(() -> {
                        VistrelService.stop();
                        feedback(VistrelService.isRunning() ? "§c服务停止失败，详见日志" : "§e服务已停止");
                    });
                    return Command.SINGLE_SUCCESS;
                }))
            .then(LiteralArgumentBuilder
                .<S>literal("status")
                .executes(ctx -> {
                    feedback(statusMessage());
                    return Command.SINGLE_SUCCESS;
                }))
            .then(LiteralArgumentBuilder
                .<S>literal("password")
                .then(LiteralArgumentBuilder.<S>literal("set").then(setPassword))
            );
    }

    /**
     * 密码规则：trim 后非空且长度落在 {@link VistrelConfig} 的允许区间内，其余不设约束；
     * 返回 {@code null} 表示合法，否则为错误提示
     *
     * @param raw 候选密码原文（未 trim）
     */
    public static String validatePassword(String raw) {
        String trimmed = raw == null ? "" : raw.trim();
        if (trimmed.isEmpty()) {
            return "密码不能为空";
        }
        if (!VistrelConfig.isPasswordLengthValid(trimmed)) {
            return "密码长度须在 " + VistrelConfig.MIN_PASSWORD_LENGTH + "~" + VistrelConfig.MAX_PASSWORD_LENGTH + " 位";
        }
        return null;
    }

    /**
     * 应用密码变更：写入配置并即时生效；服务停止时只改配置，下次启动生效。
     * 前提：{@code raw} 已通过 {@link #validatePassword} 校验，trim 只在这里做一次。
     * 返回成功文案
     */
    static String applyPasswordChange(String raw) {
        String newPassword = raw.trim();
        VistrelConfig.update(VistrelConfig.current().withPassword(newPassword));
        SessionServer server = VistrelService.getServer();
        if (server != null) {
            server.setPassword(newPassword);
        }
        return "§a密码已更新: §b" + newPassword;
    }

    /**
     * 生成当前服务状态行；除一次在位编码器查询（纯内存读、不等待、不碰设备）外
     * 全是 Java 内存读取，渲染主线程直接调用不会卡顿
     *
     * @return 状态行列表，至少一行
     */
    public static List<Component> statusLines() {
        VistrelConfig config = VistrelConfig.current();
        EncoderChannel channel = VistrelService.getChannel();
        boolean running = VistrelService.isRunning();
        List<Component> lines = new ArrayList<>();
        lines.add(Component.literal("§6§l▍ §r" + (running ? "§a● §f运行中" : "§c● §f已停止")));
        if (!running) {
            lines.add(row("执行命令", Component.literal("§f/vistrel start §8启动服务")));
            return lines;
        }
        lines.add(row("访问地址", link(VistrelService.getAccessUrl())));
        lines.add(
                row(
                        "访问密码",
                        config.password().isEmpty() ? Component.literal("§e未启用 §8(免认证)") : maskedPassword(config.password()
                        )
                )
        );
        appendEncoderLines(lines, channel);
        return lines;
    }

    /**
     * 把状态行拼成一条多行消息：行间用换行分隔，各行样式与点击/悬停事件互斥
     *
     * @return 多行状态消息组件
     */
    public static Component statusMessage() {
        MutableComponent body = Component.empty();
        List<Component> lines = statusLines();
        for (int i = 0; i < lines.size(); i++) {
            if (i > 0) {
                body.append(Component.literal("\n"));
            }
            body.append(lines.get(i));
        }
        return body;
    }

    /**
     * 状态行布局：统一两字标签 + 灰点分隔，各行的值起始列因此对齐
     */
    private static Component row(String label, Component value) {
        return Component.literal("§7" + label + ": ").append(value);
    }

    /**
     * 可点击地址
     */
    private static Component link(String url) {
        return Component
            .literal(url)
            .withStyle(Style.EMPTY
                .withColor(ChatFormatting.AQUA)
                .withUnderlined(true)
                .withClickEvent(openUrl(url))
                .withHoverEvent(showText(Component.literal("§7点击在浏览器中打开")))
            );
    }

    /**
     * 打码密码
     */
    private static Component maskedPassword(String password) {
        return Component
            .literal("****************")
            .withStyle(Style.EMPTY
                .withColor(ChatFormatting.LIGHT_PURPLE)
                .withObfuscated(true)
                .withClickEvent(copyText(password))
                .withHoverEvent(showText(Component.literal("§7点击复制密码到剪贴板")))
            );
    }

    //? if >=26.1 {
    private static ClickEvent openUrl(String url) {
        return new ClickEvent.OpenUrl(java.net.URI.create(url));
    }

    private static ClickEvent copyText(String text) {
        return new ClickEvent.CopyToClipboard(text);
    }

    private static HoverEvent showText(Component text) {
        return new HoverEvent.ShowText(text);
    }

    //? } else {
    /*private static ClickEvent openUrl(String url) {
        return (ClickEvent) richEvent(ClickEvent.class, "OPEN_URL", url, java.net.URI.create(url));
    }

    private static ClickEvent copyText(String text) {
        return (ClickEvent) richEvent(ClickEvent.class, "COPY_TO_CLIPBOARD", text);
    }

    private static HoverEvent showText(Component text) {
        return (HoverEvent) richEvent(HoverEvent.class, "SHOW_TEXT", text);
    }

    // <26.1 没有 record 子类型：富文本实例只能经 (Action, 值) 通用构造器创建，1.21.5 起才改为 record 子类型。
    // 构造路径按运行期形态解析：枚举常量按 name() 选取（枚举名字面量在三 loader 原样保留）；
    // 旧 HoverEvent.Action 是普通类而非枚举（1.20/1.21），其常量经
    // “零参返回 String 且等于序列化名”的访问器定位
    private static Object richEvent(Class<?> family, String actionName, Object value) {
        return richEvent(family, actionName, value, value);
    }

    private static Object richEvent(Class<?> family, String actionName, Object genericValue, Object recordValue) {
        for (java.lang.reflect.Constructor<?> constructor : family.getDeclaredConstructors()) {
            Class<?>[] parameters = constructor.getParameterTypes();
            if (parameters.length == 2 && parameters[0].isMemberClass() && parameters[1].isInstance(genericValue)) {
                // 通用构造器直接收目标常量，实例必然携带它，无需再经访问器比对
                try {
                    return constructor.newInstance(actionConstant(parameters[0], actionName), genericValue);
                } catch (ReflectiveOperationException | IllegalArgumentException e) {
                    throw new IllegalStateException(family.getName() + " 构造失败", e);
                }
            }
        }
        Class<?> actionType = actionEnum(family);
        Object action = actionConstant(actionType, actionName);
        for (Class<?> nested : family.getDeclaredClasses()) {
            if (!nested.isRecord()) {
                continue;
            }
            for (java.lang.reflect.Constructor<?> constructor : nested.getDeclaredConstructors()) {
                Class<?>[] parameters = constructor.getParameterTypes();
                if (parameters.length != 1 || !parameters[0].isInstance(recordValue)) {
                    continue;
                }
                Object candidate;
                try {
                    candidate = constructor.newInstance(recordValue);
                } catch (ReflectiveOperationException | RuntimeException e) {
                    continue;
                }
                if (actionOf(family, actionType, candidate) == action) {
                    return candidate;
                }
            }
        }
        throw new IllegalStateException(family.getName() + " 未找到可构造 " + actionName + " 的子类型");
    }

    private static Class<?> actionEnum(Class<?> family) {
        for (Class<?> nested : family.getDeclaredClasses()) {
            if (nested.isEnum()) {
                return nested;
            }
        }
        throw new IllegalStateException(family.getName() + " 缺少 Action 枚举");
    }

    private static Object actionConstant(Class<?> actionType, String name) {
        if (actionType.isEnum()) {
            for (Object constant : actionType.getEnumConstants()) {
                if (((Enum<?>) constant).name().equals(name)) {
                    return constant;
                }
            }
            throw new IllegalStateException(actionType.getName() + " 缺少常量 " + name);
        }
        String expected = name.toLowerCase(java.util.Locale.ROOT);
        for (java.lang.reflect.Field field : actionType.getDeclaredFields()) {
            if (!java.lang.reflect.Modifier.isStatic(field.getModifiers())
                    || !actionType.isAssignableFrom(field.getType())) {
                continue;
            }
            Object constant;
            try {
                constant = field.get(null);
            } catch (ReflectiveOperationException e) {
                continue;
            }
            if (constant != null && hasSerializedName(actionType, constant, expected)) {
                return constant;
            }
        }
        throw new IllegalStateException(actionType.getName() + " 缺少常量 " + name);
    }

    private static boolean hasSerializedName(Class<?> actionType, Object constant, String expected) {
        for (java.lang.reflect.Method method : actionType.getDeclaredMethods()) {
            if (java.lang.reflect.Modifier.isStatic(method.getModifiers()) || method.getParameterCount() != 0
                    || method.getReturnType() != String.class) {
                continue;
            }
            try {
                if (expected.equals(method.invoke(constant))) {
                    return true;
                }
            } catch (ReflectiveOperationException | RuntimeException e) {
                continue;
            }
        }
        return false;
    }

    private static Object actionOf(Class<?> family, Class<?> actionType, Object instance) {
        for (java.lang.reflect.Method method : family.getMethods()) {
            if (method.getParameterCount() == 0 && method.getReturnType() == actionType) {
                try {
                    return method.invoke(instance);
                } catch (ReflectiveOperationException e) {
                    throw new IllegalStateException(family.getName() + " Action 访问器调用失败", e);
                }
            }
        }
        throw new IllegalStateException(family.getName() + " 缺少 Action 访问器");
    }*/
    //? }
    /**
     * 读取会话实际建成的编码后端，从本机能力快照里查它的 codec 与显卡名
     */
    private static void appendEncoderLines(List<Component> lines, EncoderChannel channel) {
        SessionServer.SessionState state = VistrelService.getSessionState();
        String backendToken = state == null ? null : state.generation().backend();
        if (backendToken == null) {
            lines.add(row("编码信息", Component.literal("§8无 §7(无活跃会话)")));
            return;
        }
        EncoderCaps.Backend backend = channel == null ? null : channel.caps().find(backendToken);
        if (backend == null) {
            lines.add(row("编码信息", Component.literal("§8未知 §7(" + backendToken + ")")));
            return;
        }
        String codecs = String.join(" §8· §f", backend.codecLabels());
        lines.add(row("编码信息", Component.literal("§f" + backend.label() + " §8· §f" + codecs)));
        if (!backend.gpu().isEmpty()) {
            lines.add(row("使用显卡", Component.literal("§f" + backend.gpu())));
        }
    }

    /**
     * 派发动作至服务控制面串行队列
     */
    private static void submit(Runnable action) {
        VistrelService.submitControl(action);
    }

    /**
     * 经主线程队列投递聊天反馈；任意线程调用均安全
     *
     * @param message 纯文本消息（自动加 Vistrel 前缀）
     */
    public static void feedback(String message) {
        feedback(Component.literal(message));
    }

    /**
     * 组件版反馈：需要点击事件等富文本时使用；任意线程调用均安全
     *
     * @param message 组件消息（自动加 Vistrel 前缀）；无游戏实例或无玩家时静默丢弃
     */
    public static void feedback(Component message) {
        Minecraft minecraft = Minecraft.getInstance();
        // 无游戏实例（纯 JVM 测试/无头上下文）时无处投递，静默跳过
        if (minecraft == null) {
            return;
        }
        minecraft.execute(() -> {
            Component chatMessage = Component.literal(PREFIX).append(message);
            //? if >=26.1 {
            if (minecraft.player != null) {
                minecraft.player.sendSystemMessage(chatMessage);
            }
            //? } else {
            /*// 状态回执直写聊天组件：不过玩家系统消息管线——那条管线先过 onClientSystemChat 事件
            //（可被其他 mod 改写或取消，回执必须保达），并附带举报日志推送与旁白副作用；
            //系统消息不走延迟队列。26.1 起 addMessage 收私有，26.x 只能走 sendSystemMessage
            minecraft.gui.getChat().addMessage(chatMessage);*/
            //? }
        });
    }
}
