//! [`crate::noop_filters`] 的测试：canonical 名集合与 `slot_for` 对拍、
//! `build` 产出的 wrapper 恒等语义（包与不包结果完全一致）。

use std::collections::BTreeMap;
use std::sync::Arc;

use http_body_util::BodyExt;
use rgateway_config::{slot_for, FilterBuildCtx, FilterSpec, FILTER_NAMES};
use rgateway_core::{
    empty_body, full_body, BoxedService, GatewayError, ProxyBody, RouteState, StaticDiscovery,
};
use tower::{service_fn, ServiceExt};

use super::{noop_registry, CANONICAL_FILTER_NAMES};

/// 构造 `build` 所需的路由级上下文（noop 工厂不读取其中任何字段）。
fn build_ctx() -> FilterBuildCtx {
    FilterBuildCtx {
        route_id: Arc::from("test-route"),
        route_state: Arc::new(RouteState::new("test-route")),
        discovery: Arc::new(StaticDiscovery::new(std::collections::HashMap::new())),
        max_retry_buffer: 1024,
        config_dir: Arc::new(std::path::PathBuf::from("conf")),
    }
}

/// 恒等验证用的假内层链：恒返回 418 + 标记体 "marker"。
fn marker_chain() -> BoxedService {
    BoxedService::new(service_fn(|_req: http::Request<ProxyBody>| async move {
        let mut resp = http::Response::new(full_body(bytes::Bytes::from_static(b"marker")));
        *resp.status_mut() = http::StatusCode::IM_A_TEAPOT;
        Ok::<http::Response<ProxyBody>, GatewayError>(resp)
    }))
}

/// 取出响应体字节（消费响应；测试辅助）。
async fn body_bytes(resp: http::Response<ProxyBody>) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .expect("body collect")
        .to_bytes()
        .to_vec()
}

// 验证 noop_registry 恰好覆盖 rgateway_config::FILTER_NAMES（唯一事实源），
// 且每个名字都在 slot_for 登记——双向对拍：集合相等（增删任一侧都会失败）
// + 逐名槽位映射存在。
#[test]
fn registry_covers_exactly_the_canonical_names() {
    let registry = noop_registry();
    let mut expected = FILTER_NAMES.to_vec();
    expected.sort_unstable();
    let names = registry.names();
    assert_eq!(
        names, expected,
        "注册名集合必须与 FILTER_NAMES 一致（双向）"
    );
    for name in FILTER_NAMES {
        assert!(slot_for(name).is_some(), "{name} 必须在 slot_for 登记槽位");
        assert!(registry.get(name).is_some(), "{name} 必须注册进 registry");
    }
}

// 验证每个 canonical 名的 build 都成功、且 wrapper 是恒等包装：
// 包一层 oneshot 的结果（状态 + 体）与裸内层链完全一致。
#[tokio::test]
async fn noop_wrapper_is_identity() {
    let registry = noop_registry();
    let ctx = build_ctx();
    for name in CANONICAL_FILTER_NAMES {
        let factory = registry.get(name).expect("registered").clone();
        assert_eq!(factory.name(), name);
        // 列含义：(name, params) —— noop 工厂不读参数，任意参数都不影响输出
        let spec = FilterSpec {
            name: name.to_string(),
            params: BTreeMap::new(),
            source: name.to_string(),
        };
        let wrapper = factory.build(&spec, &ctx).expect("noop build must succeed");
        let wrapped = wrapper(marker_chain());
        let resp = wrapped
            .oneshot(http::Request::new(empty_body()))
            .await
            .expect("chain call");
        assert_eq!(resp.status(), http::StatusCode::IM_A_TEAPOT, "{name}");
        assert_eq!(body_bytes(resp).await, b"marker", "{name}");
    }
}
