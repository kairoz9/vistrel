package com.kairo.vistrel.util;

import java.net.Inet4Address;
import java.net.Inet6Address;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.net.SocketException;
import java.net.UnknownHostException;
import java.util.Arrays;
import java.util.Collections;
import java.util.Enumeration;

/**
 * 地址展示辅助：地址文本格式化（聊天/状态展示与 URL、SocketAddr 拼装）与浏览器访问地址解析；
 * 只产出展示文本，不建立任何网络连接
 */
public final class Addresses {
    private Addresses() {
    }

    /**
     * 聊天展示用主机地址：IPv6 回环的完整写法 {@code 0:0:0:0:0:0:0:1} 既难读也难复制，
     * 统一收敛为 {@code 127.0.0.1}
     *
     * @param address 待格式化地址
     * @return 主机段展示文本（IPv6 压短零段）
     */
    public static String host(InetAddress address) {
        return shortHost(address);
    }

    /**
     * URL 主机段：IPv6 一律加方括号；地址族不跨族改写（IPv6 回环就是 {@code [::1]}）
     *
     * @param address 待格式化地址
     * @return 可直接拼入 URL 的主机段文本
     */
    public static String urlHost(InetAddress address) {
        return bracket(address);
    }

    /**
     * Rust SocketAddr 文本（{@code host:port}）：与 URL 主机段共用同一括号规则
     *
     * @param address 待格式化地址
     * @param port    端口号
     * @return 形如 {@code host:port}（IPv6 为 {@code [host]:port}）的地址文本
     */
    public static String socketAddress(InetAddress address, int port) {
        return bracket(address) + ":" + port;
    }

    /**
     * 生成浏览器访问地址。绑定全部网卡（通配地址）时取本机一个局域网 IPv4，
     * 取不到回落回环；IPv6 字面量由 {@link #urlHost} 加方括号
     *
     * <p>会发起 DNS 查询并枚举网卡，只可在后台线程调用
     *
     * @param bindAddress 配置的监听地址（主机名或 IP 字面量）
     * @param port        监听端口
     * @return 完整访问 URL；地址解析失败回落 {@code http://127.0.0.1:<port>}
     */
    public static String accessUrl(String bindAddress, int port) {
        try {
            InetAddress bound = InetAddress.getByName(bindAddress);
            InetAddress target = bound.isAnyLocalAddress() ? findLanAddress() : bound;
            return "http://" + urlHost(target) + ":" + port;
        } catch (UnknownHostException e) {
            return "http://127.0.0.1:" + port;
        }
    }

    /**
     * 枚举本机对外 IPv4：优先站点本地（192.168/10/172.16 段），其次任意非回环 IPv4;
     * 全部落空才回到回环
     */
    private static InetAddress findLanAddress() throws UnknownHostException {
        InetAddress fallback = null;
        try {
            Enumeration<NetworkInterface> interfaces = NetworkInterface.getNetworkInterfaces();
            while (interfaces != null && interfaces.hasMoreElements()) {
                NetworkInterface nic = interfaces.nextElement();
                if (!nic.isUp() || nic.isLoopback()) {
                    continue;
                }
                for (InetAddress address : Collections.list(nic.getInetAddresses())) {
                    if (address.isLoopbackAddress() || !(address instanceof Inet4Address)) {
                        continue;
                    }
                    if (address.isSiteLocalAddress()) {
                        return address;
                    }
                    if (fallback == null) {
                        fallback = address;
                    }
                }
            }
        } catch (SocketException e) {
            // 枚举失败不阻塞状态行：回落到回环
        }
        // getLoopbackAddress() 在 IPv6 优先的机器上会给出 ::1，而那时 ::1 上并没有监听
        return fallback != null ? fallback : InetAddress.getByAddress(new byte[] {127, 0, 0, 1});
    }

    /**
     * 主机段文本：IPv4 原样；IPv6 压短零段（JDK 的 getHostAddress() 恒为展开形式，不提供压缩 API）
     */
    private static String shortHost(InetAddress address) {
        return address instanceof Inet6Address v6 ? compressV6(v6.getHostAddress()) : address.getHostAddress();
    }

    /**
     * RFC 5952 §4 的零段压缩：最长连续零组（长度 ≥2，并列取最左的那段）换成 {@code ::}
     */
    private static String compressV6(String text) {
        int scopeStart = text.indexOf('%');
        String scope = scopeStart >= 0 ? text.substring(scopeStart) : "";
        String body = scopeStart >= 0 ? text.substring(0, scopeStart) : text;
        String[] groups = body.split(":", -1);
        if (groups.length != 8) {
            // 非标准 8 组形式（例如内嵌点分四段）不做压缩
            return body + scope;
        }
        int bestStart = -1;
        int bestLen = 1;
        for (int i = 0; i < 8; i++) {
            if (!"0".equals(groups[i])) {
                continue;
            }
            int end = i;
            while (end < 8 && "0".equals(groups[end])) {
                end++;
            }
            if (end - i > bestLen) {
                bestStart = i;
                bestLen = end - i;
            }
            i = end;
        }
        if (bestStart < 0) {
            return body + scope;
        }
        String head = String.join(":", Arrays.copyOfRange(groups, 0, bestStart));
        String tail = String.join(":", Arrays.copyOfRange(groups, bestStart + bestLen, 8));
        return head + "::" + tail + scope;
    }

    /**
     * IPv6 字面量在 URL 与 SocketAddr 中都必须加方括号
     */
    private static String bracket(InetAddress address) {
        return address instanceof Inet6Address ? "[" + shortHost(address) + "]" : address.getHostAddress();
    }
}
