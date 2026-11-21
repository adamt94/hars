//! LIFX's config flow, as HA 2026.6.1's `components/lifx/config_flow.py`
//! with aiolifx 1.2.1: `user` (a bulb's address, or nothing to look for
//! bulbs) and `pick_device` (the bulbs a broadcast found). Before an entry
//! is made the bulb is asked for its firmware, version, label and group,
//! with HA's retries (5 tries in 15 s) over aiolifx's own (each message
//! sent 3 times, 0.5 s apart).
//!
//! Where `hars` differs from HA, on purpose:
//! - A product `hars` doesn't run as HA does ([`super::runs`]: multi-zone
//!   strips, tiles and ceilings, the Clean's HEV, Night Vision's infrared)
//!   ends the flow (`unsupported_device`) where HA makes an entry. A LIFX
//!   Switch ends it with `cannot_connect`, as in HA.
//! - No flow starts by itself: HA's DHCP, HomeKit and broadcast discovery
//!   flows, and its migration of the old shared entry, don't exist (no
//!   listener runs while no flow is open).
//!
//! The broadcast runs only within the step that asks for it (HA's 8.5 s),
//! on a socket that is closed when the step answers.

use super::conn::{Conn, Op, Resend};
use super::proto::{self, ty};
use super::{DOMAIN, features, products, runs};
use crate::flow::{Cx, Field, Flow, Handler, Kind, Step, StepFuture};
use serde_json::{Map, Value};
use std::time::Duration;

/// `LifXConfigFlow`.
pub static HANDLER: Handler = Handler {
    domain: DOMAIN,
    class: "LifXConfigFlow",
    version: 1,
    minor_version: 1,
    steps: &["user", "pick_device"],
    new: || Box::new(LifxFlow::default()),
};

/// The flow's waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// How aiolifx sends each message.
    pub resend: Resend,
    /// One of HA's 5 tries (`OVERALL_TIMEOUT / DEFAULT_ATTEMPTS`).
    pub per_try: Duration,
    /// How long a broadcast is listened to (`DEFAULT_TIMEOUT`).
    pub discovery: Duration,
}

impl Timing {
    pub const HA: Timing = Timing {
        resend: Resend::LIBRARY,
        per_try: Duration::from_secs(3),
        discovery: Duration::from_millis(8500),
    };
}

/// HA's `DEFAULT_ATTEMPTS`.
const ATTEMPTS: u32 = 5;

#[derive(Default)]
struct LifxFlow {
    /// The bulbs the broadcast found: (serial, address).
    discovered: Vec<(String, String)>,
}

/// A bulb that answered.
struct Device {
    ip: String,
    label: String,
    product: Option<u32>,
}

/// `_async_try_connect`: the bulb at `host` and what it says it is, or
/// `None` where HA's has nothing (no answer, a name that can't be found, a
/// LIFX Switch). `Err` is an abort (an entry has that address).
async fn try_connect(cx: &mut Cx<'_>, host: &str) -> Result<Option<Device>, Step> {
    if cx.entry_with_data("host", host) {
        return Err(Step::abort("already_configured"));
    }
    let timing = cx.endpoints.lifx.flow;
    let (ip, port) = cx.endpoints.lifx.addr(host);
    let Ok(conn) = Conn::open_with(&ip, port, timing.resend).await else {
        return Ok(None);
    };
    // get_hostfirmware for the MAC address, get_version for the features,
    // get_label for the title, get_group for the suggested area.
    let ops = [
        Op::get(ty::GET_HOST_FIRMWARE, ty::STATE_HOST_FIRMWARE),
        Op::get(ty::GET_VERSION, ty::STATE_VERSION),
        Op::get(ty::GET_LABEL, ty::STATE_LABEL),
        Op::get(ty::GET_GROUP, ty::STATE_GROUP),
    ];
    let replies = conn.execute(&ops, ATTEMPTS, timing.per_try).await;
    drop(conn);
    let Ok(replies) = replies else {
        return Ok(None);
    };
    let firmware = proto::host_firmware(&replies[0].1);
    let product = proto::product(&replies[1].1);
    let label = proto::state_label(&replies[2].1).unwrap_or_default();
    // Relays (a LIFX Switch) aren't supported by HA.
    if firmware.is_none() || features(product).flags & products::RELAYS != 0 {
        return Ok(None);
    }
    cx.set_unique_id(&proto::mac(&replies[0].0.target));
    Ok(Some(Device {
        ip: host.to_owned(),
        label,
        product,
    }))
}

/// `_async_create_entry_from_device`.
fn create_entry(cx: &mut Cx<'_>, device: Device) -> Step {
    if let Some(abort) = cx.abort_if_unique_id_configured_updating("host", &device.ip) {
        return abort;
    }
    if !runs(features(device.product)) {
        tracing::warn!(
            "lifx: {} is a {}, which hars doesn't run yet",
            device.label,
            super::model_name(device.product)
        );
        return Step::abort("unsupported_device");
    }
    let mut data = Map::new();
    data.insert("host".into(), Value::String(device.ip));
    Step::CreateEntry {
        title: device.label,
        data,
    }
}

impl LifxFlow {
    fn user_form(error: Option<&'static str>) -> Step {
        Step::form("user", vec![Field::optional_str("host", "")], error)
    }

    async fn user(&mut self, cx: &mut Cx<'_>, input: Option<Map<String, Value>>) -> Step {
        let Some(input) = input else {
            return Self::user_form(None);
        };
        let host = input
            .get("host")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if host.is_empty() {
            return self.pick_device(cx, None).await;
        }
        match try_connect(cx, host).await {
            Err(abort) => abort,
            Ok(None) => Self::user_form(Some("cannot_connect")),
            Ok(Some(device)) => create_entry(cx, device),
        }
    }

    async fn pick_device(&mut self, cx: &mut Cx<'_>, input: Option<Map<String, Value>>) -> Step {
        if let Some(input) = input {
            let serial = input
                .get("device")
                .and_then(Value::as_str)
                .unwrap_or_default();
            cx.set_unique_id(serial);
            let ip = self
                .discovered
                .iter()
                .find(|(s, _)| s == serial)
                .map(|(_, ip)| ip.clone())
                .unwrap_or_default();
            return match try_connect(cx, &ip).await {
                Err(abort) => abort,
                Ok(None) => Step::abort("cannot_connect"),
                Ok(Some(device)) => create_entry(cx, device),
            };
        }
        // What is set up already, but for the old shared entry.
        let mut serials = Vec::new();
        let mut hosts = Vec::new();
        for (unique_id, host) in cx.current_entries("host") {
            if let Some(unique_id) = unique_id.filter(|u| u != DOMAIN) {
                serials.push(unique_id);
                hosts.extend(host);
            }
        }
        self.discovered =
            super::scan(&cx.endpoints.lifx, cx.endpoints.lifx.flow.discovery, None).await;
        let options: Vec<(String, String)> = self
            .discovered
            .iter()
            .filter(|(serial, ip)| !serials.contains(serial) && !hosts.contains(ip))
            .map(|(serial, ip)| (serial.clone(), format!("{serial} ({ip})")))
            .collect();
        if options.is_empty() {
            return Step::abort("no_devices_found");
        }
        Step::Form {
            step_id: "pick_device",
            schema: Some(vec![Field::required("device", Kind::Select(options))]),
            errors: None,
            description_placeholders: None,
        }
    }
}

impl Flow for LifxFlow {
    fn step<'a>(
        &'a mut self,
        cx: &'a mut Cx<'_>,
        step_id: &'a str,
        input: Option<Map<String, Value>>,
    ) -> StepFuture<'a> {
        Box::pin(async move {
            match step_id {
                "pick_device" => self.pick_device(cx, input).await,
                _ => self.user(cx, input).await,
            }
        })
    }
}
