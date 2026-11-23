package kairo.vistrel.streaming.natives;

import kairo.vistrel.Constants;
import kairo.vistrel.config.VistrelConfig;
import kairo.vistrel.platform.Services;
import java.io.IOException;
import java.io.InputStream;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.security.MessageDigest;
import java.time.Duration;
import java.util.HexFormat;
import java.util.Optional;

/**
 * Cisco OpenH264 运行期获取器
 *
 * <p>
 * jar 不捆绑 openh264 二进制：游戏侧首次需要软编时从 Cisco 官方源下载预编译归档
 * （.bz2）到加载器配置目录下的 {@code vistrel/natives/}，解压、SHA-256 白名单校验与加载都由
 * Rust 侧 {@code software::init_library} 统一处理。缓存跨重启保留，命中后不再
 * 请求 ciscobinary。
 */
public final class OpenH264Loader {
    private static final String CISCO_BASE_URL = "https://ciscobinary.openh264.org/";
    /**
     * Cisco 官方归档名（远程文件名带完整版本号），按平台取用
     */
    private static final String LINUX_ARCHIVE = "libopenh264-2.6.0-linux64.8.so.bz2";
    private static final String WINDOWS_ARCHIVE = "openh264-2.6.0-win64.dll.bz2";
    /**
     * 解压产物的 SHA-256（与 openh264-sys2 白名单同值），用于缓存文件自愈预校验
     */
    private static final String LINUX_SHA256 = "2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04";
    private static final String WINDOWS_SHA256 = "2076cb5675ec6c1a4c70e7a2a322552f547b6eeed649d6dfcd9e02a543b24691";
    private static final Duration CONNECT_TIMEOUT = Duration.ofSeconds(10);
    private static final Duration REQUEST_TIMEOUT = Duration.ofSeconds(30);

    private OpenH264Loader() {
    }

    /**
     * 确保软编库就绪，返回传给 {@code startEngine} 的路径。三种结果：
     * 缓存命中返回库路径 / 刚下载完返回 .bz2 归档路径（由 Rust 解压安装）/ 空串
     * （已禁用或下载失败，Rust 会自行降级，硬件编码不受影响）。
     *
     * <p>
     * 阻塞式：首启未命中时耗时即下载耗时（约 1-2MB 归档），调用方位于后台线程；
     * 缓存命中瞬时返回。
     */
    public static String ensureLibrary() {
        Path nativesDir = Services.PLATFORM.getConfigDir().resolve(Constants.MOD_ID).resolve("natives");
        return ensureLibrary(nativesDir, VistrelConfig.current().softwareEncoder());
    }

    /**
     * 核心逻辑（缓存目录与开关参数化，供测试定向注入）
     */
    static String ensureLibrary(Path cacheDir, boolean enabled) {
        String libName = libName();
        if (libName == null) {
            Constants.LOG.warn("当前平台不受支持，软件编码不可用");
            return "";
        }
        if (!enabled) {
            Constants.LOG.info("softwareEncoder=false，跳过 OpenH264 下载，软件编码禁用");
            return "";
        }
        try {
            Files.createDirectories(cacheDir);
        } catch (IOException e) {
            Constants.LOG.error("创建 OpenH264 缓存目录失败: {}", e.getMessage());
            return "";
        }
        Path finalLib = cacheDir.resolve(libName);
        if (Files.exists(finalLib)) {
            // 缓存校验：哈希不符（文件损坏/被替换）就删掉当作未命中，下次重新下载
            if (sha256Hex(finalLib).map(expectedSha256()::equalsIgnoreCase).orElse(false)) {
                return finalLib.toAbsolutePath().toString();
            }
            Constants.LOG.warn("OpenH264 缓存文件校验不符，删除后重新下载: {}", finalLib);
            quietDelete(finalLib);
        }
        Path archive = cacheDir.resolve(libName + ".bz2");
        if (Files.exists(archive)) {
            // 上次下载完成后解压未成的残留：交给 Rust 再试一次
            return archive.toAbsolutePath().toString();
        }
        if (!download(CISCO_BASE_URL + remoteArchiveName(), archive)) {
            return "";
        }
        return archive.toAbsolutePath().toString();
    }

    private static boolean download(String url, Path target) {
        Path part = target.resolveSibling(target.getFileName() + ".part");
        HttpClient client = HttpClient.newBuilder().connectTimeout(CONNECT_TIMEOUT).build();
        for (int attempt = 1; attempt <= 2; attempt++) {
            try {
                Constants.LOG.info("下载 OpenH264 归档 (第 {} 次): {}", attempt, url);
                HttpRequest request = HttpRequest.newBuilder(URI.create(url)).timeout(REQUEST_TIMEOUT).GET().build();
                HttpResponse<Path> response = client.send(request, HttpResponse.BodyHandlers.ofFile(part));
                // 下载写 .part 再原子改名：残留的正式名归档一定是完整文件
                if (response.statusCode() == 200 && Files.size(part) > 0) {
                    Files.move(part, target, StandardCopyOption.REPLACE_EXISTING);
                    Constants.LOG.info("OpenH264 归档下载完成: {} ({} KB)", target, Files.size(target) / 1024);
                    return true;
                }
                Constants.LOG.warn("OpenH264 下载失败: HTTP {}", response.statusCode());
            } catch (IOException e) {
                Constants.LOG.warn("OpenH264 下载异常 (第 {} 次): {}", attempt, e.getMessage());
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                quietDelete(part);
                return false;
            }
            quietDelete(part);
        }
        return false;
    }

    private static String expectedSha256() {
        return Constants.IS_WINDOWS ? WINDOWS_SHA256 : LINUX_SHA256;
    }

    private static String remoteArchiveName() {
        return Constants.IS_WINDOWS ? WINDOWS_ARCHIVE : LINUX_ARCHIVE;
    }

    private static String libName() {
        if (Constants.IS_WINDOWS) {
            return "openh264.dll";
        }
        if (Constants.IS_LINUX) {
            return "libopenh264.so";
        }
        return null;
    }

    private static Optional<String> sha256Hex(Path file) {
        try {
            MessageDigest digest = MessageDigest.getInstance("SHA-256");
            try (InputStream in = Files.newInputStream(file)) {
                byte[] buf = new byte[8192];
                int n;
                while ((n = in.read(buf)) > 0) {
                    digest.update(buf, 0, n);
                }
            }
            return Optional.of(HexFormat.of().formatHex(digest.digest()));
        } catch (Exception e) {
            Constants.LOG.warn("计算 OpenH264 缓存哈希失败: {}", e.getMessage());
            return Optional.empty();
        }
    }

    private static void quietDelete(Path file) {
        try {
            Files.deleteIfExists(file);
        } catch (IOException e) {
            Constants.LOG.warn("删除文件失败 ({}): {}", file, e.getMessage());
        }
    }
}
