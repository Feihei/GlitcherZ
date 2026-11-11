//! Node-graph commands (F-039; spec `.agents/spec/command-vocabulary.md`).
//!
//! `graph.set_param` addresses a parameter by its stable snake_case key plus
//! the document node id; `graph.query` lists what a node exposes, so an agent
//! can discover the vocabulary instead of hard-coding it.

use glitcherz_model::{
    ChannelParam, ModalParams, NodeDoc, NodeParam, OrganParams, PluckParams, VoiceSpec,
    WavetableParams, node_param_keys, node_param_range, node_param_value, set_node_param,
};
use serde_json::{Value, json};

use crate::registry::{always, i64_or};
use crate::{Session, bad_params};

/// Every graph command.
pub fn specs() -> Vec<crate::CommandSpec> {
    vec![
        crate::cmd!(
            "graph.set_param",
            "Set Node Parameter",
            ["Event", "Patch"],
            None,
            "{track?, node, param, value, index?, channel?, channel_param?} → {node, param, value}",
            always,
            set_param
        ),
        crate::cmd!(
            query "graph.query",
            "Query Graph",
            [],
            None,
            "{track?} → {nodes: [{node, kind, params}]}",
            always,
            query
        ),
        crate::cmd!(
            "graph.set_voice",
            "Set Instrument Voice",
            ["Event", "Patch"],
            None,
            "{node, voice: builtin|modal|pluck|organ|wavetable}",
            always,
            set_voice
        ),
    ]
}

/// Mutable node by its document id (node ids are unique across the graph).
fn node_mut<'a>(
    session: &'a mut Session,
    node_id: u64,
    command: &str,
) -> crate::Result<&'a mut NodeDoc> {
    session
        .project_mut()
        .graph
        .nodes
        .iter_mut()
        .find(|node| node.node_id() == node_id)
        .ok_or_else(|| bad_params(command, format!("no node {node_id}")))
}

/// Parses `param` (+ `index` / `channel` / `channel_param`) into the model
/// address. `effect_slot` / `organ_bar` are index-addressed,
/// `mixer_channel` is channel-addressed, everything else is a plain key.
fn parse_param(key: &str, params: &Value) -> crate::Result<NodeParam> {
    if key == "effect_slot" || key == "organ_bar" {
        let index = i64_or(params, "index", -1);
        if !(0..=255).contains(&index) {
            return Err(bad_params(
                "graph.set_param",
                format!("`index` is required for `{key}`"),
            ));
        }
        return NodeParam::from_key_indexed(key, index as u8)
            .ok_or_else(|| bad_params("graph.set_param", format!("invalid index for `{key}`")));
    }
    if key == "mixer_channel" {
        let channel = i64_or(params, "channel", -1);
        if !(0..=255).contains(&channel) {
            return Err(bad_params(
                "graph.set_param",
                "`channel` is required for `mixer_channel`",
            ));
        }
        let channel_param = params
            .get("channel_param")
            .and_then(Value::as_str)
            .and_then(ChannelParam::from_key)
            .ok_or_else(|| {
                bad_params(
                    "graph.set_param",
                    "`channel_param` must be gain|pan|mute|solo",
                )
            })?;
        return Ok(NodeParam::MixerChannel {
            channel: channel as u8,
            param: channel_param,
        });
    }
    NodeParam::from_key(key)
        .ok_or_else(|| bad_params("graph.set_param", format!("unknown param `{key}`")))
}

fn set_param(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let node_id = i64_or(params, "node", -1);
    if node_id < 0 {
        return Err(bad_params("graph.set_param", "`node` is required"));
    }
    let node_id = node_id as u64;
    let key = params
        .get("param")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_params("graph.set_param", "`param` is required"))?;
    let value = params
        .get("value")
        .and_then(Value::as_f64)
        .ok_or_else(|| bad_params("graph.set_param", "`value` must be a number"))?;
    let param = parse_param(key, params)?;
    // Clamp to the documented domain so CLI / MCP writes cannot push a
    // parameter outside what the UI or the engine accepts (F-039 D4).
    let value = match node_param_range(param) {
        Some((min, max)) => value.clamp(f64::from(min), f64::from(max)),
        None => value,
    };
    let node = node_mut(session, node_id, "graph.set_param")?;
    // Effect slots have no context-free range: clamp against the node's own
    // descriptor table (F-039 V1b-2).
    let value =
        if let (NodeParam::EffectSlot(slot), NodeDoc::Effect { params, .. }) = (param, &*node) {
            let axis = glitcherz_model::effect_slot_axis(params, slot);
            value.clamp(f64::from(axis.min), f64::from(axis.max))
        } else {
            value
        };
    if !set_node_param(node, param, value as f32) {
        return Err(bad_params(
            "graph.set_param",
            format!("param `{}` is not supported by node {node_id}", param.key()),
        ));
    }
    Ok(json!({
        "node": node_id,
        "param": param.key(),
        "value": value,
    }))
}

/// Switches an instrument's voice to one of the built-in (file-free) sound
/// sources (F-039 V1b). Each voice starts from its documented defaults;
/// `graph.set_param` then tunes it. External-file voices (SF2 / sample) and
/// plugins stay out of scope (D5 / D6).
fn set_voice(session: &mut Session, params: &Value) -> crate::Result<Value> {
    let node_id = i64_or(params, "node", -1);
    if node_id < 0 {
        return Err(bad_params("graph.set_voice", "`node` is required"));
    }
    let node_id = node_id as u64;
    let voice = params
        .get("voice")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_params("graph.set_voice", "`voice` is required"))?;
    let node = node_mut(session, node_id, "graph.set_voice")?;
    let NodeDoc::Instrument { voice: slot, .. } = node else {
        return Err(bad_params("graph.set_voice", "node is not an instrument"));
    };
    *slot = match voice {
        "builtin" => VoiceSpec::Builtin,
        "modal" => VoiceSpec::Modal {
            params: ModalParams::default(),
        },
        "pluck" => VoiceSpec::Pluck {
            params: PluckParams::default(),
        },
        "organ" => VoiceSpec::Organ {
            params: OrganParams::default(),
        },
        "wavetable" => VoiceSpec::Wavetable {
            params: WavetableParams::default(),
        },
        other => {
            return Err(bad_params(
                "graph.set_voice",
                format!("voice `{other}` is not supported (builtin|modal|pluck|organ|wavetable)"),
            ));
        }
    };
    Ok(json!({ "node": node_id, "voice": voice }))
}

fn query(session: &mut Session, _params: &Value) -> crate::Result<Value> {
    let nodes: Vec<Value> = session
        .project()
        .graph
        .nodes
        .iter()
        .map(|node| {
            let params: Vec<Value> = node_param_keys(node)
                .into_iter()
                .map(|param| {
                    let (min, max) = node_param_range(param).unwrap_or((0.0, 1.0));
                    json!({
                        "key": param.key(),
                        "channel": match param {
                            NodeParam::MixerChannel { channel, .. } => Some(channel),
                            _ => None,
                        },
                        "channel_param": match param {
                            NodeParam::MixerChannel { param, .. } => Some(param.key()),
                            _ => None,
                        },
                        "index": match param {
                            NodeParam::OrganBar(index) | NodeParam::EffectSlot(index) => {
                                Some(index)
                            }
                            _ => None,
                        },
                        "value": node_param_value(node, param),
                        "min": min,
                        "max": max,
                    })
                })
                .collect();
            json!({
                "node": node.node_id(),
                "kind": node.kind_key(),
                "params": params,
            })
        })
        .collect();
    Ok(json!({ "nodes": nodes }))
}
