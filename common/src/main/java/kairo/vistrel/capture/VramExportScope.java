package kairo.vistrel.capture;

//? if vulkan_backend {
import kairo.vistrel.Constants;
import org.lwjgl.system.MemoryStack;
import org.lwjgl.util.vma.Vma;
import org.lwjgl.util.vma.VmaAllocationCreateInfo;
import org.lwjgl.vulkan.VK11;
import org.lwjgl.vulkan.VkExternalMemoryImageCreateInfo;
import org.lwjgl.vulkan.VkImageCreateInfo;

/**
 * 渲染线程上的一次 VRAM 导出作用域：作用域内创建的槽位纹理自动挂上外部显存导出标记，
 * 并落进同一个 VMA 导出池。
 *
 * <p>作用域按线程栈嵌套：{@link #open} 压栈，{@link #close} 弹栈。标签格式、平台句柄类型
 * 与图像改写的规则都在本类，Mixin 只调一次 {@link #markExportable}，不再自己判断作用域或标签。
 *
 * <p>线程归属：open / close / markExportable 都只在渲染线程调用。
 */
public final class VramExportScope implements AutoCloseable {
    /**
     * 槽位纹理标签前缀：纹理创建时靠它识别本导出集的槽位；即使作用域泄漏到无关的
     * 创建路径，也不会波及别的纹理分配。
     */
    private static final String LABEL_PREFIX = "Vistrel Vulkan Dedicated Frame ";
    private static final ThreadLocal<VramExportScope> CURRENT = new ThreadLocal<>();
    private final long exportPool;
    private final VramExportScope previous;
    private boolean closed = false;

    private VramExportScope(long exportPool, VramExportScope previous) {
        this.exportPool = exportPool;
        this.previous = previous;
    }

    /**
     * 打开当前线程的导出作用域。
     *
     * @param exportPool 已创建的 VMA 导出池，必须非 0
     * @return 作用域句柄，必须关闭
     * @throws IllegalStateException exportPool 为 0（池尚未建好）
     */
    public static VramExportScope open(long exportPool) {
        if (exportPool == 0L) {
            throw new IllegalStateException("Cannot open export scope: exportPool is not initialized");
        }
        VramExportScope scope = new VramExportScope(exportPool, CURRENT.get());
        CURRENT.set(scope);
        return scope;
    }

    /**
     * 槽位纹理标签。
     *
     * @param slotIdx 槽位索引
     * @return 用于创建槽位纹理的标签
     */
    static String slotLabel(int slotIdx) {
        return LABEL_PREFIX + slotIdx;
    }

    /**
     * 当前线程是否处于导出作用域，且 label 属于本导出集。
     *
     * <p>包内可见：生产路径只走 {@link #markExportable}，本方法供测试与同包诊断使用。
     *
     * @param label 纹理标签；{@code null} 视为不匹配
     * @return true 表示该纹理应导入导出池
     */
    static boolean accepts(String label) {
        return CURRENT.get() != null && label != null && label.startsWith(LABEL_PREFIX);
    }

    /**
     * 把这张纹理标记为可导出：匹配、pNext 改写与导出池设置一次完成。
     *
     * <p>不匹配时原样返回 false，不改动任何参数。挂到 pNext 的导出信息取自当前线程的
     * LWJGL 栈帧，必须在 {@code vmaCreateImage} 返回前保持有效。
     *
     * @param label     纹理标签
     * @param imageInfo 待创建的图像描述
     * @param allocInfo 本次分配描述
     * @return true 表示已写入导出标记与导出池
     */
    public static boolean markExportable(String label, VkImageCreateInfo imageInfo, VmaAllocationCreateInfo allocInfo) {
        if (!accepts(label)) {
            return false;
        }
        VramExportScope scope = CURRENT.get();
        VkExternalMemoryImageCreateInfo exportInfo = VkExternalMemoryImageCreateInfo
            .calloc(MemoryStack.stackGet())
            .sType$Default()
            .handleTypes(memoryHandleType());
        exportInfo.pNext(imageInfo.pNext());
        imageInfo.pNext(exportInfo.address());

        allocInfo.flags(allocInfo.flags() | Vma.VMA_ALLOCATION_CREATE_DEDICATED_MEMORY_BIT);
        allocInfo.pool(scope.exportPool);
        return true;
    }

    /**
     * 当前平台外部显存导出句柄类型 (Linux: OPAQUE_FD, Windows: OPAQUE_WIN32)
     *
     * @return {@code VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_*} 常量值
     */
    static int memoryHandleType() {
        return Constants.IS_WINDOWS
                ? VK11.VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT
                : VK11.VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;
    }

    /**
     * 当前平台外部时间线信号量导出句柄类型 (Linux: OPAQUE_FD, Windows: OPAQUE_WIN32)
     *
     * @return {@code VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_*} 常量值
     */
    static int semaphoreHandleType() {
        return Constants.IS_WINDOWS
                ? VK11.VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT
                : VK11.VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT;
    }

    /**
     * 关闭作用域并恢复上一层；重复关闭无副作用。
     */
    @Override
    public void close() {
        if (closed) {
            return;
        }
        closed = true;
        if (previous != null) {
            CURRENT.set(previous);
        } else {
            CURRENT.remove();
        }
    }
}
//? } else {
/*public final class VramExportScope implements AutoCloseable {
    @Override
    public void close() {
    }
}*/
//? }
