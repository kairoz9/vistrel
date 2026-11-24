package kairo.vistrel.capture;

//? if vulkan_backend {
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import org.lwjgl.system.MemoryStack;
import org.lwjgl.util.vma.Vma;
import org.lwjgl.util.vma.VmaAllocationCreateInfo;
import org.lwjgl.vulkan.VkImageCreateInfo;

/**
 * VRAM 导出作用域：标签匹配、嵌套还原与一次 markExportable 的改写结果。
 */
class VramExportScopeTest {
    /**
     * 作用域外不匹配；作用域内只接受本导出集前缀，null 与无关标签都不改参数。
     */
    @Test
    void testLabelMatchingFollowsScope() {
        String slot0 = VramExportScope.slotLabel(0);
        Assertions.assertFalse(VramExportScope.accepts(slot0), "作用域外不匹配");
        try (VramExportScope _ = VramExportScope.open(1L)) {
            Assertions.assertTrue(VramExportScope.accepts(slot0), "槽位标签在作用域内匹配");
            Assertions.assertTrue(VramExportScope.accepts(VramExportScope.slotLabel(2)), "同前缀的其他槽位也匹配");
            Assertions.assertFalse(VramExportScope.accepts("Vistrel Vulkan Dedicated"), "前缀不完整不匹配");
            Assertions.assertFalse(VramExportScope.accepts("other"), "无关标签不匹配");
            Assertions.assertFalse(VramExportScope.accepts(null), "null 标签不匹配");
        }
        Assertions.assertFalse(VramExportScope.accepts(slot0), "关闭后不匹配");
    }

    /**
     * 池尚未建好时不能开作用域。
     */
    @Test
    void testOpenRequiresInitializedPool() {
        Assertions.assertThrows(IllegalStateException.class, () -> VramExportScope.open(0L));
    }

    /**
     * 内层关闭恢复外层，外层关闭后回到无作用域。
     */
    @Test
    void testNestedScopesRestoreInOrder() {
        try (VramExportScope _ = VramExportScope.open(1L)) {
            try (VramExportScope _ = VramExportScope.open(2L)) {
                Assertions.assertTrue(VramExportScope.accepts(VramExportScope.slotLabel(0)), "内层有效");
            }
            Assertions.assertTrue(VramExportScope.accepts(VramExportScope.slotLabel(0)), "内层关闭后外层仍有效");
        }
        Assertions.assertFalse(VramExportScope.accepts(VramExportScope.slotLabel(0)), "外层关闭后失效");
    }

    /**
     * 匹配时一次调用即写入导出 pNext、Dedicated 位与导出池；不匹配时不改动。
     */
    @Test
    void testApplyRewritesOnlyMatchingLabels() {
        try (MemoryStack stack = MemoryStack.stackPush();
                VramExportScope _ = VramExportScope.open(0xCAFEL)
        ) {
            VkImageCreateInfo imageInfo = VkImageCreateInfo.calloc(stack).sType$Default();
            VmaAllocationCreateInfo allocInfo = VmaAllocationCreateInfo.calloc(stack);

            Assertions.assertFalse(VramExportScope.markExportable("other", imageInfo, allocInfo), "无关标签必须被忽略");
            Assertions.assertEquals(0L, imageInfo.pNext(), "未匹配时不得挂 pNext");
            Assertions.assertEquals(0, allocInfo.flags(), "未匹配时不得改 flags");
            Assertions.assertEquals(0L, allocInfo.pool(), "未匹配时不得设池");

            Assertions.assertTrue(
                    VramExportScope.markExportable(VramExportScope.slotLabel(0), imageInfo, allocInfo),
                    "槽位标签应命中"
            );
            Assertions.assertNotEquals(0L, imageInfo.pNext(), "导出信息已挂到 pNext");
            Assertions.assertNotEquals(
                    0,
                    allocInfo.flags() & Vma.VMA_ALLOCATION_CREATE_DEDICATED_MEMORY_BIT,
                    "必须置 Dedicated 位"
            );
            Assertions.assertEquals(0xCAFEL, allocInfo.pool(), "必须使用当前作用域的导出池");
        }
    }
}
//? } else {
/*class VramExportScopeTest {
}*/
//? }
