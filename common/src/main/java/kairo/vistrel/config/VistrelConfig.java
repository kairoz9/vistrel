package kairo.vistrel.config;

import kairo.vistrel.Constants;
import kairo.vistrel.platform.Services;
import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.SecureRandom;
import java.util.Objects;
import java.util.Properties;

/**
 * Vistrel 运行时不可变配置对象
 *
 * <p>
 * 全局只维护一个不可变配置快照引用 {@link #current()}，
 * 避免静态变量与实例字段各存一份造成不一致。
 */
public final class VistrelConfig {
    private static final String CONFIG_FILE_NAME = "vistrel.properties";
    private static final String CONFIG_FILE_HEADER = "Vistrel Configuration File";
    private static final String KEY_PASSWORD = "password";
    /**
     * 每次启动重新生成随机密码且不落盘
     */
    private static final String PASSWORD_AUTO = "auto";
    /**
     * 密码的最小长度（临时生成的密码同样取该长度）
     */
    public static final int MIN_PASSWORD_LENGTH = 6;
    /**
     * 密码的最大长度
     */
    public static final int MAX_PASSWORD_LENGTH = 64;
    private static final String KEY_PORT = "port";
    /**
     * 监听地址：默认 0.0.0.0（全部网卡）
     */
    private static final String KEY_BIND_ADDRESS = "bindAddress";
    private static final String KEY_AUDIO_BITRATE = "audioBitrate";
    /**
     * 显存直通开关：关闭后恒定使用内存回读管线
     */
    private static final String KEY_VRAM_DIRECT = "vramDirect";
    /**
     * 软件编码开关：关闭后不下载、不加载 OpenH264
     * （Cisco Binary License 要求最终用户可控制该编解码器的启用与停用）
     */
    private static final String KEY_SOFTWARE_ENCODER = "softwareEncoder";
    public static final String DEFAULT_BIND_ADDRESS = "0.0.0.0";
    public static final int DEFAULT_PORT = 30808;
    public static final int DEFAULT_AUDIO_BITRATE = 128;
    public static final boolean DEFAULT_VRAM_DIRECT = true;
    public static final boolean DEFAULT_SOFTWARE_ENCODER = true;
    private static final String PASSWORD_CHARSET = "23456789abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ";
    // 全局唯一快照引用
    private static volatile VistrelConfig current = null;
    private final String bindAddress;
    private final int port;
    private final String password;
    private final int audioBitrate;
    private final boolean vramDirect;
    private final boolean softwareEncoder;
    // 密码是否为启动期临时生成（为真时只存在于内存，落盘写回哨兵值 auto）
    private final boolean ephemeralPassword;

    /**
     * 以默认监听地址 {@link #DEFAULT_BIND_ADDRESS} 构造配置快照
     *
     * @param port            监听端口
     * @param password        访问密码；空串表示禁用密码认证（任何连接免密直连）
     * @param audioBitrate    音频推流比特率 (kbps)
     * @param vramDirect      显存直通开关；关闭后恒定使用内存回读管线
     * @param softwareEncoder 软件编码开关；关闭后不下载、不加载 OpenH264
     */
    public VistrelConfig(int port, String password, int audioBitrate, boolean vramDirect, boolean softwareEncoder) {
        this(port, DEFAULT_BIND_ADDRESS, password, audioBitrate, vramDirect, softwareEncoder, false);
    }

    private VistrelConfig(
            int port,
            String bindAddress,
            String password,
            int audioBitrate,
            boolean vramDirect,
            boolean softwareEncoder,
            boolean ephemeralPassword
    ) {
        this.port = port;
        this.bindAddress = Objects.requireNonNull(bindAddress, "bindAddress 不能为空");
        this.password = Objects.requireNonNull(password, "password 不能为空");
        this.audioBitrate = audioBitrate;
        this.vramDirect = vramDirect;
        this.softwareEncoder = softwareEncoder;
        this.ephemeralPassword = ephemeralPassword;
    }

    // 衍生 with* 方法（供游戏内运行时改配置使用）
    /**
     * 衍生一份更换访问密码的配置快照，本实例不变（显式设定密码即退出启动期自动轮换）
     *
     * @param newPassword 新访问密码；空串表示禁用密码认证
     * @return 仅密码字段（及临时密码标记复位为 false）不同的新不可变实例
     */
    public VistrelConfig withPassword(String newPassword) {
        // 显式设定密码即退出自动轮换，玩家希望固定密码不变
        return new VistrelConfig(
                this.port,
                this.bindAddress,
                newPassword,
                this.audioBitrate,
                this.vramDirect,
                this.softwareEncoder,
                false
        );
    }

    /**
     * 衍生一份更换监听端口的配置快照，本实例不变
     *
     * @param newPort 新监听端口
     * @return 仅端口字段不同的新不可变实例
     */
    public VistrelConfig withPort(int newPort) {
        return new VistrelConfig(
                newPort,
                this.bindAddress,
                this.password,
                this.audioBitrate,
                this.vramDirect,
                this.softwareEncoder,
                this.ephemeralPassword
        );
    }

    /**
     * 衍生一份更换音频比特率的配置快照，本实例不变
     *
     * @param newAudioBitrate 新音频推流比特率 (kbps)
     * @return 仅音频比特率字段不同的新不可变实例
     */
    public VistrelConfig withAudioBitrate(int newAudioBitrate) {
        return new VistrelConfig(
                this.port,
                this.bindAddress,
                this.password,
                newAudioBitrate,
                this.vramDirect,
                this.softwareEncoder,
                this.ephemeralPassword
        );
    }

    /**
     * 衍生一份更换显存直通开关的配置快照，本实例不变；关闭后恒定使用内存回读管线
     *
     * @param newVramDirect 新显存直通开关
     * @return 仅显存直通字段不同的新不可变实例
     */
    public VistrelConfig withVramDirect(boolean newVramDirect) {
        return new VistrelConfig(
                this.port,
                this.bindAddress,
                this.password,
                this.audioBitrate,
                newVramDirect,
                this.softwareEncoder,
                this.ephemeralPassword
        );
    }

    /**
     * 衍生一份更换软件编码开关的配置快照，本实例不变；关闭后不下载、不加载 OpenH264
     *
     * @param newSoftwareEncoder 新软件编码开关
     * @return 仅软件编码字段不同的新不可变实例
     */
    public VistrelConfig withSoftwareEncoder(boolean newSoftwareEncoder) {
        return new VistrelConfig(
                this.port,
                this.bindAddress,
                this.password,
                this.audioBitrate,
                this.vramDirect,
                newSoftwareEncoder,
                this.ephemeralPassword
        );
    }

    public int port() {
        return port;
    }

    /**
     * 访问密码；空串表示禁用密码认证（任何连接免密直连）
     *
     * @return 访问密码原文；是否为启动期临时生成见 {@link #ephemeralPassword()}
     */
    public String password() {
        return password;
    }

    /**
     * 监听地址，默认 {@link #DEFAULT_BIND_ADDRESS}
     *
     * @return 监听地址（主机名或 IP 字面量）；合法性在绑定期校验，非法直接报错不静默回退
     */
    public String bindAddress() {
        return bindAddress;
    }

    /**
     * 密码是否由启动期临时生成（未落盘，每次启动都会换新）
     */
    public boolean ephemeralPassword() {
        return ephemeralPassword;
    }

    public int audioBitrate() {
        return audioBitrate;
    }

    public boolean vramDirect() {
        return vramDirect;
    }

    public boolean softwareEncoder() {
        return softwareEncoder;
    }

    /**
     * 密码长度是否落在允许区间；入参须为 trim 后的非空密码。
     * 命令校验与配置加载共用同一条长度规则
     *
     * @param trimmedPassword 已 trim 的候选密码（调用方保证非空）
     * @return true 表示长度落在 [{@link #MIN_PASSWORD_LENGTH}, {@link #MAX_PASSWORD_LENGTH}] 区间内
     */
    public static boolean isPasswordLengthValid(String trimmedPassword) {
        return trimmedPassword.length() >= MIN_PASSWORD_LENGTH && trimmedPassword.length() <= MAX_PASSWORD_LENGTH;
    }

    /**
     * 获取全局当前生效的配置快照 (若尚未加载则自动执行一次 load)
     *
     * @return 全局配置快照，永不返回 {@code null}
     */
    public static VistrelConfig current() {
        VistrelConfig cfg = current;
        if (cfg == null) {
            synchronized (VistrelConfig.class) {
                if (current == null) {
                    load();
                }
                cfg = current;
            }
        }
        return cfg;
    }

    /**
     * 更新全局配置快照并持久化至磁盘
     *
     * @param newConfig 新配置快照；原子替换后即为全局生效值
     * @throws NullPointerException newConfig 为 {@code null} 时抛出
     */
    public static synchronized void update(VistrelConfig newConfig) {
        current = Objects.requireNonNull(newConfig, "Config 不能为空");
        save(newConfig);
    }

    /**
     * 从配置文件读取并初始化全局快照
     */
    public static synchronized void load() {
        load(Services.PLATFORM.getConfigDir());
    }

    /**
     * 从指定目录读取并初始化全局快照（包内可见，供迁移测试定向注入目录）
     */
    public static synchronized void load(Path configDir) {
        Path configFile = configDir.resolve(CONFIG_FILE_NAME);

        Properties props = new Properties();
        boolean modified = false;

        if (Files.exists(configFile)) {
            try (BufferedReader reader = Files.newBufferedReader(configFile)) {
                props.load(reader);
            } catch (IOException e) {
                Constants.LOG.error("读取 Vistrel 配置文件失败: {}", e.getMessage());
            }
        }
        // 1. 读取访问密码（临时生成严格排除 0/O/1/l 等混淆字符）
        //    缺键或 auto：启动期临时生成并写回哨兵值，每次启动换新密码；
        //    空值：禁用密码认证，任何连接免密直连；
        //    密码：强制 [MIN, MAX] 位，越界报错并本次回退临时密码（保留原值待修正）
        String parsedPassword;
        boolean ephemeralPassword;
        String rawPassword = props.getProperty(KEY_PASSWORD);
        if (rawPassword == null || PASSWORD_AUTO.equalsIgnoreCase(rawPassword.trim())) {
            parsedPassword = generateRandomPassword(MIN_PASSWORD_LENGTH);
            ephemeralPassword = true;
            if (!PASSWORD_AUTO.equals(rawPassword)) {
                props.setProperty(KEY_PASSWORD, PASSWORD_AUTO);
                modified = true;
            }
        } else {
            String trimmedPassword = rawPassword.trim();
            if (trimmedPassword.isEmpty()) {
                parsedPassword = "";
                ephemeralPassword = false;
            } else if (!isPasswordLengthValid(trimmedPassword)) {
                Constants.LOG.error(
                        "配置项 password 长度越界 [{},{}]，本次回退为临时密码，请修正后重启",
                        MIN_PASSWORD_LENGTH,
                        MAX_PASSWORD_LENGTH
                );
                parsedPassword = generateRandomPassword(MIN_PASSWORD_LENGTH);
                ephemeralPassword = true;
            } else {
                parsedPassword = trimmedPassword;
                ephemeralPassword = false;
            }
        }
        // 2. 服务监听端口 (默认 30808)
        int parsedPort = DEFAULT_PORT;
        String rawPort = props.getProperty(KEY_PORT);
        if (rawPort != null) {
            try {
                int parsed = Integer.parseInt(rawPort.trim());
                if (parsed >= 1024 && parsed <= 65535) {
                    parsedPort = parsed;
                } else {
                    Constants.LOG.warn("配置项 port ({}) 越界 [1024..65535]，回退为默认值 {}", parsed, DEFAULT_PORT);
                }
            } catch (NumberFormatException e) {
                Constants.LOG.warn("配置项 port 非法，回退为默认值 {}", DEFAULT_PORT);
            }
        } else {
            props.setProperty(KEY_PORT, String.valueOf(DEFAULT_PORT));
            modified = true;
        }
        // 3. 音频推流比特率 (默认 128 kbps，支持 64 ~ 510)
        int parsedAudioBitrate = DEFAULT_AUDIO_BITRATE;
        String rawBitrate = props.getProperty(KEY_AUDIO_BITRATE);
        if (rawBitrate != null) {
            try {
                int parsed = Integer.parseInt(rawBitrate.trim());
                if (parsed >= 32 && parsed <= 510) {
                    parsedAudioBitrate = parsed;
                } else {
                    Constants.LOG.warn("配置项 audioBitrate ({}) 越界 [32..510]，回退为默认值 {}", parsed, DEFAULT_AUDIO_BITRATE);
                }
            } catch (NumberFormatException e) {
                Constants.LOG.warn("配置项 audioBitrate 非法，回退为默认值 {}", DEFAULT_AUDIO_BITRATE);
            }
        } else {
            props.setProperty(KEY_AUDIO_BITRATE, String.valueOf(DEFAULT_AUDIO_BITRATE));
            modified = true;
        }
        // 4. 显存直通硬件开关 (默认 true)
        boolean parsedVramDirect = DEFAULT_VRAM_DIRECT;
        String rawVramDirect = props.getProperty(KEY_VRAM_DIRECT);
        if (rawVramDirect != null) {
            parsedVramDirect = Boolean.parseBoolean(rawVramDirect.trim());
        } else {
            props.setProperty(KEY_VRAM_DIRECT, String.valueOf(DEFAULT_VRAM_DIRECT));
            modified = true;
        }
        // 5. 软件编码开关 (默认 true)；关闭后不下载、不加载 OpenH264
        boolean parsedSoftwareEncoder = DEFAULT_SOFTWARE_ENCODER;
        String rawSoftwareEncoder = props.getProperty(KEY_SOFTWARE_ENCODER);
        if (rawSoftwareEncoder != null) {
            parsedSoftwareEncoder = Boolean.parseBoolean(rawSoftwareEncoder.trim());
        } else {
            props.setProperty(KEY_SOFTWARE_ENCODER, String.valueOf(DEFAULT_SOFTWARE_ENCODER));
            modified = true;
        }
        // 6. 监听地址 (默认 0.0.0.0 即全部网卡)；合法性在绑定期校验，非法直接 fail-closed
        String parsedBindAddress = DEFAULT_BIND_ADDRESS;
        String rawBindAddress = props.getProperty(KEY_BIND_ADDRESS);
        if (rawBindAddress != null && !rawBindAddress.trim().isEmpty()) {
            parsedBindAddress = rawBindAddress.trim();
        } else {
            props.setProperty(KEY_BIND_ADDRESS, DEFAULT_BIND_ADDRESS);
            modified = true;
        }
        // 若首次创建、补全默认键或生成了随机密码，写回保存配置文件
        if (modified || !Files.exists(configFile)) {
            storeProps(configDir, props);
        }
        // 构造单一实例存入全局快照
        current = new VistrelConfig(
                parsedPort,
                parsedBindAddress,
                parsedPassword,
                parsedAudioBitrate,
                parsedVramDirect,
                parsedSoftwareEncoder,
                ephemeralPassword
        );
    }

    /**
     * 将配置持久化保存至磁盘文件；写盘失败仅记日志，不向调用方抛出
     *
     * @param config 待持久化的配置快照；临时密码不落盘，写回哨兵值 {@code auto}
     */
    public static synchronized void save(VistrelConfig config) {
        Path configDir = Services.PLATFORM.getConfigDir();
        Properties props = new Properties();
        // 临时密码不落盘：写回哨兵值 auto，下次启动重新生成
        props.setProperty(KEY_PASSWORD, config.ephemeralPassword() ? PASSWORD_AUTO : config.password());
        props.setProperty(KEY_PORT, String.valueOf(config.port()));
        props.setProperty(KEY_BIND_ADDRESS, config.bindAddress());
        props.setProperty(KEY_AUDIO_BITRATE, String.valueOf(config.audioBitrate()));
        props.setProperty(KEY_VRAM_DIRECT, String.valueOf(config.vramDirect()));
        props.setProperty(KEY_SOFTWARE_ENCODER, String.valueOf(config.softwareEncoder()));

        storeProps(configDir, props);
    }

    /**
     * 写出配置文件；失败仅记日志，不打断调用方
     */
    private static void storeProps(Path configDir, Properties props) {
        try {
            Files.createDirectories(configDir);
            try (BufferedWriter writer = Files.newBufferedWriter(configDir.resolve(CONFIG_FILE_NAME))) {
                props.store(writer, CONFIG_FILE_HEADER);
            }
        } catch (IOException e) {
            Constants.LOG.error("写入 Vistrel 配置文件失败: {}", e.getMessage());
        }
    }

    private static String generateRandomPassword(int length) {
        SecureRandom random = new SecureRandom();
        StringBuilder sb = new StringBuilder(length);
        for (int i = 0; i < length; i++) {
            int idx = random.nextInt(PASSWORD_CHARSET.length());
            sb.append(PASSWORD_CHARSET.charAt(idx));
        }
        return sb.toString();
    }
}
