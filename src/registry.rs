//! Runtime registry: an in-memory snapshot of the admin-configured providers,
//! accounts, models, aliases, and routes. Reloaded from the database whenever
//! configuration changes so edits take effect without a restart (FR-10.13).
//!
//! In-flight requests keep the `Arc<Registry>` snapshot they started with.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::RwLock;

use crate::db::{
    self, AccountRow, AliasRow, ModelRow, Pool, ProviderRow, RouteRow, RouteTargetRow,
};

/// An immutable runtime configuration snapshot (FR-10.13, NFR-2.10).
///
/// The data plane holds an `Arc<Snapshot>` for the lifetime of a request, so a
/// configuration change (which swaps in a new snapshot) never affects in-flight
/// work. There are no interior locks on this type; it is read-only once built.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<RwLock<Arc<Snapshot>>>,
}

#[derive(Default)]
pub struct Snapshot {
    pub providers: HashMap<String, ProviderRow>,
    pub provider_order: Vec<String>,
    pub accounts: HashMap<String, AccountRow>,
    pub models: HashMap<String, ModelRow>,
    /// (provider_id, upstream_id) -> model_id
    pub model_by_upstream: HashMap<(String, String), String>,
    pub aliases: HashMap<String, AliasRow>,
    pub routes: HashMap<String, RouteRow>,
    pub route_targets: HashMap<String, Vec<RouteTargetRow>>,
}

/// A resolved routing decision for a client-requested model name.
#[derive(Debug, Clone)]
pub enum Resolved {
    /// A single (provider, model) target.
    Single {
        provider_id: String,
        model_id: String,
    },
    /// A route with an ordered list of targets.
    Route {
        route: RouteRow,
        targets: Vec<ResolvedTarget>,
    },
}

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub account: AccountRow,
    pub model: ModelRow,
    pub provider: ProviderRow,
    /// Stable logical route-target identity. Multiple account candidates for an
    /// unpinned route target share this id so route strategy/weight is applied
    /// once to the logical target, not once per account.
    pub route_target_id: Option<String>,
    pub priority: i64,
    pub weight: i64,
    /// Optional typed eligibility predicate (FR-12.3).
    pub predicate: crate::predicate::TargetPredicate,
    /// Optional per-target parameter overrides (FR-12.2).
    pub param_overrides: serde_json::Value,
}

impl Registry {
    pub fn new() -> Self {
        Registry {
            inner: Arc::new(RwLock::new(Arc::new(Snapshot::default()))),
        }
    }

    pub async fn reload(&self, pool: &Pool) -> Result<()> {
        let providers = db::list_providers(pool).await?;
        let accounts = db::list_accounts(pool).await?;
        let models = db::list_models(pool).await?;
        let aliases = db::list_aliases(pool).await?;
        let routes = db::list_routes(pool).await?;

        let mut snap = Snapshot::default();
        for p in providers {
            if p.enabled == 0 {
                continue;
            }
            snap.provider_order.push(p.id.clone());
            snap.providers.insert(p.id.clone(), p);
        }
        for a in accounts {
            snap.accounts.insert(a.id.clone(), a);
        }
        for m in models {
            snap.model_by_upstream
                .insert((m.provider_id.clone(), m.upstream_id.clone()), m.id.clone());
            snap.models.insert(m.id.clone(), m);
        }
        for a in aliases {
            snap.aliases.insert(a.alias.clone(), a);
        }
        for c in routes {
            let targets = db::route_targets(pool, &c.id).await?;
            snap.route_targets.insert(c.id.clone(), targets);
            snap.routes.insert(c.id.clone(), c);
        }

        // Atomically activate the new immutable snapshot (NFR-2.10).
        *self.inner.write() = Arc::new(snap);
        Ok(())
    }

    /// Take a reference to the current immutable snapshot.
    ///
    /// A request should call this once at the start and use the returned Arc
    /// throughout, so it is unaffected by concurrent configuration changes.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner.read().clone()
    }

    /// The number of providers currently in the active snapshot.
    pub fn provider_count(&self) -> usize {
        self.inner.read().providers.len()
    }

    /// Resolve a client-facing model name to a route against the active snapshot.
    pub fn resolve(&self, requested: &str) -> Option<Resolved> {
        Self::resolve_in(&self.snapshot(), requested)
    }

    /// Resolve against a caller-held snapshot (used by the request pipeline so
    /// the whole request sees one consistent view, NFR-2.10).
    pub fn resolve_in(snap: &Snapshot, requested: &str) -> Option<Resolved> {
        // 1. Alias table.
        if let Some(alias) = snap.aliases.get(requested) {
            if alias.target_type == "route" {
                if let Some(route) = Self::build_route(snap, &alias.target_id) {
                    return Some(route);
                }
            } else if let Some(m) = snap.models.get(&alias.target_id) {
                if m.enabled != 0 {
                    return Some(Resolved::Single {
                        provider_id: m.provider_id.clone(),
                        model_id: m.id.clone(),
                    });
                }
            }
        }

        // 2. Route by name.
        if let Some(route) = snap.routes.values().find(|c| c.name == requested) {
            if let Some(route) = Self::build_route(snap, &route.id) {
                return Some(route);
            }
        }

        // 3. `provider/model-id` (provider matched by name or id).
        if let Some((prov_part, model_part)) = requested.split_once('/') {
            let provider = snap
                .providers
                .values()
                .find(|p| p.name == prov_part || p.id == prov_part);
            if let Some(p) = provider {
                if let Some(mid) = snap
                    .model_by_upstream
                    .get(&(p.id.clone(), model_part.to_string()))
                {
                    if let Some(m) = snap.models.get(mid) {
                        if m.enabled != 0 {
                            return Some(Resolved::Single {
                                provider_id: m.provider_id.clone(),
                                model_id: m.id.clone(),
                            });
                        }
                    }
                }
            }
        }

        // 4. Bare upstream model id.
        if let Some(m) = snap
            .models
            .values()
            .find(|m| m.upstream_id == requested && m.enabled != 0)
        {
            return Some(Resolved::Single {
                provider_id: m.provider_id.clone(),
                model_id: m.id.clone(),
            });
        }

        None
    }

    fn build_route(snap: &Snapshot, route_id: &str) -> Option<Resolved> {
        let route = snap.routes.get(route_id)?.clone();
        if route.enabled == 0 {
            return None;
        }
        let mut targets = Vec::new();
        for t in snap.route_targets.get(route_id).into_iter().flatten() {
            let Some(model) = snap.models.get(&t.model_id).cloned() else {
                continue;
            };
            // A disabled model is never a routable target (FR-10.2).
            if model.enabled == 0 {
                continue;
            }
            let Some(provider) = snap.providers.get(&model.provider_id).cloned() else {
                continue;
            };
            // An explicit account remains pinned. An unpinned target expands
            // to the provider's full account pool; the pipeline later orders
            // these siblings without multiplying this logical target's route
            // priority/weight.
            let accounts: Vec<AccountRow> = match &t.account_id {
                Some(aid) => snap
                    .accounts
                    .get(aid)
                    .filter(|a| a.provider_id == model.provider_id)
                    .cloned()
                    .into_iter()
                    .collect(),
                None => snap
                    .accounts
                    .values()
                    .filter(|a| a.provider_id == model.provider_id)
                    .cloned()
                    .collect(),
            };
            for account in accounts {
                targets.push(ResolvedTarget {
                    account,
                    model: model.clone(),
                    provider: provider.clone(),
                    route_target_id: Some(t.id.clone()),
                    priority: t.priority,
                    weight: t.weight,
                    predicate: crate::predicate::TargetPredicate::parse(&t.predicate),
                    param_overrides: serde_json::from_str(&t.param_overrides)
                        .unwrap_or(serde_json::Value::Null),
                });
            }
        }
        if targets.is_empty() {
            return None;
        }
        Some(Resolved::Route { route, targets })
    }

    /// All enabled models the registry knows, for `/v1/models`.
    pub fn enabled_models(&self) -> Vec<ModelRow> {
        let snap = self.snapshot();
        snap.models
            .values()
            .filter(|m| m.enabled != 0)
            .cloned()
            .collect()
    }

    /// Provider by id.
    pub fn provider(&self, id: &str) -> Option<ProviderRow> {
        self.snapshot().providers.get(id).cloned()
    }

    pub fn model(&self, id: &str) -> Option<ModelRow> {
        self.snapshot().models.get(id).cloned()
    }

    pub fn account(&self, id: &str) -> Option<AccountRow> {
        self.snapshot().accounts.get(id).cloned()
    }

    pub fn route_name(&self, id: &str) -> Option<String> {
        self.snapshot().routes.get(id).map(|c| c.name.clone())
    }

    /// The full route row (used for cache-affinity / portability policy).
    pub fn route_row(&self, id: &str) -> Option<RouteRow> {
        self.snapshot().routes.get(id).cloned()
    }

    pub fn aliases(&self) -> Vec<AliasRow> {
        self.snapshot().aliases.values().cloned().collect()
    }

    /// Enabled Routes whose target list is currently empty, i.e. no eligible
    /// target (disabled model, missing account, or no targets configured).
    /// Used by the startup diagnostic and alerting; it only reads the snapshot.
    pub fn routes_with_no_targets(&self) -> Vec<String> {
        let snap = self.snapshot();
        snap.routes
            .values()
            .filter(|r| r.enabled != 0)
            .filter(|r| {
                snap.route_targets
                    .get(&r.id)
                    .map(|ts| ts.is_empty())
                    .unwrap_or(true)
            })
            .map(|r| r.name.clone())
            .collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
