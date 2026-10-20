//! Read declared specification relationships; never infer requirements from prose.
use awr_core::*;
use awr_store::Store;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecificationCheckRequest {
    #[serde(default)]
    pub spec: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
fn default_limit() -> usize {
    1000
}

fn replacement(
    items: &BTreeMap<&str, &SpecificationItem>,
    item: &SpecificationItem,
) -> (Option<String>, Option<&'static str>) {
    let mut seen = BTreeSet::new();
    let mut current = item;
    while current.disposition == RequirementDisposition::Superseded {
        if !seen.insert(&current.id) {
            return (None, Some("replacement_cycle"));
        }
        let Some(target) = &current.superseded_by else {
            return (None, Some("replacement_not_declared"));
        };
        let Some(next) = items.get(target.as_str()) else {
            return (None, Some("replacement_not_resolved"));
        };
        current = next;
    }
    ((current.id != item.id).then(|| current.id.clone()), None)
}

/// Approval content/identity validation remains in the existing source adapter.
/// Its accepted status is sufficient here; no replacement history or public validity flag.
fn specification_status(d: &Projected<Decision>) -> (&'static str, Option<&'static str>) {
    if d.source.freshness != Freshness::Fresh {
        return ("unknown", Some("specification_source_not_fresh"));
    }
    if d.item.adoption.is_some() && d.item.status == DecisionStatus::Unknown {
        return ("unknown", Some("specification_adoption_unverified"));
    }
    if d.item.status == DecisionStatus::Superseded {
        return ("unknown", Some("specification_superseded"));
    }
    if d.item.status != DecisionStatus::Accepted {
        return ("unknown", Some("specification_not_accepted"));
    }
    if d.item.superseded_by.is_some() {
        return ("unknown", Some("specification_status_conflict"));
    }
    ("current", None)
}

/// Caller provides a coherent, source-verified snapshot. No filesystem or writes occur here.
pub fn check_specifications(
    store: &Store,
    root: &Path,
    request: &SpecificationCheckRequest,
) -> Result<Value> {
    if !(1..=10000).contains(&request.limit)
        || request.spec.as_ref().is_some_and(|s| {
            s.trim().is_empty() || s.len() > 512 || s.chars().any(char::is_control)
        })
    {
        return Err(Error::InvalidInput("spec check requires limit 1..10000 and an optional nonblank registered specification key".into()));
    }
    let project = store.project_by_root(root)?;
    let decisions = store.decisions(project.id)?;
    let works = store.work_items(project.id)?;
    let mut documents: BTreeMap<&str, Vec<&Projected<Decision>>> = BTreeMap::new();
    for d in &decisions {
        documents
            .entry(&d.item.meta.external_key)
            .or_default()
            .push(d);
    }
    let assessments = decisions
        .iter()
        .map(|d| {
            (
                d.item.meta.id,
                if documents[d.item.meta.external_key.as_str()].len() == 1 {
                    specification_status(d)
                } else {
                    ("unknown", Some("specification_ambiguous"))
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let selected = |key: &str| request.spec.as_deref().is_none_or(|s| s == key);
    let references_total = works
        .iter()
        .map(|w| {
            w.item
                .spec_refs
                .iter()
                .filter(|r| selected(&r.specification))
                .count()
        })
        .sum::<usize>();
    let requirements_total = decisions
        .iter()
        .filter(|d| selected(&d.item.meta.external_key))
        .filter_map(|d| d.item.specification.as_ref())
        .map(|s| s.items.len())
        .sum::<usize>();
    let specifications_total = decisions
        .iter()
        .filter(|d| selected(&d.item.meta.external_key) && d.item.specification.is_some())
        .count();
    let required = references_total
        .saturating_add(requirements_total)
        .saturating_add(specifications_total);
    if required > request.limit {
        return Err(Error::BudgetExceeded {
            required,
            budget: request.limit,
        });
    }
    // Cross-check applicability without using it to infer or gate explicit coverage.
    let mut works_by_key: BTreeMap<&str, &Projected<WorkItem>> = BTreeMap::new();
    let mut referencing: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for w in &works {
        works_by_key.insert(&w.item.meta.external_key, w);
        for r in &w.item.spec_refs {
            referencing
                .entry(&r.specification)
                .or_default()
                .insert(&w.item.meta.external_key);
        }
    }
    let goal_keys = store
        .goals(project.id)?
        .into_iter()
        .map(|g| g.item.meta.external_key)
        .collect::<BTreeSet<_>>();
    let mut association_diagnostics = vec![];
    let mut association_issue =
        |d: &Projected<Decision>, key: &str, reason: &str, status: &str| -> Result<()> {
            let rows = required
                .saturating_add(association_diagnostics.len())
                .saturating_add(1);
            if rows > request.limit {
                return Err(Error::BudgetExceeded {
                    required: rows,
                    budget: request.limit,
                });
            }
            association_diagnostics.push(
                json!({"specification":d.item.meta.external_key,"work":key,
            "status":status,"reasons":[reason],"source_ref":d.item.meta.source_ref,
            "work_source_ref":works_by_key.get(key).map(|w|&w.item.meta.source_ref)}),
            );
            Ok(())
        };
    for d in decisions
        .iter()
        .filter(|d| selected(&d.item.meta.external_key) && d.item.specification.is_some())
    {
        let affected = d
            .item
            .affected_keys
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let empty = BTreeSet::new();
        let refs = referencing
            .get(d.item.meta.external_key.as_str())
            .unwrap_or(&empty);
        for key in refs {
            if !affected.contains(key) && !affected.contains("*") {
                association_issue(
                    d,
                    key,
                    "referencing_work_not_in_affected_keys",
                    "needs_review",
                )?;
            }
        }
        let mut listed = affected
            .iter()
            .copied()
            .filter(|key| *key != "*")
            .collect::<BTreeSet<_>>();
        if affected.contains("*") {
            listed.extend(works_by_key.keys().copied());
        }
        for key in listed {
            match works_by_key.get(key) {
                None if goal_keys.contains(key) => continue,
                None => association_issue(d, key, "affected_key_not_resolved", "unknown")?,
                Some(_) if !refs.contains(key) => {
                    association_issue(d, key, "affected_work_without_spec_ref", "needs_review")?
                }
                _ => (),
            }
        }
    }
    // One transient item index per inspected specification, reused by refs and coverage.
    let item_indexes = decisions
        .iter()
        .filter(|d| selected(&d.item.meta.external_key))
        .filter_map(|d| {
            d.item.specification.as_ref().map(|s| {
                (
                    d.item.meta.id,
                    s.items
                        .iter()
                        .map(|i| (i.id.as_str(), i))
                        .collect::<BTreeMap<_, _>>(),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    let mut refs = vec![];
    let mut mapped_refs: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
    let mut diagnostics = vec![];
    let mut specifications = vec![];
    for (key, candidates) in &documents {
        if !selected(key) || !candidates.iter().any(|d| d.item.specification.is_some()) {
            continue;
        }
        for d in candidates {
            if let Some(spec) = &d.item.specification {
                specifications.push(json!({"key":key,"version":spec.version,
                    "status":assessments[&d.item.meta.id].0,"diagnostic":assessments[&d.item.meta.id].1,
                    "item_count":spec.items.len(),
                    "decision_status":d.item.status,"source_ref":d.item.meta.source_ref,
                    "superseded_by":d.item.superseded_by}));
            }
        }
    }
    if let Some(key) = &request.spec {
        match documents.get(key.as_str()) {
            None => diagnostics.push("specification_not_registered"),
            Some(d) if !d.iter().any(|d| d.item.specification.is_some()) => {
                diagnostics.push("specification_not_declared")
            }
            _ => (),
        }
    } else if specifications.is_empty() {
        diagnostics.push("no_explicit_specifications");
    }
    // Assessment order is independent of source discovery order.
    let mut works = works.iter().collect::<Vec<_>>();
    works.sort_by_key(|w| &w.item.meta.external_key);
    for w in works {
        for (index, r) in w
            .item
            .spec_refs
            .iter()
            .enumerate()
            .filter(|(_, r)| selected(&r.specification))
        {
            let mut reasons = vec![];
            let mut unknown = false;
            let mut replacement_key = None;
            let mut observed_version = None;
            if w.source.freshness != Freshness::Fresh {
                unknown = true;
                reasons.push("work_source_not_fresh");
            }
            if matches!(w.item.status, WorkStatus::Unknown | WorkStatus::Draft) {
                unknown = true;
                reasons.push("work_scope_not_admitted");
            }
            if w.item.archived || w.item.status == WorkStatus::Cancelled {
                reasons.push("work_not_active");
            }
            if r.criterion
                .as_ref()
                .is_some_and(|c| !w.item.acceptance.contains(c))
            {
                unknown = true;
                reasons.push("criterion_not_resolved");
            }
            match documents.get(r.specification.as_str()) {
                None => {
                    unknown = true;
                    reasons.push("specification_not_registered");
                }
                Some(ds) if ds.len() != 1 => {
                    unknown = true;
                    reasons.push("specification_ambiguous");
                }
                Some(ds) => {
                    let d = ds[0];
                    let (document_state, diagnostic) = assessments[&d.item.meta.id];
                    if document_state == "unknown" {
                        unknown = true;
                        reasons.push("specification_not_currently_approved");
                        if let Some(diagnostic) = diagnostic {
                            reasons.push(diagnostic);
                        }
                    }
                    if !r
                        .source_fingerprint
                        .eq_ignore_ascii_case(&d.item.meta.source_ref.source_fingerprint)
                    {
                        reasons.push("approved_source_changed");
                    }
                    match &d.item.specification {
                        None => {
                            unknown = true;
                            reasons.push("specification_not_declared");
                        }
                        Some(spec) => {
                            observed_version = Some(spec.version.clone());
                            let items = &item_indexes[&d.item.meta.id];
                            match items.get(r.item.as_str()) {
                                None => {
                                    unknown = true;
                                    reasons.push("item_not_resolved");
                                }
                                Some(item) => {
                                    let (target, error) = replacement(items, item);
                                    replacement_key = target;
                                    if let Some(error) = error {
                                        unknown = true;
                                        reasons.push(error);
                                    }
                                    match item.disposition {
                                        RequirementDisposition::Superseded => {
                                            reasons.push("item_superseded")
                                        }
                                        RequirementDisposition::Cancelled
                                        | RequirementDisposition::OutOfScope => {
                                            reasons.push("item_not_required")
                                        }
                                        _ => (),
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let status = if unknown {
                "unknown"
            } else if reasons.is_empty() {
                "current"
            } else {
                "needs_review"
            };
            let mut source_ref = w.item.meta.source_ref.clone();
            let field = w.source.config["adapter_options"]["field_map"]["spec_refs"]
                .as_str()
                .unwrap_or("spec_refs");
            let field = field.replace('~', "~0").replace('/', "~1");
            if let Some(pointer) = &mut source_ref.pointer {
                pointer.push_str(&format!("/{field}/{index}"));
            }
            mapped_refs
                .entry((&r.specification, &r.item))
                .or_default()
                .push(refs.len());
            refs.push(json!({"work":w.item.meta.external_key,"reference":r,"status":status,"reasons":reasons,
                "observed_version":observed_version,"replacement":replacement_key,"source_ref":source_ref}));
        }
    }
    let mut requirements = vec![];
    for (key, ds) in &documents {
        if !selected(key) {
            continue;
        }
        for d in ds {
            let Some(spec) = &d.item.specification else {
                continue;
            };
            for item in &spec.items {
                let (replacement, error) = replacement(&item_indexes[&d.item.meta.id], item);
                let mapped = mapped_refs
                    .get(&(*key, item.id.as_str()))
                    .into_iter()
                    .flatten()
                    .map(|i| &refs[*i])
                    .collect::<Vec<_>>();
                let current = mapped
                    .iter()
                    .filter(|r| r["status"] == "current")
                    .map(|r| r["work"].as_str().unwrap())
                    .collect::<BTreeSet<_>>();
                let status = if ds.len() != 1
                    || error.is_some()
                    || assessments[&d.item.meta.id].0 == "unknown"
                {
                    "unknown"
                } else if matches!(
                    item.disposition,
                    RequirementDisposition::Cancelled
                        | RequirementDisposition::OutOfScope
                        | RequirementDisposition::Superseded
                ) {
                    "not_required"
                } else if !current.is_empty() {
                    "covered"
                } else if mapped.iter().any(|r| r["status"] == "unknown") {
                    "unknown"
                } else if !mapped.is_empty() {
                    "needs_review"
                } else {
                    "uncovered"
                };
                requirements.push(json!({"specification":key,"item":item.id,"disposition":item.disposition,"status":status,
                    "work_items":current,"replacement":replacement,"diagnostic":error,
                    "source_ref":d.item.meta.source_ref}));
            }
        }
    }
    requirements.sort_by(|a, b| {
        (a["specification"].as_str(), a["item"].as_str())
            .cmp(&(b["specification"].as_str(), b["item"].as_str()))
    });
    let mut counts = BTreeMap::from([
        ("covered", 0),
        ("uncovered", 0),
        ("not_required", 0),
        ("needs_review", 0),
        ("unknown", 0),
    ]);
    for r in &requirements {
        *counts.get_mut(r["status"].as_str().unwrap()).unwrap() += 1;
    }
    let statuses = refs
        .iter()
        .chain(requirements.iter())
        .chain(specifications.iter())
        .chain(association_diagnostics.iter())
        .filter_map(|r| r["status"].as_str())
        .collect::<BTreeSet<_>>();
    let status = if !diagnostics.is_empty() || statuses.contains("unknown") {
        "unknown"
    } else if statuses.contains("needs_review") || statuses.contains("uncovered") {
        "needs_review"
    } else {
        "matched"
    };
    let report = json!({"version":1,"status":status,"selection":{"spec":request.spec},
        "specifications":specifications,"references":refs,"requirements":requirements,"counts":counts,"diagnostics":diagnostics,
        "association_diagnostics":association_diagnostics,
        "read_only":true,"source_write_performed":false,"runtime_write_performed":false,
        "coverage_basis":"Explicit registered items and exact task references only; no prose scanning or inferred cancellations.",
        "boundary":"Matched/covered describe declared version bindings and task coverage, never semantic agreement, implementation, acceptance or completion. No source is modified."});
    let required = serde_json::to_vec(&report)?.len();
    if required > 4 * 1024 * 1024 {
        return Err(Error::BudgetExceeded {
            required,
            budget: 4 * 1024 * 1024,
        });
    }
    Ok(report)
}
