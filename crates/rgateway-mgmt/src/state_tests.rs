//! [`MgmtState`] 的注入与访问器测试。

use std::sync::Arc;

use rgateway_config::SnapshotStore;
use rgateway_core::StateRegistry;

use super::MgmtState;

/// 用空配置编译出的快照存储（测试只需可构造的 store）。
fn stub_store() -> Arc<SnapshotStore> {
    let file = rgateway_config::load_from_str("{}").expect("empty yaml must parse");
    let states = Arc::new(StateRegistry::new());
    let compiled = rgateway_config::compile(rgateway_config::CompileInput {
        file: &file,
        registry: &rgateway_config::FilterRegistry::new(),
        states: &states,
        handler_factory: &StubFactory,
        config_dir: std::path::Path::new("."),
    })
    .expect("empty config must compile");
    Arc::new(SnapshotStore::new(compiled))
}

/// 测试替身 handler 工厂：state 测试从不执行路由链。
struct StubFactory;

impl rgateway_config::RouteHandlerFactory for StubFactory {
    fn build(&self, _route: &rgateway_config::RouteCompileInfo) -> rgateway_core::BoxedService {
        rgateway_core::BoxedService::new(tower::service_fn(
            |_req: http::Request<rgateway_core::ProxyBody>| async move {
                Ok::<http::Response<rgateway_core::ProxyBody>, rgateway_core::GatewayError>(
                    http::Response::new(rgateway_core::empty_body()),
                )
            },
        ))
    }
}

/// 表格驱动：`new` 原样暴露注入的 store / states，且默认无指标闭包。
#[test]
fn new_exposes_injected_dependencies() {
    let cases: &[(&str, u64)] = &[("initial", 1), ("initial-2", 1)];
    for &(tag, want_version) in cases {
        let store = stub_store();
        let states = Arc::new(StateRegistry::new());
        let state = MgmtState::new(Arc::clone(&store), Arc::clone(&states));

        assert!(
            Arc::ptr_eq(state.store(), &store),
            "{tag}: store must round-trip"
        );
        assert!(
            Arc::ptr_eq(state.states(), &states),
            "{tag}: states must round-trip"
        );
        assert_eq!(state.store().version(), want_version, "{tag}");
        assert!(
            state.metrics().is_none(),
            "{tag}: metrics absent by default"
        );
    }
}

/// 验证：`with_metrics` 注入渲染闭包后可调用得到其产出。
#[test]
fn with_metrics_injects_renderer() {
    let state = MgmtState::new(stub_store(), Arc::new(StateRegistry::new()))
        .with_metrics(Arc::new(|| Some("metric_text".to_string())));

    let rendered = state.metrics().map(|m| m());
    assert_eq!(rendered, Some(Some("metric_text".to_string())));
}
