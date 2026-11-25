use serde::Serialize;
use serde_json::Value;

/// 统一 JNI 请求/应答信封，与 Java 侧 `NativeResult` 一一对应
///
/// 成功 `{"status":"ok","data":...}`，失败按可重试语义取
/// `{"status":"retry"|"fatal","message":"..."}`：成功只带 `data`，失败只带
/// `message`，不重复空字段。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Reply {
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

/// 信封状态：`ok` 完成，`retry` / `fatal` 是失败的两种重试语义
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Retry,
    Fatal,
}

impl Reply {
    /// 调用完成，`data` 承载方法结果
    pub(crate) fn ok(data: impl Into<Value>) -> Self {
        Self {
            status: Status::Ok,
            message: None,
            data: Some(data.into()),
        }
    }

    /// 调用未成功，同一请求稍后可能成功
    pub(crate) fn retry(message: impl Into<String>) -> Self {
        Self {
            status: Status::Retry,
            message: Some(message.into()),
            data: None,
        }
    }

    /// 调用未成功，同一请求重试不会成功
    pub(crate) fn fatal(message: impl Into<String>) -> Self {
        Self {
            status: Status::Fatal,
            message: Some(message.into()),
            data: None,
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        self.status == Status::Retry
    }

    /// 失败原因；成功时为空串
    pub(crate) fn message(&self) -> &str {
        self.message.as_deref().unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn data(&self) -> Option<&Value> {
        self.data.as_ref()
    }

    /// 序列化为线协议 JSON
    ///
    /// 信封只含 JSON 原生值，序列化不会失败；兜底分支只为守住「不 panic」边界。
    pub(crate) fn encode(self) -> String {
        serde_json::to_string(&self).unwrap_or_else(|e| {
            serde_json::json!({"status": "fatal", "message": format!("信封序列化失败: {e}")})
                .to_string()
        })
    }
}
