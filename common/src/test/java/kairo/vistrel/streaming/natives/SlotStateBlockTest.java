package kairo.vistrel.streaming.natives;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import kairo.vistrel.streaming.VramBinding;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * 槽位所有权协议用例：位出错的最坏结果只是覆盖自己邮箱里未取走的帧
 * （最新帧优先允许，不构成跨 API 竞态）；停滞判定在每次采集 tick 执行。
 */
class SlotStateBlockTest {
    private static final long TIMEOUT = VramBinding.STALL_HEARTBEAT_TIMEOUT_NS;

    /**
     * 64 字节直接块替身：busy 位在 [0..32)，心跳在 [32..40)，停滞阶段码在 [40..48)，
     * 显存绑定世代在 [48..56)。
     */
    private static ByteBuffer newBlock() {
        return ByteBuffer.allocateDirect(NativeWireContract.SLOT_STATE_BLOCK_BYTES).order(ByteOrder.nativeOrder());
    }

    @Test
    void testClaimMarksBusyAndPrefersRotation() {
        ByteBuffer block = newBlock();
        SlotStateBlock ownership = new SlotStateBlock(block, 3);
        Assertions.assertEquals(1, ownership.claim(1), "轮转首选槽位空闲时直接认领");
        Assertions.assertTrue(isBusy(block, 1), "认领即占位，编码器归还前不得复写");
        // 首选忙时降级到索引序第一个空闲槽位
        Assertions.assertEquals(0, ownership.claim(1), "首选忙时按索引序降级");
        Assertions.assertTrue(isBusy(block, 0));
    }

    @Test
    void testClaimAllBusyReturnsMinusOneMeaningDropped() {
        SlotStateBlock ownership = new SlotStateBlock(newBlock(), 3);
        Assertions.assertNotEquals(-1, ownership.claim(0));
        Assertions.assertNotEquals(-1, ownership.claim(1));
        Assertions.assertNotEquals(-1, ownership.claim(2));
        Assertions.assertEquals(-1, ownership.claim(0), "全忙拒绝认领：调用方据此走主动丢帧 (Dropped)");
    }

    @Test
    void testReleaseMakesSlotReclaimable() {
        ByteBuffer block = newBlock();
        SlotStateBlock ownership = new SlotStateBlock(block, 3);
        Assertions.assertNotEquals(-1, ownership.claim(0));
        ownership.release(0);
        Assertions.assertFalse(isBusy(block, 0), "归还后立即可再认领");
        Assertions.assertEquals(0, ownership.claim(0), "归还位必须能被重新认领");
    }

    @Test
    void testClaimOnlyTouchesConfiguredSlotCount() {
        ByteBuffer block = newBlock();
        SlotStateBlock ownership = new SlotStateBlock(block, 3);
        int claimed = ownership.claim(3);
        Assertions.assertTrue(claimed >= 0 && claimed < 3, "非法轮转索引退化为索引序扫描，认领结果必须落在 slotCount 内");
        Assertions.assertEquals(1, ownership.claim(-1), "负轮转索引同样退化，从索引序补位（0 已被上一手认领）");
        Assertions.assertFalse(isBusy(block, 5), "掩码容量 8 位，未配置的高位不得被触碰");
    }

    @Test
    void testHeartbeatTimeoutRequiresFrozenHeartbeatSinceBaseline() {
        ByteBuffer block = newBlock();
        SlotStateBlock ownership = new SlotStateBlock(block, 3);
        long now = 10_000_000_000L;
        writeHeartbeat(block, 41L);
        ownership.markPushed(now - TIMEOUT);
        Assertions.assertTrue(ownership.heartbeatTimedOut(now), "心跳停在基准值且超过时限：判定编码线程卡死");
        writeHeartbeat(block, 42L);
        Assertions.assertFalse(ownership.heartbeatTimedOut(now), "心跳仍在推进（Actor 健康或在处理丢弃帧）：不触发");
        writeHeartbeat(block, 41L);
        ownership.markPushed(now - TIMEOUT + 1);
        Assertions.assertFalse(ownership.heartbeatTimedOut(now), "未达时限不触发");
    }

    @Test
    void testConstructorBaselinesHeartbeatBeforeFirstPush() {
        ByteBuffer block = newBlock();
        SlotStateBlock ownership = new SlotStateBlock(block, 3);
        // 心跳从未推进（Actor 卡在第一次循环前）且时长已过：构造时记的基准必须能触发
        Assertions.assertTrue(ownership.heartbeatTimedOut(Long.MAX_VALUE), "Actor 一次迭代都没有且超时，必须判定卡死");
        // 刚构造完：基准记在构造时刻，时限未满，不得误触发
        Assertions.assertFalse(ownership.heartbeatTimedOut(System.nanoTime()));
    }

    @Test
    void testConcurrentClaimElectsSingleWinnerPerRound() throws Exception {
        SlotStateBlock ownership = new SlotStateBlock(newBlock(), 1);
        int threads = 4;
        ExecutorService pool = Executors.newFixedThreadPool(threads);
        try {
            for (int round = 0; round < 2_000; round++) {
                List<Future<Integer>> attempts = new ArrayList<>();
                for (int t = 0; t < threads; t++) {
                    attempts.add(pool.submit(() -> ownership.claim(0)));
                }
                int winners = 0;
                for (Future<Integer> attempt : attempts) {
                    if (attempt.get() == 0) {
                        winners++;
                    }
                }
                Assertions.assertEquals(1, winners, "单槽位一轮认领恰有一个胜者，CAS 不会双占");
                ownership.release(0);
            }
        } finally {
            pool.shutdownNow();
        }
    }

    @Test
    void testStageNameReadsCrossLanguageContract() {
        ByteBuffer block = newBlock();
        SlotStateBlock ownership = new SlotStateBlock(block, 3);
        Assertions.assertEquals("空闲等待", ownership.stageName(), "零值必须是空闲阶段");
        block.putLong(NativeWireContract.STAGE_OFFSET, 9L);
        Assertions.assertEquals("锁定码流", ownership.stageName(), "阶段码 9 与 Rust ActorStage::LockBitstream 对应");
        block.putLong(NativeWireContract.STAGE_OFFSET, 99L);
        Assertions.assertEquals("未知阶段(99)", ownership.stageName(), "未约定的码按原值展示，不得越界");
    }

    @Test
    void testBindingEpochReadsCrossLanguageOffset() {
        ByteBuffer block = newBlock();
        SlotStateBlock ownership = new SlotStateBlock(block, 3);
        Assertions.assertEquals(0L, ownership.bindingEpoch(), "初始绑定世代为 0：首个编码器尚未重建");
        block.putLong(NativeWireContract.BINDING_EPOCH_OFFSET, 7L);
        Assertions.assertEquals(7L, ownership.bindingEpoch(), "包装视图必须按共享块偏移读取绑定世代");
        Assertions.assertEquals(56, NativeWireContract.RESERVED_OFFSET, "世代段之后才是保留段");
    }

    private static void writeHeartbeat(ByteBuffer block, long value) {
        block.putLong(NativeWireContract.HEARTBEAT_OFFSET, value);
    }

    private static boolean isBusy(ByteBuffer block, int slotIdx) {
        return block.getInt(NativeWireContract.BUSY_OFFSET + slotIdx * Integer.BYTES) != 0;
    }
}
