//! Bounded worker pools (plan: "Worker fairness & job queue" → "Shape").
//!
//! Four work classes, each with a **total** in-flight cap (per pod) and a
//! **per-tenant** in-flight cap. The total cap is a tokio [`Semaphore`]; the
//! per-tenant cap is a counter map checked-and-bumped under one mutex before
//! the semaphore is tried. [`WorkerPools::try_acquire`] never waits — the
//! dispatcher's round-robin treats a refusal as "skip this tenant (or this
//! class) for the rest of the tick"; capacity freed mid-tick is picked up by
//! the next tick rather than by parked waiters, keeping the selection loop
//! free of wakeup ordering questions.
//!
//! Caps are per-pod by design: a tenant's effective fleet-wide concurrency
//! is `per-tenant cap × pod count` (plan: "Shape"). Default numbers are the
//! plan's initial guesses calibrated to ~50 active tenants per pod; every
//! one is a `serve` CLI knob.
//!
//! Work-type overrides (e.g. reports = llm class with per-tenant cap 1) are
//! the *caller's* vocabulary: [`WorkerPools::try_acquire`] takes an optional
//! tighter per-tenant cap per call and applies `min(class cap, override)`.
//! The pool itself stays work-type-agnostic — one counter per
//! `(class, tenant)`, so an override tightens admission without splitting
//! the accounting.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

/// The four work classes (plan table: "How each work-type lands").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkClass {
    /// `atom_pipeline_jobs` execution: embedding + tagging batches.
    Embedding,
    /// Long LLM syntheses: wiki regeneration, reports.
    Llm,
    /// Feed polls (fetch + parse + ingest).
    Ingestion,
    /// Housekeeping system tasks: draft pipeline, graph maintenance,
    /// ledger GC.
    Maintenance,
}

impl WorkClass {
    /// All classes, for iteration (metrics, tests).
    pub const ALL: [WorkClass; 4] = [
        WorkClass::Embedding,
        WorkClass::Llm,
        WorkClass::Ingestion,
        WorkClass::Maintenance,
    ];
}

/// One class's caps: total in-flight per pod, and in-flight per tenant.
#[derive(Debug, Clone, Copy)]
pub struct PoolCaps {
    pub total: usize,
    pub per_tenant: usize,
}

/// Caps for all four classes. Defaults are the plan's table.
#[derive(Debug, Clone, Copy)]
pub struct WorkerPoolsConfig {
    pub embedding: PoolCaps,
    pub llm: PoolCaps,
    pub ingestion: PoolCaps,
    pub maintenance: PoolCaps,
}

impl Default for WorkerPoolsConfig {
    fn default() -> Self {
        Self {
            embedding: PoolCaps {
                total: 32,
                per_tenant: 4,
            },
            llm: PoolCaps {
                total: 16,
                per_tenant: 2,
            },
            ingestion: PoolCaps {
                total: 16,
                per_tenant: 4,
            },
            maintenance: PoolCaps {
                total: 8,
                per_tenant: 1,
            },
        }
    }
}

impl WorkerPoolsConfig {
    fn caps(&self, class: WorkClass) -> PoolCaps {
        match class {
            WorkClass::Embedding => self.embedding,
            WorkClass::Llm => self.llm,
            WorkClass::Ingestion => self.ingestion,
            WorkClass::Maintenance => self.maintenance,
        }
    }
}

/// Per-class admission state: the total-cap semaphore plus the
/// per-tenant in-flight counters. Counters are shared with every
/// [`PoolPermit`] handed out so a worker finishing on another task
/// releases its slot without going through the pool.
struct ClassPool {
    caps: PoolCaps,
    total: Arc<Semaphore>,
    per_tenant: Arc<Mutex<HashMap<String, usize>>>,
}

/// The four bounded pools. Cheap to share via `Arc`; all methods take
/// `&self`.
pub struct WorkerPools {
    classes: HashMap<WorkClass, ClassPool>,
}

impl WorkerPools {
    pub fn new(config: WorkerPoolsConfig) -> Self {
        let classes = WorkClass::ALL
            .into_iter()
            .map(|class| {
                let caps = config.caps(class);
                (
                    class,
                    ClassPool {
                        caps,
                        // A zero total would make the semaphore permanently
                        // empty AND trip Semaphore's max-permits assert on
                        // some versions; clamp so a misconfigured pool
                        // degrades to serial rather than wedged.
                        total: Arc::new(Semaphore::new(caps.total.max(1))),
                        per_tenant: Arc::new(Mutex::new(HashMap::new())),
                    },
                )
            })
            .collect();
        Self { classes }
    }

    /// Try to admit one job for `(class, tenant)` without waiting.
    ///
    /// `per_tenant_cap_override` tightens (never loosens) the class's
    /// per-tenant cap for this admission — the reports work-type passes the
    /// plan's `1`. Returns `None` when the tenant is at its cap or the
    /// class total is exhausted; the permit releases both slots on drop.
    pub fn try_acquire(
        &self,
        class: WorkClass,
        tenant: &str,
        per_tenant_cap_override: Option<usize>,
    ) -> Option<PoolPermit> {
        let pool = &self.classes[&class];
        let cap = per_tenant_cap_override
            .map(|o| o.min(pool.caps.per_tenant))
            .unwrap_or(pool.caps.per_tenant)
            .max(1);

        // Reserve the tenant slot first, under the counter lock, so two
        // concurrent admissions for the same tenant can't both pass the
        // check; release it again if the total semaphore refuses.
        {
            let mut counts = pool.per_tenant.lock().expect("pool counters poisoned");
            let count = counts.entry(tenant.to_string()).or_insert(0);
            if *count >= cap {
                return None;
            }
            *count += 1;
        }

        match Arc::clone(&pool.total).try_acquire_owned() {
            Ok(permit) => Some(PoolPermit {
                _total: permit,
                class,
                tenant: tenant.to_string(),
                per_tenant: Arc::clone(&pool.per_tenant),
            }),
            Err(TryAcquireError::NoPermits) | Err(TryAcquireError::Closed) => {
                release_tenant_slot(&pool.per_tenant, tenant);
                None
            }
        }
    }

    /// Jobs currently in flight for `(class, tenant)`. Test/metrics
    /// instrumentation.
    pub fn in_flight(&self, class: WorkClass, tenant: &str) -> usize {
        self.classes[&class]
            .per_tenant
            .lock()
            .expect("pool counters poisoned")
            .get(tenant)
            .copied()
            .unwrap_or(0)
    }

    /// Jobs currently in flight for `class` across every tenant.
    pub fn total_in_flight(&self, class: WorkClass) -> usize {
        let pool = &self.classes[&class];
        pool.caps.total.max(1) - pool.total.available_permits()
    }
}

fn release_tenant_slot(per_tenant: &Mutex<HashMap<String, usize>>, tenant: &str) {
    let mut counts = per_tenant.lock().expect("pool counters poisoned");
    if let Some(count) = counts.get_mut(tenant) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            counts.remove(tenant);
        }
    }
}

/// An admitted job's capacity: one unit of the class total plus one unit of
/// the tenant's allowance. Both release on drop — hold it for exactly the
/// lifetime of the work.
pub struct PoolPermit {
    _total: OwnedSemaphorePermit,
    class: WorkClass,
    tenant: String,
    per_tenant: Arc<Mutex<HashMap<String, usize>>>,
}

impl PoolPermit {
    pub fn class(&self) -> WorkClass {
        self.class
    }

    pub fn tenant(&self) -> &str {
        &self.tenant
    }
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        release_tenant_slot(&self.per_tenant, &self.tenant);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_pools() -> WorkerPools {
        WorkerPools::new(WorkerPoolsConfig {
            embedding: PoolCaps {
                total: 3,
                per_tenant: 2,
            },
            llm: PoolCaps {
                total: 2,
                per_tenant: 2,
            },
            ingestion: PoolCaps {
                total: 1,
                per_tenant: 1,
            },
            maintenance: PoolCaps {
                total: 1,
                per_tenant: 1,
            },
        })
    }

    #[test]
    fn per_tenant_cap_refuses_third_job() {
        let pools = small_pools();
        let _a = pools.try_acquire(WorkClass::Embedding, "t1", None).unwrap();
        let _b = pools.try_acquire(WorkClass::Embedding, "t1", None).unwrap();
        assert!(pools
            .try_acquire(WorkClass::Embedding, "t1", None)
            .is_none());
        // Another tenant still fits under the remaining total.
        assert!(pools
            .try_acquire(WorkClass::Embedding, "t2", None)
            .is_some());
    }

    #[test]
    fn total_cap_refuses_across_tenants() {
        let pools = small_pools();
        let _a = pools.try_acquire(WorkClass::Llm, "t1", None).unwrap();
        let _b = pools.try_acquire(WorkClass::Llm, "t2", None).unwrap();
        assert!(pools.try_acquire(WorkClass::Llm, "t3", None).is_none());
        // Refusal must not leak the tenant slot it briefly reserved.
        assert_eq!(pools.in_flight(WorkClass::Llm, "t3"), 0);
    }

    #[test]
    fn drop_releases_both_slots() {
        let pools = small_pools();
        let permit = pools.try_acquire(WorkClass::Ingestion, "t1", None).unwrap();
        assert_eq!(pools.in_flight(WorkClass::Ingestion, "t1"), 1);
        assert_eq!(pools.total_in_flight(WorkClass::Ingestion), 1);
        drop(permit);
        assert_eq!(pools.in_flight(WorkClass::Ingestion, "t1"), 0);
        assert_eq!(pools.total_in_flight(WorkClass::Ingestion), 0);
        assert!(pools
            .try_acquire(WorkClass::Ingestion, "t1", None)
            .is_some());
    }

    #[test]
    fn override_tightens_but_never_loosens() {
        let pools = small_pools();
        // Tighten: llm per-tenant is 2, override 1 admits only one.
        let _a = pools.try_acquire(WorkClass::Llm, "t1", Some(1)).unwrap();
        assert!(pools.try_acquire(WorkClass::Llm, "t1", Some(1)).is_none());
        // Never loosen: maintenance per-tenant is 1; override 5 is clamped.
        let _b = pools
            .try_acquire(WorkClass::Maintenance, "t1", Some(5))
            .unwrap();
        assert!(pools
            .try_acquire(WorkClass::Maintenance, "t1", Some(5))
            .is_none());
    }

    #[test]
    fn classes_are_independent() {
        let pools = small_pools();
        let _a = pools.try_acquire(WorkClass::Ingestion, "t1", None).unwrap();
        // Ingestion exhausted; maintenance unaffected.
        assert!(pools
            .try_acquire(WorkClass::Ingestion, "t2", None)
            .is_none());
        assert!(pools
            .try_acquire(WorkClass::Maintenance, "t2", None)
            .is_some());
    }
}
