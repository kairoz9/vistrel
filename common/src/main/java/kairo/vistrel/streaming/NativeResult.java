package kairo.vistrel.streaming;

import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;

/**
 * 统一 JNI 请求/应答信封：成功 {@code {"status":"ok","data":...}}，失败按可重试
 * 语义取 {@code {"status":"retry"|"fatal","message":"..."}}。
 *
 * <p>所有可能失败的请求/应答 JNI 调用共用这一形状；{@code data} 的具体类型由各调用点
 * 自行解释。{@code retry} 表示同一请求稍后可能成功，{@code fatal} 表示不会成功。
 *
 * <p>线程归属：无状态，可在任意线程解析。
 */
public record NativeResult(String status, String message, JsonElement data) {
    /**
     * 调用执行完成，{@code data} 是结果
     */
    public static final String OK = "ok";
    /**
     * 调用未执行成功，同一请求稍后可能成功
     */
    public static final String RETRY = "retry";
    /**
     * 调用不会成功，改参数或偏好后才可能
     */
    public static final String FATAL = "fatal";

    /**
     * 成功结果
     *
     * @param data 方法结果
     * @return 成功信封
     */
    public static NativeResult ok(JsonElement data) {
        return new NativeResult(OK, null, data);
    }

    /**
     * 失败结果
     *
     * @param retryable 同一请求是否可能重试成功
     * @param message   面向日志与界面的原因
     * @return 失败信封
     */
    public static NativeResult error(boolean retryable, String message) {
        return new NativeResult(retryable ? RETRY : FATAL, message, null);
    }

    /**
     * 解析 JNI 返回的信封。空串视为 JNI 层的瞬时失败；结构错误或未知状态按
     * fatal 处理，避免永久性契约漂移被浏览器当成可重试而无限重连。
     *
     * @param json JNI 返回的 JSON
     * @return 解析结果，不为 {@code null}
     */
    public static NativeResult parse(String json) {
        if (json == null || json.isEmpty()) {
            return error(true, "Native 返回空结果");
        }
        try {
            JsonElement root = JsonParser.parseString(json);
            if (!root.isJsonObject()) {
                return error(false, "Native 结果不是 JSON 对象");
            }
            JsonObject object = root.getAsJsonObject();
            JsonElement statusElement = object.get("status");
            if (statusElement == null || !statusElement.isJsonPrimitive()) {
                return error(false, "Native 结果缺少 status");
            }
            String status = statusElement.getAsString();
            if (!OK.equals(status) && !RETRY.equals(status) && !FATAL.equals(status)) {
                return error(false, "未知的 Native 状态: " + status);
            }
            JsonElement messageElement = object.get("message");
            String message =
                    messageElement != null && messageElement.isJsonPrimitive() ? messageElement.getAsString() : null;
            JsonElement data = object.get("data");
            return new NativeResult(status, message, data);
        } catch (RuntimeException e) {
            return error(false, "Native 结果解析失败: " + e.getMessage());
        }
    }

    /**
     * 调用是否执行完成
     *
     * @return true 表示 {@link #data()} 可用
     */
    public boolean ok() {
        return OK.equals(status);
    }

    /**
     * 失败是否可重试
     *
     * @return true 仅当 {@code status} 为 {@link #RETRY}
     */
    public boolean retryable() {
        return RETRY.equals(status);
    }
}
