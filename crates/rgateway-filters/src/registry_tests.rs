//! registry() 测试：恰好包含全部已实装工厂、canonical 名与槽位一致。

use rgateway_config::slot_for;

use super::*;
use crate::test_support::{build_ctx, empty_spec, spec, spec_yaml};

/// 验证注册表恰好包含全部已实装过滤器（未实装名字属后续阶段，不归本
/// crate），名字有序且与 [`slot_for`] 的 canonical 映射一致。
#[test]
fn registry_registers_exactly_builtin_factories() {
    let registry = registry();
    assert_eq!(
        registry.names(),
        vec![
            "ApiKeyAuth",
            "BodyLimit",
            "CircuitBreaker",
            "IpFilter",
            "JwtAuth",
            "RateLimit",
            "Retry",
            "Timeout"
        ],
        "registry must contain exactly the built-in filters (sorted)"
    );
    for name in registry.names() {
        let factory = registry.get(name).expect("factory registered");
        assert_eq!(factory.name(), name, "canonical name must match the key");
        assert!(
            slot_for(name).is_some(),
            "{name} must map to a canonical chain slot"
        );
    }
}

/// 验证每个工厂可独立构建（拿得到 ServiceWrapper）：注册表条目真实可用，
/// 不是占位。
#[test]
fn every_registered_factory_builds_a_wrapper() {
    let states = rgateway_core::StateRegistry::new();
    let fctx = build_ctx("r1", states.route_state("r1"));
    let registry = registry();
    // 每行 = (过滤器紧凑 spec 文本)
    let cases = [
        "RateLimit=key:client_ip,rate:10,period:1s",
        "Timeout=per_attempt:5s",
        "BodyLimit=max_body:1KB",
        "Retry=retries:3",
    ];
    for compact in cases {
        let spec = spec(compact);
        let factory = registry.get(&spec.name).expect("registered");
        let _wrapper = factory.build(&spec, &fctx).expect("build must succeed");
    }
    // CircuitBreaker 无参（结构化空参形态）
    let factory = registry.get("CircuitBreaker").expect("registered");
    let _wrapper = factory
        .build(&empty_spec("CircuitBreaker"), &fctx)
        .expect("build must succeed");
    // IpFilter 列表参数（结构化形态）
    let spec = spec_yaml(r#"{ name: IpFilter, allow: ["10.0.0.0/8"] }"#);
    let factory = registry.get("IpFilter").expect("registered");
    let _wrapper = factory.build(&spec, &fctx).expect("build must succeed");
    // ApiKeyAuth map 参数（结构化形态）
    let spec = spec_yaml("{ name: ApiKeyAuth, keys: { s: alice } }");
    let factory = registry.get("ApiKeyAuth").expect("registered");
    let _wrapper = factory.build(&spec, &fctx).expect("build must succeed");
}
