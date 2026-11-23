package kairo.vistrel.util;

import java.net.InetAddress;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * IPv6 文本压缩（RFC 5952 §4）与各出口的括号规则
 */
class UtilsTest {
    private static InetAddress addr(String literal) throws Exception {
        return InetAddress.getByName(literal);
    }

    @Test
    void testLoopbackIsCompressedButNeverRewrittenToIpv4() throws Exception {
        InetAddress loopback = addr("0:0:0:0:0:0:0:1");
        Assertions.assertEquals("::1", Addresses.host(loopback), "7 段零组必须压成 ::1");
        Assertions.assertEquals("[::1]", Addresses.urlHost(loopback), "IPv6 回环仍属 IPv6，不得写成 127.0.0.1");
        Assertions.assertEquals("[::1]:30808", Addresses.socketAddress(loopback, 30808));
    }

    @Test
    void testLongestZeroRunIsCompressedAndLeftmostWinsOnTie() throws Exception {
        Assertions.assertEquals("2001:db8::1", Addresses.host(addr("2001:db8:0:0:0:0:0:1")));
        Assertions.assertEquals("fe80::1", Addresses.host(addr("fe80:0:0:0:0:0:0:1")));
        // 两段等长零组并列时取最左
        Assertions.assertEquals("2001:db8::1:0:0:1", Addresses.host(addr("2001:db8:0:0:1:0:0:1")));
    }

    @Test
    void testSingleZeroGroupAndAllZeroAddress() throws Exception {
        // 单个零组不得压缩成 “::”
        Assertions.assertEquals("2001:db8:0:1:1:1:1:1", Addresses.host(addr("2001:db8:0:1:1:1:1:1")));
        Assertions.assertEquals("::", Addresses.host(addr("::")));
        Assertions.assertEquals("[::]", Addresses.urlHost(addr("::")));
    }

    @Test
    void testIpv4AndSocketAddressStayPlain() throws Exception {
        InetAddress v4 = addr("192.168.1.20");
        Assertions.assertEquals("192.168.1.20", Addresses.host(v4));
        Assertions.assertEquals("192.168.1.20", Addresses.urlHost(v4), "IPv4 不加方括号");
        Assertions.assertEquals("192.168.1.20:30808", Addresses.socketAddress(v4, 30808));
        Assertions.assertEquals("[2001:db8::1]:30808", Addresses.socketAddress(addr("2001:db8::1"), 30808));
    }
}
