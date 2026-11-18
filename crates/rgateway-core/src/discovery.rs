//! 服务发现抽象。
//!
//! 随附实现是 YAML `discovery.providers[type=static]` 静态配置；动态注册
//! 中心（Nacos/Consul 等）通过实现 [`ServiceDiscovery`] 并让 `watch` 返回
//! 更新流即可接入，调用方代码无需改动——`watch` 预留的变更通知就是为此。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::watch;

use crate::lb::Instance;

/// 把服务名解析为实例集合的发现接口。
///
/// 约定：实现必须 `Send + Sync`（网关会在多个 tokio worker 上并发查询）；
/// [`Self::get`] 对未知服务返回空 vec 而非报错，调用方按“无可用实例”处理。
pub trait ServiceDiscovery: Send + Sync {
    /// 查询 `service` 当前实例集合；未知服务返回空 vec。
    fn get(&self, service: &str) -> Vec<Instance>;

    /// 订阅 `service` 实例集合的变更流；`None` 表示静态提供方（无通知）。
    fn watch(&self, service: &str) -> Option<watch::Receiver<Vec<Instance>>>;

    /// 列出所有已知服务名（供管理接口输出；实现宜排序保证输出稳定）。
    fn services(&self) -> Vec<String>;
}

/// 由 YAML `discovery.providers[type=static]` 块驱动的静态提供方。
#[derive(Debug, Clone, Default)]
pub struct StaticDiscovery {
    services: HashMap<Arc<str>, Vec<Instance>>,
}

impl StaticDiscovery {
    /// 用「服务名 → 实例列表」表构建（配置编译期调用）。
    pub fn new(services: HashMap<Arc<str>, Vec<Instance>>) -> Self {
        Self { services }
    }

    /// 某服务的实例列表；未知服务返回空 vec。
    pub fn instances(&self, service: &str) -> Vec<Instance> {
        self.services.get(service).cloned().unwrap_or_default()
    }
}

impl ServiceDiscovery for StaticDiscovery {
    fn get(&self, service: &str) -> Vec<Instance> {
        self.instances(service)
    }

    /// 静态配置没有变更通知，恒返回 `None`。
    fn watch(&self, _service: &str) -> Option<watch::Receiver<Vec<Instance>>> {
        None
    }

    /// 服务名按字典序排序后返回，保证输出稳定。
    fn services(&self) -> Vec<String> {
        let mut names: Vec<String> = self.services.keys().map(|k| k.to_string()).collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn inst(port: u16) -> Instance {
        Instance::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port), 100)
    }

    /// 验证：已知服务返回注册的实例、未知服务返回空，services() 排序输出。
    #[test]
    fn static_discovery_returns_known_instances() {
        let mut services = HashMap::new();
        services.insert(Arc::from("user-svc"), vec![inst(50051), inst(50052)]);
        services.insert(Arc::from("order-svc"), vec![inst(50061)]);
        let sd = StaticDiscovery::new(services);

        assert_eq!(sd.get("user-svc").len(), 2);
        assert_eq!(sd.get("order-svc").len(), 1);
        assert!(sd.get("missing-svc").is_empty());
        assert_eq!(sd.services(), vec!["order-svc", "user-svc"]);
    }

    /// 验证：静态提供方的 watch 恒为 None（无变更通知）。
    #[test]
    fn static_discovery_has_no_watch() {
        let sd = StaticDiscovery::default();
        assert!(sd.watch("any").is_none());
    }
}
