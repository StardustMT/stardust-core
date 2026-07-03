//! Core patch-graph types: ports, nodes, wires, composites, the graph.
//!
//! Mirrors `stardust-pit/src/src/components/patch-graph/_types.ts`. Field
//! names are camelCase on the wire so the TS UI can produce and consume
//! `PatchDocument` JSON without adapters.

use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------------
// ID newtypes
// -----------------------------------------------------------------------------
//
// All IDs are strings on the wire (matching the TS shape), but distinct types
// in Rust so a NodeId can't accidentally be passed where a WireId is expected.

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

string_id!(NodeId);
string_id!(PortId);
string_id!(WireId);
string_id!(CompositeId);

// -----------------------------------------------------------------------------
// Port + wire signal types
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalKind {
    Midi,
    Audio,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortDirection {
    In,
    Out,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Port {
    pub id: PortId,
    pub label: String,
    pub signal: SignalKind,
    pub direction: PortDirection,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub config: Option<PortConfig>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PortConfig {
    #[serde(rename_all = "camelCase")]
    Zone {
        from_note: u8,
        to_note: u8,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        color_hue: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        wire_follows_color: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    Pad {
        pad_index: u32,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        note: Option<u8>,
    },
    #[serde(rename_all = "camelCase")]
    Channel {
        midi_channel: u8,
    },
    #[serde(rename_all = "camelCase")]
    Stereo {
        channel: StereoChannel,
    },
    Mono,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StereoChannel {
    L,
    R,
}

// -----------------------------------------------------------------------------
// Nodes
// -----------------------------------------------------------------------------

/// Node kinds are dotted strings on the wire (matching TS), but we keep them
/// as an explicit enum in Rust so unknown kinds error at deserialize time
/// rather than slipping silently into the graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    // sources
    #[serde(rename = "source.keyboard")]
    SourceKeyboard,
    #[serde(rename = "source.pads")]
    SourcePads,
    #[serde(rename = "source.switch")]
    SourceSwitch,
    #[serde(rename = "source.sustain-pedal")]
    SourceSustainPedal,
    #[serde(rename = "source.expression-pedal")]
    SourceExpressionPedal,
    #[serde(rename = "source.pitch-wheel")]
    SourcePitchWheel,
    #[serde(rename = "source.mod-wheel")]
    SourceModWheel,
    #[serde(rename = "source.knob")]
    SourceKnob,
    #[serde(rename = "source.fader")]
    SourceFader,
    // midi processors
    #[serde(rename = "midi.transpose")]
    MidiTranspose,
    #[serde(rename = "midi.mix")]
    MidiMix,
    // instruments
    #[serde(rename = "instrument.plugin")]
    InstrumentPlugin,
    /// Diagnostic-only built-in tone generator. Not surfaced in the
    /// user-facing palette; the engine self-test (Settings) instantiates
    /// it transparently. Renamed from `instrument.sine` in schema v2.
    #[serde(rename = "instrument.testtone")]
    InstrumentTestTone,
    // audio effects
    #[serde(rename = "audio.eq")]
    AudioEq,
    #[serde(rename = "audio.mix")]
    AudioMix,
    // sinks
    #[serde(rename = "sink.main-out")]
    SinkMainOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeClass {
    Source,
    MidiProcessor,
    Instrument,
    AudioEffect,
    AudioRouter,
    Sink,
}

impl NodeKind {
    pub fn class(self) -> NodeClass {
        use NodeKind::*;
        match self {
            SourceKeyboard
            | SourcePads
            | SourceSwitch
            | SourceSustainPedal
            | SourceExpressionPedal
            | SourcePitchWheel
            | SourceModWheel
            | SourceKnob
            | SourceFader => NodeClass::Source,
            MidiTranspose | MidiMix => NodeClass::MidiProcessor,
            InstrumentPlugin | InstrumentTestTone => NodeClass::Instrument,
            AudioEq | AudioMix => NodeClass::AudioEffect,
            SinkMainOut => NodeClass::Sink,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub ports: Vec<Port>,
    /// Free-form per-kind config. Per ADR-0004, strong typing per kind lives
    /// in the engine, not in the data model. Revisit when the engine consumer
    /// makes that decision load-bearing.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub config: Option<serde_json::Value>,
}

// -----------------------------------------------------------------------------
// Wires
// -----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Wire {
    pub id: WireId,
    pub from_node: NodeId,
    pub from_port: PortId,
    pub to_node: NodeId,
    pub to_port: PortId,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub color: Option<String>,
}

// -----------------------------------------------------------------------------
// Composite blocks
// -----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotedPort {
    pub id: PortId,
    pub label: String,
    pub direction: PortDirection,
    pub signal: SignalKind,
    pub internal_node: NodeId,
    pub internal_port: PortId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompositeBlock {
    pub id: CompositeId,
    pub name: String,
    pub contains: Vec<NodeId>,
    pub locked: bool,
    pub promoted_ports: Vec<PromotedPort>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub color_hue: Option<f32>,
}

// -----------------------------------------------------------------------------
// The graph
// -----------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchGraph {
    pub nodes: Vec<GraphNode>,
    pub wires: Vec<Wire>,
    pub composites: Vec<CompositeBlock>,
}

impl PatchGraph {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn find_node(&self, id: &NodeId) -> Option<&GraphNode> {
        self.nodes.iter().find(|n| &n.id == id)
    }
}

impl GraphNode {
    pub fn find_port(&self, id: &PortId) -> Option<&Port> {
        self.ports.iter().find(|p| &p.id == id)
    }
}
