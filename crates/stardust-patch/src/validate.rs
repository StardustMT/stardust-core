//! Structural validation for a `PatchGraph`.
//!
//! Per ADR-0004, validation collects every error rather than fail-fast so
//! the UI can surface all problems with a patch at once. Engine-semantic
//! checks (reachability, cycles, runnability) are explicitly out of scope
//! for v1 and belong to whichever consumer defines what "runnable" means.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use thiserror::Error;

use crate::types::{
    CompositeId, NodeId, PatchGraph, Port, PortDirection, PortId, PromotedPort, SignalKind, Wire,
    WireId,
};

#[derive(Clone, Debug, PartialEq, Error, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ValidationError {
    #[error("duplicate node id {0:?}")]
    DuplicateNodeId(NodeId),

    #[error("duplicate wire id {0:?}")]
    DuplicateWireId(WireId),

    #[error("duplicate composite id {0:?}")]
    DuplicateCompositeId(CompositeId),

    #[error("node {node:?} has duplicate port id {port:?}")]
    DuplicatePortId { node: NodeId, port: PortId },

    #[error("wire {wire:?} references unknown endpoint {endpoint:?}")]
    WireUnknownEndpoint { wire: WireId, endpoint: String },

    #[error("wire {wire:?} references unknown port {port:?} on endpoint {endpoint:?}")]
    WireUnknownPort {
        wire: WireId,
        endpoint: String,
        port: PortId,
    },

    #[error("wire {wire:?} connects mismatched signals: source is {from:?}, destination is {to:?}")]
    WireSignalMismatch {
        wire: WireId,
        from: SignalKind,
        to: SignalKind,
    },

    #[error(
        "wire {wire:?} has bad direction: source port direction is {from:?}, destination is {to:?} (must be out -> in)"
    )]
    WireDirection {
        wire: WireId,
        from: PortDirection,
        to: PortDirection,
    },

    #[error("composite {composite:?} contains unknown node {node:?}")]
    CompositeUnknownNode {
        composite: CompositeId,
        node: NodeId,
    },

    #[error(
        "composite {composite:?} promotes unknown port {port:?} on node {node:?} (or node not in composite)"
    )]
    CompositePromotedPortInvalid {
        composite: CompositeId,
        node: NodeId,
        port: PortId,
    },

    #[error("composite {composite:?} members do not form a connected subgraph")]
    CompositeNotConnected { composite: CompositeId },
}

/// Resolved (signal, direction) for one side of a wire — works whether the
/// endpoint is a plain node port or a composite's promoted port.
#[derive(Copy, Clone)]
struct EndpointPort {
    signal: SignalKind,
    direction: PortDirection,
}

impl From<&Port> for EndpointPort {
    fn from(p: &Port) -> Self {
        Self {
            signal: p.signal,
            direction: p.direction,
        }
    }
}

impl From<&PromotedPort> for EndpointPort {
    fn from(p: &PromotedPort) -> Self {
        Self {
            signal: p.signal,
            direction: p.direction,
        }
    }
}

impl PatchGraph {
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();

        // Lookup tables for nodes and their ports.
        let mut node_ports: HashMap<&str, HashMap<&str, &Port>> = HashMap::new();
        let mut seen_node_ids: HashSet<&str> = HashSet::new();
        for node in &self.nodes {
            if !seen_node_ids.insert(node.id.as_str()) {
                errors.push(ValidationError::DuplicateNodeId(node.id.clone()));
            }
            let entry = node_ports.entry(node.id.as_str()).or_default();
            for port in &node.ports {
                if entry.insert(port.id.as_str(), port).is_some() {
                    errors.push(ValidationError::DuplicatePortId {
                        node: node.id.clone(),
                        port: port.id.clone(),
                    });
                }
            }
        }

        // Lookup tables for composites and their promoted ports. Composites
        // and nodes share the wire-endpoint namespace — a wire's fromNode /
        // toNode can be either.
        let mut composite_promoted: HashMap<&str, HashMap<&str, &PromotedPort>> = HashMap::new();
        let mut seen_composite_ids: HashSet<&str> = HashSet::new();
        for composite in &self.composites {
            if !seen_composite_ids.insert(composite.id.as_str()) {
                errors.push(ValidationError::DuplicateCompositeId(composite.id.clone()));
            }
            let entry = composite_promoted.entry(composite.id.as_str()).or_default();
            for pp in &composite.promoted_ports {
                entry.insert(pp.id.as_str(), pp);
            }
        }

        // Per-wire checks.
        let mut seen_wire_ids: HashSet<&str> = HashSet::new();
        for wire in &self.wires {
            if !seen_wire_ids.insert(wire.id.as_str()) {
                errors.push(ValidationError::DuplicateWireId(wire.id.clone()));
            }

            let from = resolve_endpoint(
                wire,
                wire.from_node.as_str(),
                wire.from_port.as_str(),
                &node_ports,
                &composite_promoted,
                &mut errors,
            );
            let to = resolve_endpoint(
                wire,
                wire.to_node.as_str(),
                wire.to_port.as_str(),
                &node_ports,
                &composite_promoted,
                &mut errors,
            );

            if let (Some(f), Some(t)) = (from, to) {
                if f.signal != t.signal {
                    errors.push(ValidationError::WireSignalMismatch {
                        wire: wire.id.clone(),
                        from: f.signal,
                        to: t.signal,
                    });
                }
                if f.direction != PortDirection::Out || t.direction != PortDirection::In {
                    errors.push(ValidationError::WireDirection {
                        wire: wire.id.clone(),
                        from: f.direction,
                        to: t.direction,
                    });
                }
            }
        }

        // Composite member + promoted-port consistency.
        for composite in &self.composites {
            let member_set: HashSet<&str> =
                composite.contains.iter().map(|id| id.as_str()).collect();

            for node_id in &composite.contains {
                if !node_ports.contains_key(node_id.as_str()) {
                    errors.push(ValidationError::CompositeUnknownNode {
                        composite: composite.id.clone(),
                        node: node_id.clone(),
                    });
                }
            }

            for promoted in &composite.promoted_ports {
                let valid = member_set.contains(promoted.internal_node.as_str())
                    && node_ports
                        .get(promoted.internal_node.as_str())
                        .map(|ports| ports.contains_key(promoted.internal_port.as_str()))
                        .unwrap_or(false);
                if !valid {
                    errors.push(ValidationError::CompositePromotedPortInvalid {
                        composite: composite.id.clone(),
                        node: promoted.internal_node.clone(),
                        port: promoted.internal_port.clone(),
                    });
                }
            }

            if composite.contains.len() > 1
                && composite
                    .contains
                    .iter()
                    .all(|id| node_ports.contains_key(id.as_str()))
                && !is_connected(&self.wires, &member_set)
            {
                errors.push(ValidationError::CompositeNotConnected {
                    composite: composite.id.clone(),
                });
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn resolve_endpoint(
    wire: &Wire,
    endpoint: &str,
    port: &str,
    node_ports: &HashMap<&str, HashMap<&str, &Port>>,
    composite_promoted: &HashMap<&str, HashMap<&str, &PromotedPort>>,
    errors: &mut Vec<ValidationError>,
) -> Option<EndpointPort> {
    if let Some(ports) = node_ports.get(endpoint) {
        if let Some(p) = ports.get(port) {
            return Some(EndpointPort::from(*p));
        }
        errors.push(ValidationError::WireUnknownPort {
            wire: wire.id.clone(),
            endpoint: endpoint.to_owned(),
            port: PortId::new(port),
        });
        return None;
    }
    if let Some(promoted) = composite_promoted.get(endpoint) {
        if let Some(pp) = promoted.get(port) {
            return Some(EndpointPort::from(*pp));
        }
        errors.push(ValidationError::WireUnknownPort {
            wire: wire.id.clone(),
            endpoint: endpoint.to_owned(),
            port: PortId::new(port),
        });
        return None;
    }
    errors.push(ValidationError::WireUnknownEndpoint {
        wire: wire.id.clone(),
        endpoint: endpoint.to_owned(),
    });
    None
}

/// Treat the wires inside `members` as undirected edges and check that every
/// member is reachable from any other. Operates on node ids only — composite
/// membership lists are pure node lists.
fn is_connected(wires: &[Wire], members: &HashSet<&str>) -> bool {
    if members.len() <= 1 {
        return true;
    }

    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for &m in members {
        adjacency.entry(m).or_default();
    }

    for wire in wires {
        let from = members.get(wire.from_node.as_str()).copied();
        let to = members.get(wire.to_node.as_str()).copied();
        if let (Some(f), Some(t)) = (from, to) {
            adjacency.entry(f).or_default().push(t);
            adjacency.entry(t).or_default().push(f);
        }
    }

    let start = *members.iter().next().unwrap();
    let mut visited: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = vec![start];
    while let Some(node) = stack.pop() {
        if !visited.insert(node) {
            continue;
        }
        if let Some(neighbors) = adjacency.get(node) {
            for &n in neighbors {
                if !visited.contains(n) {
                    stack.push(n);
                }
            }
        }
    }

    visited.len() == members.len()
}
