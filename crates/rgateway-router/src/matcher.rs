//! 有序路由表：确定性的首条匹配（first-match-wins）查找。
//!
//! 设计要点：
//!
//! * **排序键 `(order, id)`**：路由按 `order` 升序排列，`order` 相同再按
//!   `id` 比较。`id` 在配置内唯一，因此排序构成全序——配置热重载后即使
//!   路由的输入顺序改变（例如 YAML 映射迭代顺序不稳定），匹配序列依然
//!   可复现，同一请求不会因路由顺序抖动而在重载前后命中不同路由。
//! * **first-match 语义**（与 Spring Cloud Gateway 一致）：返回第一条所有
//!   谓词全部命中的路由；重叠的模式靠 `order` 显式仲裁优先级，而不是按
//!   最长前缀等隐式规则——配置作者可以用 `order` 精确表达优先级。
//! * **查找零拷贝**：`find` 返回 `Arc<Route>` 的浅克隆（引用计数自增），
//!   路由本体（含过滤器链）在请求间共享，无深拷贝。

use std::sync::Arc;

use http::request::Parts;
use rgateway_core::{RequestCtx, Route};

/// 按 `(order, id)` 排序的路由表；首条匹配胜出（Spring 语义）。
///
/// `order` 小者先匹配；`order` 相同的路由再按 `id` 比较，保证配置热重载
/// 之间匹配序列确定不变。
pub struct RouteTable {
    /// 按 `(order, id)` 排好序的路由；`find` 顺序扫描，第一条谓词全部
    /// 命中者胜出。
    routes: Vec<Arc<Route>>,
}

impl RouteTable {
    /// 构建路由表：按 `order` 升序排序，平局以 `id` 决出先后。
    ///
    /// `id` 唯一使排序成为全序，因此无论传入顺序如何，同一组路由总得到
    /// 同一匹配序列——这是热重载确定性的来源。排序只在构造期做一次，
    /// 请求路径上的 `find` 只做顺序扫描。
    pub fn new(mut routes: Vec<Arc<Route>>) -> Self {
        // 稳定排序 + (order, id) 两级比较键；id 唯一，结果与输入顺序无关。
        routes.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.id.cmp(&b.id)));
        Self { routes }
    }

    /// 返回第一条所有谓词全部命中的路由（`Route::matches` 为 AND 语义）。
    ///
    /// 无命中时返回 `None`。返回的 `Arc` 是浅克隆，调用方持有期间即使
    /// 热重载替换了旧表，路由本体也不会被释放。
    pub fn find(&self, ctx: &RequestCtx, parts: &Parts) -> Option<Arc<Route>> {
        self.routes.iter().find(|r| r.matches(ctx, parts)).cloned()
    }

    /// 以匹配顺序（`order` 升序、平局按 `id`）返回全部路由。
    pub fn routes(&self) -> &[Arc<Route>] {
        &self.routes
    }

    /// 路由表中的路由数量。
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// 路由表是否为空。
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ant_path::PathPredicate;
    use rgateway_core::{
        empty_body, BoxedService, GatewayError, Protocol, ProtocolHint, ProxyBody, RoutePredicate,
        RouteState, RouteUri,
    };

    /// 构造匹配给定路径模式的测试路由。chain 从不会被 `RouteTable::find`
    /// 调用，仅为让 `Route` 可构造而装箱。
    fn test_route(id: &str, order: i32, patterns: &[&str]) -> Arc<Route> {
        let predicate = PathPredicate::new(patterns.iter().map(|p| (*p).to_string()).collect())
            .expect("test patterns are valid");
        let chain = BoxedService::new(tower::service_fn(
            |_req: http::Request<ProxyBody>| async move {
                Ok::<http::Response<ProxyBody>, GatewayError>(http::Response::new(empty_body()))
            },
        ));
        Arc::new(Route {
            id: Arc::from(id),
            order,
            uri: RouteUri::parse("lb://test-svc").expect("static uri"),
            protocol_hint: ProtocolHint::Auto,
            predicates: vec![Arc::new(predicate) as Arc<dyn RoutePredicate>],
            state: Arc::new(RouteState::new(id)),
            chain: std::sync::Mutex::new(chain),
        })
    }

    fn ctx() -> RequestCtx {
        RequestCtx::new(Protocol::Http1, None)
    }

    fn parts(uri: &str) -> Parts {
        http::Request::builder()
            .uri(uri)
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    // 返回 uri 命中路由的 id，用于断言匹配顺序。
    fn matched_id(table: &RouteTable, uri: &str) -> Option<String> {
        table.find(&ctx(), &parts(uri)).map(|r| r.id.to_string())
    }

    // 验证排序键 (order, id)：order 升序、平局按 id 字典序，且 len/is_empty 正确。
    #[test]
    fn sorts_by_order_then_id() {
        let table = RouteTable::new(vec![
            test_route("b", 1, &["/x"]),
            test_route("a", 2, &["/x"]),
            test_route("a1", 1, &["/x"]),
        ]);
        let ids: Vec<&str> = table.routes().iter().map(|r| r.id.as_ref()).collect();
        // order 1 的在前，其内部按 id 排序（"a1" < "b"），然后是 order 2
        assert_eq!(ids, ["a1", "b", "a"]);
        assert_eq!(table.len(), 3);
        assert!(!table.is_empty());
    }

    // 验证 order 小者优先命中，即使注册顺序相反。
    #[test]
    fn find_prefers_lowest_order() {
        // order 2 先注册，仍须让位于 order 1
        let table = RouteTable::new(vec![
            test_route("second", 2, &["/x"]),
            test_route("first", 1, &["/x"]),
        ]);
        assert_eq!(matched_id(&table, "/x").as_deref(), Some("first"));
    }

    // 验证 order 相同时按 id 决出先后。
    #[test]
    fn same_order_tiebreaks_by_id() {
        let table = RouteTable::new(vec![
            test_route("zeta", 5, &["/x"]),
            test_route("alpha", 5, &["/x"]),
        ]);
        assert_eq!(matched_id(&table, "/x").as_deref(), Some("alpha"));
    }

    // 验证首条匹配胜出，由排序结果决定而非插入顺序。
    #[test]
    fn first_match_wins_in_sorted_order() {
        // 三条都匹配；由排序决定顺序，与插入顺序无关
        let table = RouteTable::new(vec![
            test_route("c", 3, &["/x"]),
            test_route("a", 3, &["/x"]),
            test_route("b", 3, &["/x"]),
        ]);
        assert_eq!(matched_id(&table, "/x").as_deref(), Some("a"));
    }

    // 验证查找会跳过谓词不命中的路由，命中排序后的第一条。
    #[test]
    fn find_skips_non_matching_routes() {
        let table = RouteTable::new(vec![
            test_route("first", 1, &["/other"]),
            test_route("second", 2, &["/x"]),
            test_route("third", 3, &["/x"]),
        ]);
        assert_eq!(matched_id(&table, "/x").as_deref(), Some("second"));
    }

    // 验证无路由命中时返回 None。
    #[test]
    fn no_match_returns_none() {
        let table = RouteTable::new(vec![
            test_route("a", 1, &["/api/**"]),
            test_route("b", 2, &["/health"]),
        ]);
        assert_eq!(matched_id(&table, "/metrics"), None);
    }

    // 验证空表查询返回 None 且 len/is_empty 正确。
    #[test]
    fn empty_table_has_no_match() {
        let table = RouteTable::new(vec![]);
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert_eq!(matched_id(&table, "/x"), None);
    }
}
