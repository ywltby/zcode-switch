//! Process-wide egress proxy slot shared by every ureq agent.
//!
//! 营销活动资格跟出口环境绑定（IP/设备指纹）：同一出口下换 device mid 无效。
//! 设置里的 auth_proxy 因此不只服务 OAuth 登录窗口，还覆盖领取、活动查询、
//! 激活上报与额度查询；地址解析失败时退回直连，不会让领取悄悄失联。

use std::sync::{Mutex, OnceLock};

static TRAFFIC_PROXY: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<String>> {
    TRAFFIC_PROXY.get_or_init(|| Mutex::new(None))
}

pub fn set_proxy(url: Option<String>) {
    let normalized = url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty());
    *slot().lock().unwrap_or_else(|e| e.into_inner()) = normalized;
}

pub fn proxy() -> Option<String> {
    slot().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn apply_proxy(builder: ureq::AgentBuilder) -> ureq::AgentBuilder {
    let Some(url) = proxy() else { return builder };
    match ureq::Proxy::new(&url) {
        Ok(proxy) => builder.proxy(proxy),
        Err(_) => builder,
    }
}
