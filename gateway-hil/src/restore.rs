//! Putting the device back the way the run found it.
//!
//! A run points the device at its own OPC UA server, which stops when the
//! runner exits. Left like that, the device reports `connecting` or `error`
//! from then on, and the copy in NVS keeps it dialling the dead host across
//! reboots. So before the first phase the runner saves the `opcua` shadow's
//! `desired` and the retained bundle it points at, and on the way out puts
//! both back.
//!
//! Three things decide whether a restore actually takes:
//!
//! * **A new version.** The device ignores the version it is running
//!   (requirements §7), and a run can end on the very number the pre-run
//!   config had: the online phases always count 1 to 8. The bundle embeds its
//!   version, so it is re-issued under the new one too, with a new digest.
//! * **A merge patch, not a document.** AWS merges `desired` into what is
//!   there. A field the run added and the pre-run document lacks — the test
//!   namespace's `ns_uri`, say — would survive a plain update and quietly
//!   change the restored config.
//! * **Never the run's own server.** A pre-run document that dials this run's
//!   endpoint is left over from an earlier run; restoring it would point the
//!   device at a server that is about to stop.
//!
//! When the pre-run config cannot be restored — there was none, its bundle is
//! gone, or it is such a leftover — the device is left disabled instead, so it
//! ends `idle` rather than dialling a host that no longer serves.
//!
//! The snapshot is also saved next to the run's artifacts until the restore
//! has gone through, so a run that died first can be finished with
//! `--restore`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use gateway_core::bundle::TagBundle;
use gateway_core::codec::sha256_hex;
use gateway_core::settings::DesiredSettings;
use gateway_opcua::AppliedConfig;
use opcua_test_server::catalogue::{self, NAMESPACE_URI};
use opcua_test_server::documents::{self, Desired};
use serde_json::{json, Map, Value};

use crate::cloud::{cfg_version, now_ms, Cloud};
use crate::interrupt;
use crate::phases::{show, truncate};
use crate::report::{banner, info, PhaseResult};

/// How long the device gets to take the restored config up. Long enough to
/// fetch the bundle and fail one connect attempt at the default keep-alive.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(180);

/// Where a disabled device's document points. Syntactically valid, because
/// the device validates a document before it looks at `enabled`, and
/// unresolvable (RFC 2606), so the shadow no longer names a real host.
const NO_ENDPOINT: &str = "opc.tcp://unconfigured.invalid:4840";

/// A retained message, verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct Retained {
    pub topic: String,
    pub payload: Vec<u8>,
}

/// What the run found before it changed anything.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub thing: String,
    /// Unix ms when it was taken.
    pub taken_ms: i64,
    /// The run's own server, which the restore must never point at.
    pub run_endpoint: String,
    /// Highest config version the shadow knew of; the restore goes past it.
    pub last_version: u32,
    /// `state.desired` as found; `None` when there was none.
    pub desired: Option<Value>,
    /// The retained bundle `desired.cfg.topic` named, if it was there.
    pub bundle: Option<Retained>,
}

/// Why the device is left disabled rather than on its pre-run config.
#[derive(Debug, Clone, PartialEq)]
pub enum Fallback {
    /// The shadow had no `desired` before the run.
    NoPriorConfig,
    /// The pre-run document dialled this run's own server.
    RunsOwnServer(String),
    /// The retained bundle the pre-run document named was gone.
    BundleMissing(String),
    /// The pre-run config was not one the device could apply.
    Unusable(String),
}

impl fmt::Display for Fallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fallback::NoPriorConfig => write!(f, "there was no configuration before the run"),
            Fallback::RunsOwnServer(endpoint) => write!(
                f,
                "the pre-run configuration dialled this run's own server ({endpoint}), \
                 left there by an earlier run"
            ),
            Fallback::BundleMissing(topic) => write!(
                f,
                "the pre-run configuration's bundle on {topic} was gone, so its tags are unknown"
            ),
            Fallback::Unusable(e) => write!(
                f,
                "the pre-run configuration was not one the device could apply: {e}"
            ),
        }
    }
}

/// What the restore publishes, decided before anything is written.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Config version it goes out under.
    pub version: u32,
    /// `None` when this is the pre-run config; otherwise why it is not.
    pub fallback: Option<Fallback>,
    /// The document `desired` ends up as.
    pub desired: Value,
    /// What to send as `desired` to get there from the current document.
    pub patch: Value,
    /// Retained messages to publish before the patch; the bundle `desired`
    /// points at comes first.
    pub retained: Vec<Retained>,
}

// ---------------------------------------------------------------------------
// decisions — pure, so the tests need neither AWS nor a device
// ---------------------------------------------------------------------------

/// The RFC 7386 merge patch that turns `from` into `to`, which is how AWS
/// applies `state.desired`: objects merge key by key, `null` deletes a key,
/// anything else replaces.
pub fn merge_patch(from: &Value, to: &Value) -> Value {
    let (Value::Object(from), Value::Object(to)) = (from, to) else {
        return to.clone();
    };
    let mut patch = Map::new();
    for key in from.keys().filter(|k| !to.contains_key(*k)) {
        patch.insert(key.clone(), Value::Null);
    }
    for (key, value) in to {
        match from.get(key) {
            Some(old) if old == value => {}
            Some(old) => {
                patch.insert(key.clone(), merge_patch(old, value));
            }
            None => {
                patch.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(patch)
}

/// A version past everything seen: the run's own counter
/// ([`crate::phases::Ctx::next_version`]) and the shadow as it is now.
pub fn next_version(last_version: u32, shadow: Option<&Value>) -> u32 {
    last_version.max(shadow.map_or(0, cfg_version)) + 1
}

/// Decides what goes back.
///
/// `current` is `desired` as the run left it. `pre_run_bundle_now` is what
/// the pre-run bundle's topic holds now: the run may have overwritten it (it
/// publishes its own v1…v8) or cleared it.
pub fn plan(
    snapshot: &Snapshot,
    current: Option<&Value>,
    version: u32,
    pre_run_bundle_now: Option<&[u8]>,
) -> Plan {
    let ((desired, bundle), fallback) = match restored(snapshot, version) {
        Ok(restored) => (restored, None),
        Err(why) => {
            let (mut desired, bundle) = disabled(&snapshot.thing, version);
            // Keys the firmware does not own belong to another writer.
            if let (Some(Value::Object(pre_run)), Some(doc)) =
                (&snapshot.desired, desired.as_object_mut())
            {
                for (key, value) in pre_run {
                    doc.entry(key.clone()).or_insert_with(|| value.clone());
                }
            }
            ((desired, bundle), Some(why))
        }
    };
    let mut retained = vec![bundle];
    // Put back what the run overwrote or cleared, so the pre-run document
    // still has its bundle should anyone point the device at it again.
    if let Some(original) = &snapshot.bundle {
        if original.topic != retained[0].topic
            && pre_run_bundle_now != Some(original.payload.as_slice())
        {
            retained.push(original.clone());
        }
    }
    Plan {
        version,
        fallback,
        patch: merge_patch(current.unwrap_or(&Value::Null), &desired),
        desired,
        retained,
    }
}

/// The pre-run document and its bundle, re-issued under `version`.
fn restored(snapshot: &Snapshot, version: u32) -> Result<(Value, Retained), Fallback> {
    let unusable = |e: &dyn fmt::Display| Fallback::Unusable(e.to_string());
    let desired = snapshot.desired.as_ref().ok_or(Fallback::NoPriorConfig)?;
    let settings: DesiredSettings =
        serde_json::from_value(desired.clone()).map_err(|e| unusable(&e))?;
    if same_endpoint(&settings.instance.endpoint, &snapshot.run_endpoint) {
        return Err(Fallback::RunsOwnServer(settings.instance.endpoint));
    }
    let bundle = snapshot
        .bundle
        .as_ref()
        .ok_or_else(|| Fallback::BundleMissing(settings.cfg.topic.clone()))?;
    // A pair the device would have refused is not a config it was running;
    // re-issuing it under a fresh digest would sanction it after the fact.
    AppliedConfig::new(settings, &bundle.payload).map_err(|e| unusable(&e))?;

    // Through the firmware's own wire type: the device checks `bundle.v`
    // against `cfg.v`, and this is what it parses, in the documented order.
    let mut tags: TagBundle = serde_json::from_slice(&bundle.payload).map_err(|e| unusable(&e))?;
    tags.v = version;
    let payload = serde_json::to_vec(&tags).map_err(|e| unusable(&e))?;
    let topic = documents::bundle_topic(&snapshot.thing, version);
    let mut desired = desired.clone();
    desired["cfg"]["v"] = json!(version);
    desired["cfg"]["sha256"] = json!(sha256_hex(&payload));
    desired["cfg"]["topic"] = json!(topic);

    // And checked again exactly as the device will see it.
    let settings = serde_json::from_value(desired.clone()).map_err(|e| unusable(&e))?;
    AppliedConfig::new(settings, &payload).map_err(|e| unusable(&e))?;
    Ok((desired, Retained { topic, payload }))
}

/// A complete document with `enabled: false`, under `version`, and a bundle
/// of its own, so re-enabling it later is a new config like any other.
fn disabled(thing: &str, version: u32) -> (Value, Retained) {
    let bundle = documents::bundle(thing, version, &catalogue::tags());
    // Never resolved while disabled, so neither is the namespace.
    let mut desired = Desired::new(NO_ENDPOINT, 2);
    desired.ns_uri = None;
    desired.enabled = false;
    (
        desired.to_json(thing, &bundle),
        Retained {
            topic: bundle.topic,
            payload: bundle.payload,
        },
    )
}

/// Close enough to URL equality for telling a leftover apart: case and a
/// trailing `/` make no difference.
fn same_endpoint(a: &str, b: &str) -> bool {
    let norm = |e: &str| e.trim().trim_end_matches('/').to_ascii_lowercase();
    norm(a) == norm(b)
}

/// Whether `reported` shows the device on the restored config: idle when it
/// is disabled; otherwise synced to it, or trying to reach its server. Only
/// a successful sync writes `cfg_v`, and the pre-run server need not be
/// reachable from where the device is now.
pub fn took_up(plan: &Plan, reported: &Value) -> bool {
    let state = reported["state"].as_str().unwrap_or("");
    if !plan.desired["enabled"].as_bool().unwrap_or(true) {
        return state == "idle";
    }
    let endpoint = plan.desired["instance"]["endpoint"]
        .as_str()
        .unwrap_or_default();
    reported["cfg_v"].as_u64() == Some(plan.version as u64)
        || (matches!(state, "connecting" | "error")
            && !endpoint.is_empty()
            && reported["last_error"]
                .as_str()
                .is_some_and(|e| e.contains(endpoint)))
}

/// Bundles `--cleanup` may clear: ones the run published, less `keep`.
pub fn to_clear(published: &[String], keep: &[&str]) -> Vec<String> {
    let mut clear: Vec<String> = Vec::new();
    for topic in published {
        if !keep.contains(&topic.as_str()) && !clear.contains(topic) {
            clear.push(topic.clone());
        }
    }
    clear
}

fn desired_of(shadow: Option<&Value>) -> Option<Value> {
    shadow
        .map(|doc| doc["state"]["desired"].clone())
        .filter(Value::is_object)
}

/// `opc.tcp://…, 14 tags, enabled`
fn summary_of(desired: &Value) -> String {
    let enabled = desired["enabled"].as_bool().unwrap_or(true);
    format!(
        "{}, {} tags, {}",
        desired["instance"]["endpoint"]
            .as_str()
            .unwrap_or("no endpoint"),
        desired["cfg"]["n"],
        if enabled { "enabled" } else { "disabled" }
    )
}

fn reported_summary(r: &Value) -> String {
    format!(
        "state={} cfg_v={} err={}",
        r["state"],
        r["cfg_v"],
        truncate(r["last_error"].as_str().unwrap_or("null"), 120)
    )
}

impl Snapshot {
    /// What the run found, and what the restore will make of it.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = vec![match &self.desired {
            Some(desired) => format!(
                "found config v{}: {}",
                desired["cfg"]["v"],
                summary_of(desired)
            ),
            None => "found no configuration in the opcua shadow".into(),
        }];
        if let Some(bundle) = &self.bundle {
            lines.push(format!(
                "saved its bundle {} ({} B)",
                bundle.topic,
                bundle.payload.len()
            ));
        }
        match plan(self, None, self.last_version + 1, None).fallback {
            None => {
                lines.push("the restore puts it back as found, under a new version".into());
                let ns_uri = self
                    .desired
                    .as_ref()
                    .and_then(|d| d["instance"]["ns_uri"].as_str());
                if ns_uri == Some(NAMESPACE_URI) {
                    lines.push(format!(
                        "note: it uses the test namespace {NAMESPACE_URI}, so it looks left over \
                         from an earlier gateway-hil run; the device goes back to it all the same"
                    ));
                }
            }
            Some(why) => lines.push(format!("the restore leaves the device disabled: {why}")),
        }
        lines
    }

    pub fn to_json(&self) -> Value {
        json!({
            "thing": self.thing,
            "taken_ms": self.taken_ms,
            "run_endpoint": self.run_endpoint,
            "last_version": self.last_version,
            "desired": self.desired,
            // A bundle is JSON, so text; one that is not could not have
            // been applied anyway, and a lossy copy fails the same checks.
            "bundle": self.bundle.as_ref().map(|b| json!({
                "topic": b.topic,
                "payload": String::from_utf8_lossy(&b.payload),
            })),
        })
    }

    pub fn from_json(doc: &Value) -> Result<Self> {
        let text = |key: &str| {
            doc[key]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("no {key:?}"))
        };
        let bundle = match &doc["bundle"] {
            Value::Null => None,
            b => Some(Retained {
                topic: b["topic"]
                    .as_str()
                    .ok_or_else(|| anyhow!("bundle without a topic"))?
                    .to_string(),
                payload: b["payload"]
                    .as_str()
                    .ok_or_else(|| anyhow!("bundle without a payload"))?
                    .as_bytes()
                    .to_vec(),
            }),
        };
        Ok(Self {
            thing: text("thing")?,
            taken_ms: doc["taken_ms"].as_i64().unwrap_or(0),
            run_endpoint: text("run_endpoint")?,
            last_version: doc["last_version"].as_u64().unwrap_or(0) as u32,
            desired: Some(doc["desired"].clone()).filter(Value::is_object),
            bundle,
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(&self.to_json())?)
            .with_context(|| format!("saving the snapshot to {}", path.display()))
    }

    /// The snapshot a run left behind, if one did.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let doc: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not JSON", path.display()))?;
        Self::from_json(&doc)
            .map(Some)
            .with_context(|| format!("{} is not a snapshot", path.display()))
    }
}

/// Where a run keeps its snapshot until the restore has gone through. One
/// per thing, so the next run on it finds a restore that never happened.
pub fn path(artifacts: &Path, thing: &str) -> PathBuf {
    let name: String = thing
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    artifacts.join(format!("restore-{name}.json"))
}

// ---------------------------------------------------------------------------
// the planes
// ---------------------------------------------------------------------------

/// The two configuration planes, as far as the restore needs them: [`Cloud`]
/// in a run, a fake in the tests.
pub(crate) trait Planes {
    async fn read_shadow(&self) -> Result<Option<Value>>;
    async fn read_retained(&self, topic: &str) -> Result<Option<Vec<u8>>>;
    /// An empty payload clears the topic.
    async fn write_retained(&self, topic: &str, payload: &[u8]) -> Result<()>;
    async fn write_desired(&self, desired: &Value) -> Result<()>;
}

impl Planes for Cloud {
    async fn read_shadow(&self) -> Result<Option<Value>> {
        self.get_shadow().await
    }

    async fn read_retained(&self, topic: &str) -> Result<Option<Vec<u8>>> {
        self.get_retained(topic).await
    }

    async fn write_retained(&self, topic: &str, payload: &[u8]) -> Result<()> {
        self.publish_retained(topic, payload.to_vec()).await
    }

    async fn write_desired(&self, desired: &Value) -> Result<()> {
        self.update_desired(desired).await
    }
}

/// Reads what the run is about to change. Fails rather than guesses: a run
/// that cannot say what it found cannot put it back.
pub(crate) async fn take(
    planes: &impl Planes,
    thing: &str,
    run_endpoint: &str,
) -> Result<Snapshot> {
    let shadow = planes
        .read_shadow()
        .await
        .context("reading the opcua shadow")?;
    let desired = desired_of(shadow.as_ref());
    let mut bundle = None;
    if let Some(topic) = desired.as_ref().and_then(|d| d["cfg"]["topic"].as_str()) {
        bundle = planes
            .read_retained(topic)
            .await
            .with_context(|| format!("reading the retained bundle on {topic}"))?
            .map(|payload| Retained {
                topic: topic.to_string(),
                payload,
            });
    }
    Ok(Snapshot {
        thing: thing.to_string(),
        taken_ms: now_ms(),
        run_endpoint: run_endpoint.to_string(),
        last_version: shadow.as_ref().map_or(0, cfg_version),
        desired,
        bundle,
    })
}

/// Plans against the shadow as it is now and publishes: the bundles first,
/// so the device finds them the moment the patch sends it looking. Every
/// topic written is added to `published`.
pub(crate) async fn apply(
    planes: &impl Planes,
    snapshot: &Snapshot,
    last_version: u32,
    published: &mut Vec<String>,
) -> Result<Plan> {
    let shadow = planes
        .read_shadow()
        .await
        .context("reading the opcua shadow")?;
    let pre_run_bundle_now = match &snapshot.bundle {
        // Unreadable counts as changed: republishing the same bytes is
        // harmless, and not republishing lost ones is not.
        Some(bundle) => planes.read_retained(&bundle.topic).await.unwrap_or(None),
        None => None,
    };
    let plan = plan(
        snapshot,
        desired_of(shadow.as_ref()).as_ref(),
        next_version(last_version, shadow.as_ref()),
        pre_run_bundle_now.as_deref(),
    );
    for retained in &plan.retained {
        published.push(retained.topic.clone());
        planes
            .write_retained(&retained.topic, &retained.payload)
            .await
            .with_context(|| format!("publishing the bundle on {}", retained.topic))?;
    }
    planes
        .write_desired(&plan.patch)
        .await
        .context("updating the opcua shadow's desired")?;
    Ok(plan)
}

/// Clears what [`to_clear`] allows. What `desired` points at is read back
/// rather than assumed, so a restore that failed halfway still keeps the
/// bundle the device runs on. Returns the topics cleared and those kept.
pub(crate) async fn cleanup(
    planes: &impl Planes,
    snapshot: &Snapshot,
    published: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    let shadow = planes
        .read_shadow()
        .await
        .context("reading back the opcua shadow")?;
    let desired = desired_of(shadow.as_ref());
    let mut keep: Vec<&str> = Vec::new();
    let candidates = [
        desired.as_ref().and_then(|d| d["cfg"]["topic"].as_str()),
        snapshot.bundle.as_ref().map(|b| b.topic.as_str()),
    ];
    for topic in candidates.into_iter().flatten() {
        if !keep.contains(&topic) {
            keep.push(topic);
        }
    }
    let mut cleared = Vec::new();
    for topic in to_clear(published, &keep) {
        match planes.write_retained(&topic, &[]).await {
            Ok(()) => cleared.push(topic),
            Err(e) => log::warn!("could not clear {topic}: {e:#}"),
        }
    }
    let kept = keep.iter().map(|t| t.to_string()).collect();
    Ok((cleared, kept))
}

// ---------------------------------------------------------------------------
// the run's last step
// ---------------------------------------------------------------------------

/// How the restore went, for the console and `summary.json`.
pub struct Outcome {
    /// Counted like a phase, so a failed restore fails the run.
    pub result: PhaseResult,
    /// One line for the run summary.
    pub line: String,
    pub json: Value,
    /// The shadow holds what it should, so the snapshot file can go.
    pub settled: bool,
}

/// Restores the pre-run configuration and checks that the device took it up.
///
/// `changed` is false when the run published nothing; then nothing is
/// touched. After Ctrl-C the device is not waited for: whoever pressed it
/// does not want to sit through another three minutes.
pub async fn finish(
    cloud: &mut Cloud,
    snapshot: &Snapshot,
    last_version: u32,
    published: &mut Vec<String>,
    changed: bool,
) -> Outcome {
    banner("RESTORE");
    let mut result = PhaseResult::new("restore");
    let record =
        |action: &str, plan: Option<&Plan>, confirmed: Option<bool>, error: Option<&str>| {
            json!({
                "action": action,
                "reason": plan.and_then(|p| p.fallback.as_ref()).map(Fallback::to_string),
                "version": plan.map(|p| p.version),
                "desired": plan.map(|p| &p.desired),
                "bundle_topic": plan.map(|p| &p.retained[0].topic),
                "confirmed": confirmed,
                "error": error,
                "pre_run": {
                    "desired": snapshot.desired,
                    "bundle_topic": snapshot.bundle.as_ref().map(|b| &b.topic),
                    "bundle_bytes": snapshot.bundle.as_ref().map(|b| b.payload.len()),
                },
            })
        };

    if !changed {
        info("the run changed no configuration, so there is nothing to put back");
        result.skipped = true;
        result.note = "the run changed no configuration".into();
        return Outcome {
            result,
            line: "nothing to do: the run changed no configuration".into(),
            json: record("none", None, None, None),
            settled: true,
        };
    }

    let since_ms = now_ms();
    let plan = match apply(&*cloud, snapshot, last_version, published).await {
        Ok(plan) => plan,
        Err(e) => {
            let error = format!("{e:#}");
            result.check(
                "the pre-run configuration is back in the shadow",
                false,
                error.clone(),
            );
            return Outcome {
                result,
                line: format!("FAILED: {error}"),
                json: record("failed", None, None, Some(&error)),
                settled: false,
            };
        }
    };

    let what = format!("v{}: {}", plan.version, summary_of(&plan.desired));
    let action = match &plan.fallback {
        None => {
            result.check(
                "the pre-run configuration is back in the shadow",
                true,
                &what,
            );
            "restored"
        }
        Some(why) => {
            if let Some(pre_run) = &snapshot.desired {
                // Its only other copy is the snapshot file, which goes now.
                log::warn!(
                    "the pre-run configuration is NOT restored, because {why}. It was: {pre_run}"
                );
            }
            result.check("the device is left disabled in the shadow", true, &what);
            "disabled"
        }
    };

    let confirmed = if interrupt::caught() {
        None
    } else {
        tokio::select! {
            confirmed = confirm(cloud, &plan, since_ms) => Some(confirmed),
            () = interrupt::wait() => None,
        }
    };
    let took = match &confirmed {
        Some((ok, reported)) => {
            let mut detail = reported_summary(reported);
            if !ok {
                if let Some(stale) = cloud.stale_note() {
                    detail = format!("{stale} (last reported: {detail})");
                }
            }
            result.check("the device took it up", *ok, detail);
            if *ok {
                "the device took it up"
            } else {
                "the device did NOT report taking it up"
            }
        }
        None => {
            info("not waiting for the device to take it up: the run was interrupted");
            "not waited for (interrupted)"
        }
    };
    let line = match &plan.fallback {
        None => format!("pre-run configuration, as {what}; {took}"),
        Some(why) => format!(
            "device left DISABLED as v{}, because {why}; {took}",
            plan.version
        ),
    };
    Outcome {
        result,
        line,
        json: record(action, Some(&plan), confirmed.map(|(ok, _)| ok), None),
        settled: true,
    }
}

async fn confirm(cloud: &mut Cloud, plan: &Plan, since_ms: i64) -> (bool, Value) {
    info("waiting for the device to take the restored configuration up…");
    // Only a report written after the restore says anything about it.
    cloud.require_fresh_since(since_ms);
    cloud
        .wait_for_reported(
            |r| took_up(plan, r),
            CONFIRM_TIMEOUT,
            show(reported_summary),
        )
        .await
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;

    const THING: &str = "T";
    const RUN: &str = "opc.tcp://192.168.50.28:4855/ergousha/test";

    /// The shadow and the retained topics, with AWS's merge semantics.
    #[derive(Default)]
    struct Fake {
        desired: RefCell<Value>,
        reported: RefCell<Value>,
        retained: RefCell<BTreeMap<String, Vec<u8>>>,
    }

    /// Applies an RFC 7386 merge patch, as `UpdateThingShadow` does.
    fn merge(target: &mut Value, patch: &Value) {
        let Value::Object(patch) = patch else {
            *target = patch.clone();
            return;
        };
        if !target.is_object() {
            *target = json!({});
        }
        let target = target.as_object_mut().expect("just made an object");
        for (key, value) in patch {
            if value.is_null() {
                target.remove(key);
            } else {
                merge(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
    }

    impl Planes for Fake {
        async fn read_shadow(&self) -> Result<Option<Value>> {
            let (desired, reported) = (self.desired.borrow(), self.reported.borrow());
            if desired.is_null() && reported.is_null() {
                return Ok(None);
            }
            Ok(Some(
                json!({ "state": { "desired": *desired, "reported": *reported } }),
            ))
        }

        async fn read_retained(&self, topic: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.retained.borrow().get(topic).cloned())
        }

        async fn write_retained(&self, topic: &str, payload: &[u8]) -> Result<()> {
            let mut retained = self.retained.borrow_mut();
            if payload.is_empty() {
                retained.remove(topic);
            } else {
                retained.insert(topic.to_string(), payload.to_vec());
            }
            Ok(())
        }

        async fn write_desired(&self, desired: &Value) -> Result<()> {
            merge(&mut self.desired.borrow_mut(), desired);
            Ok(())
        }
    }

    impl Fake {
        fn topics(&self) -> Vec<String> {
            self.retained.borrow().keys().cloned().collect()
        }

        /// What `Ctx::publish_config` does, plus whatever a phase leaves
        /// behind in the document.
        async fn publish(&self, version: u32, tweak: impl FnOnce(&mut Desired)) -> String {
            let bundle = documents::bundle(THING, version, &catalogue::tags());
            let mut desired = Desired::new(RUN, 2);
            tweak(&mut desired);
            self.write_retained(&bundle.topic, &bundle.payload)
                .await
                .unwrap();
            self.write_desired(&desired.to_json(THING, &bundle))
                .await
                .unwrap();
            *self.reported.borrow_mut() = json!({ "state": "running", "cfg_v": version });
            bundle.topic
        }

        /// The online scenario's publishes, v1 to v8, including the two it
        /// clears again.
        async fn run_the_scenario(&self) -> Vec<String> {
            let mut published = Vec::new();
            for v in 1..=8 {
                let topic = self
                    .publish(v, |d| match v {
                        3 => d.ns = 99,
                        4 => d.sec_policy = "Basic256Sha256".into(),
                        5 => d.sha256_override = Some("0".repeat(64)),
                        7 => d.enabled = false,
                        _ => {}
                    })
                    .await;
                if v == 4 || v == 5 {
                    self.write_retained(&topic, &[]).await.unwrap();
                }
                published.push(topic);
            }
            published
        }
    }

    /// A production config unlike the harness's own: another server and
    /// namespace, no `ns_uri`, a numeric-id group, a deadband, and a field
    /// the firmware does not own.
    fn pre_run_bundle() -> Vec<u8> {
        br#"{"v":8,"g":[{"r":500,"d":0.5,"a":["Boiler.Temp","Boiler.Pressure"]},{"r":2000,"i":"i","a":["4711"]}]}"#
            .to_vec()
    }

    fn pre_run_desired() -> Value {
        json!({
            "enabled": true,
            "instance": {
                "endpoint": "opc.tcp://plc.local:4840",
                "ns": 3,
                "sec_mode": "None",
                "sec_policy": "None",
                "session_timeout_ms": 30000,
                "keepalive_ms": 5000,
                "publish_ms": 500,
            },
            "telemetry": {
                "topic": "dt/T/opcua",
                "qos": 0,
                "batch_max_items": 50,
                "batch_max_bytes": 8192,
                "batch_max_age_ms": 500,
            },
            "cfg": {
                "v": 8,
                "n": 3,
                "sha256": sha256_hex(&pre_run_bundle()),
                "topic": "cmd/T/opcua/tags/v8",
            },
            "owner": "ops",
        })
    }

    fn configured() -> Fake {
        let fake = Fake::default();
        *fake.desired.borrow_mut() = pre_run_desired();
        *fake.reported.borrow_mut() = json!({ "state": "connecting", "cfg_v": 8 });
        fake.retained
            .borrow_mut()
            .insert("cmd/T/opcua/tags/v8".into(), pre_run_bundle());
        fake
    }

    fn without_cfg(doc: &Value) -> Value {
        let mut doc = doc.clone();
        doc.as_object_mut().unwrap().remove("cfg");
        doc
    }

    fn applies(desired: &Value, bundle: &[u8]) -> bool {
        let settings = serde_json::from_value(desired.clone()).unwrap();
        AppliedConfig::new(settings, bundle).is_ok()
    }

    fn snapshot_of(desired: Option<Value>, bundle: Option<Vec<u8>>) -> Snapshot {
        Snapshot {
            thing: THING.into(),
            taken_ms: 0,
            run_endpoint: RUN.into(),
            last_version: 8,
            bundle: desired.as_ref().zip(bundle).map(|(d, payload)| Retained {
                topic: d["cfg"]["topic"].as_str().unwrap().into(),
                payload,
            }),
            desired,
        }
    }

    #[test]
    fn a_merge_patch_turns_one_document_into_the_other() {
        let from = json!({"a": 1, "b": {"x": 1, "y": 2}, "c": [1, 2], "gone": true});
        let to = json!({"a": 1, "b": {"x": 5}, "c": [3], "new": {"z": 1}});
        let patch = merge_patch(&from, &to);
        assert_eq!(
            patch,
            json!({"gone": null, "b": {"x": 5, "y": null}, "c": [3], "new": {"z": 1}})
        );
        let mut merged = from.clone();
        merge(&mut merged, &patch);
        assert_eq!(merged, to);
        // From nothing, the patch is the document.
        assert_eq!(merge_patch(&Value::Null, &to), to);
    }

    #[test]
    fn the_restore_version_passes_everything_seen() {
        let shadow = json!({"state": {"desired": {"cfg": {"v": 12}}, "reported": {"cfg_v": 10}}});
        assert_eq!(next_version(8, Some(&shadow)), 13);
        assert_eq!(next_version(15, Some(&shadow)), 16);
        assert_eq!(next_version(0, None), 1);
    }

    /// Acceptance: after a run, `desired` equals the pre-run document apart
    /// from the version, and the device can apply it.
    #[tokio::test]
    async fn a_run_is_undone_apart_from_the_version() {
        let fake = configured();
        let snapshot = take(&fake, THING, RUN).await.unwrap();
        assert_eq!(snapshot.last_version, 8);
        let mut published = fake.run_the_scenario().await;
        // The run overwrote the pre-run bundle with its own v8.
        assert_ne!(
            fake.retained.borrow()["cmd/T/opcua/tags/v8"],
            pre_run_bundle()
        );

        let plan = apply(&fake, &snapshot, 8, &mut published).await.unwrap();
        assert_eq!(plan.fallback, None);
        assert_eq!(plan.version, 9);

        let desired = fake.desired.borrow().clone();
        // `ns_uri` and `id_type` came with the run and must not stay.
        assert_eq!(without_cfg(&desired), without_cfg(&pre_run_desired()));
        assert_eq!(desired["cfg"]["v"], 9);
        assert_eq!(desired["cfg"]["n"], 3);
        assert_eq!(desired["cfg"]["topic"], "cmd/T/opcua/tags/v9");

        let bundle = fake.retained.borrow()["cmd/T/opcua/tags/v9"].clone();
        assert_eq!(desired["cfg"]["sha256"], sha256_hex(&bundle));
        // The same tags, byte for byte, apart from the version.
        assert_eq!(
            String::from_utf8(bundle.clone()).unwrap(),
            String::from_utf8(pre_run_bundle())
                .unwrap()
                .replace(r#""v":8"#, r#""v":9"#)
        );
        assert!(applies(&desired, &bundle));
        // And the pre-run bundle is back on its own topic.
        assert_eq!(
            fake.retained.borrow()["cmd/T/opcua/tags/v8"],
            pre_run_bundle()
        );
    }

    /// Acceptance: a device with no prior config is left disabled — `idle` —
    /// by a document it will accept, naming no real host.
    #[tokio::test]
    async fn a_device_without_prior_config_is_left_disabled() {
        let fake = Fake::default();
        let snapshot = take(&fake, THING, RUN).await.unwrap();
        assert_eq!(snapshot.desired, None);
        let mut published = vec![fake.publish(1, |_| {}).await];

        let plan = apply(&fake, &snapshot, 1, &mut published).await.unwrap();
        assert_eq!(plan.fallback, Some(Fallback::NoPriorConfig));
        let desired = fake.desired.borrow().clone();
        assert_eq!(desired["enabled"], false);
        assert_eq!(desired["cfg"]["v"], 2);
        assert!(!desired.to_string().contains("192.168.50.28"));
        let bundle = fake.retained.borrow()["cmd/T/opcua/tags/v2"].clone();
        assert!(applies(&desired, &bundle));

        assert!(took_up(&plan, &json!({"state": "idle"})));
        assert!(!took_up(&plan, &json!({"state": "running", "cfg_v": 2})));
    }

    /// Acceptance: `--cleanup` keeps the bundle `desired` points at, and the
    /// pre-run one; everything else the run published goes.
    #[tokio::test]
    async fn cleanup_keeps_the_bundles_the_device_needs() {
        let fake = configured();
        let snapshot = take(&fake, THING, RUN).await.unwrap();
        let mut published = fake.run_the_scenario().await;
        apply(&fake, &snapshot, 8, &mut published).await.unwrap();

        let (cleared, kept) = cleanup(&fake, &snapshot, &published).await.unwrap();
        assert_eq!(kept, ["cmd/T/opcua/tags/v9", "cmd/T/opcua/tags/v8"]);
        assert_eq!(cleared.len(), 7, "v1-v7: {cleared:?}");
        assert_eq!(
            fake.topics(),
            ["cmd/T/opcua/tags/v8", "cmd/T/opcua/tags/v9"]
        );
        assert_eq!(
            fake.retained.borrow()["cmd/T/opcua/tags/v8"],
            pre_run_bundle()
        );
    }

    #[tokio::test]
    async fn cleanup_after_a_failed_restore_keeps_what_the_device_runs_on() {
        let fake = configured();
        let snapshot = take(&fake, THING, RUN).await.unwrap();
        let published = fake.run_the_scenario().await;
        // No restore: desired still points at the run's v8.
        let (cleared, kept) = cleanup(&fake, &snapshot, &published).await.unwrap();
        assert_eq!(kept, ["cmd/T/opcua/tags/v8"]);
        assert!(!cleared.contains(&"cmd/T/opcua/tags/v8".to_string()));
        assert_eq!(fake.topics(), ["cmd/T/opcua/tags/v8"]);
    }

    #[test]
    fn to_clear_skips_kept_and_repeated_topics() {
        let published = ["a", "b", "a", "c"].map(String::from);
        assert_eq!(to_clear(&published, &["b"]), ["a", "c"]);
    }

    #[test]
    fn a_leftover_pointing_at_this_runs_server_is_not_restored() {
        let bundle = documents::bundle(THING, 8, &catalogue::tags());
        // Spelled differently, but the same server.
        let leftover = Desired::new(format!("{}/", RUN.to_uppercase()), 2).to_json(THING, &bundle);
        let snapshot = snapshot_of(Some(leftover), Some(bundle.payload));
        let plan = plan(&snapshot, None, 9, None);
        assert!(matches!(plan.fallback, Some(Fallback::RunsOwnServer(_))));
        assert_eq!(plan.desired["enabled"], false);
        assert_eq!(plan.desired["instance"]["endpoint"], NO_ENDPOINT);
    }

    #[test]
    fn a_missing_pre_run_bundle_leaves_the_device_disabled() {
        let snapshot = snapshot_of(Some(pre_run_desired()), None);
        let plan = plan(&snapshot, Some(&pre_run_desired()), 9, None);
        assert_eq!(
            plan.fallback,
            Some(Fallback::BundleMissing("cmd/T/opcua/tags/v8".into()))
        );
        assert_eq!(plan.desired["enabled"], false);
        assert_eq!(plan.desired["instance"]["endpoint"], NO_ENDPOINT);
        assert_eq!(
            plan.desired["owner"], "ops",
            "another writer's field survives"
        );
        assert_eq!(plan.retained.len(), 1);
    }

    #[test]
    fn a_config_the_device_refused_is_not_sanctioned_by_the_restore() {
        let mut refused = pre_run_desired();
        refused["cfg"]["sha256"] = json!("0".repeat(64));
        let plan_for = |desired: Value| {
            plan(
                &snapshot_of(Some(desired), Some(pre_run_bundle())),
                None,
                9,
                None,
            )
        };
        assert!(matches!(
            plan_for(refused).fallback,
            Some(Fallback::Unusable(e)) if e.contains("sha256")
        ));

        let mut secured = pre_run_desired();
        secured["instance"]["sec_policy"] = json!("Basic256Sha256");
        assert!(matches!(
            plan_for(secured).fallback,
            Some(Fallback::Unusable(e)) if e.contains("Basic256Sha256")
        ));
    }

    #[test]
    fn a_disabled_pre_run_config_stays_disabled() {
        let mut off = pre_run_desired();
        off["enabled"] = json!(false);
        let plan = plan(
            &snapshot_of(Some(off), Some(pre_run_bundle())),
            None,
            9,
            None,
        );
        assert_eq!(plan.fallback, None);
        assert_eq!(plan.desired["enabled"], false);
        assert!(took_up(&plan, &json!({"state": "idle"})));
    }

    #[test]
    fn an_untouched_pre_run_bundle_is_not_republished() {
        let snapshot = snapshot_of(Some(pre_run_desired()), Some(pre_run_bundle()));
        let plan = plan(&snapshot, None, 9, Some(&pre_run_bundle()));
        let topics: Vec<&str> = plan.retained.iter().map(|r| r.topic.as_str()).collect();
        assert_eq!(topics, ["cmd/T/opcua/tags/v9"]);
    }

    #[test]
    fn the_device_takes_a_restore_up_by_syncing_or_by_trying_its_server() {
        let snapshot = snapshot_of(Some(pre_run_desired()), Some(pre_run_bundle()));
        let plan = plan(&snapshot, None, 9, None);
        let error = |e: &str| json!({"state": "error", "cfg_v": 8, "last_error": e});
        assert!(took_up(&plan, &json!({"state": "running", "cfg_v": 9})));
        assert!(took_up(
            &plan,
            &error("could not connect to opc.tcp://plc.local:4840: refused")
        ));
        assert!(!took_up(&plan, &json!({"state": "running", "cfg_v": 8})));
        assert!(!took_up(
            &plan,
            &error(&format!("could not connect to {RUN}"))
        ));
    }

    #[test]
    fn a_snapshot_survives_its_file() {
        let snapshot = Snapshot {
            taken_ms: 1_758_000_000_000,
            ..snapshot_of(Some(pre_run_desired()), Some(pre_run_bundle()))
        };
        assert_eq!(Snapshot::from_json(&snapshot.to_json()).unwrap(), snapshot);
        let empty = snapshot_of(None, None);
        assert_eq!(Snapshot::from_json(&empty.to_json()).unwrap(), empty);
        assert!(Snapshot::load(Path::new("/nonexistent/restore-T.json"))
            .unwrap()
            .is_none());
        assert_eq!(
            path(Path::new("a"), "thing:1"),
            Path::new("a").join("restore-thing_1.json")
        );
    }
}
