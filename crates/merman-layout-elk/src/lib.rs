#![forbid(unsafe_code)]

//! ELK layout for `merman`, over the [`elk_layout`] port.
//!
//! This crate keeps the public shape of `merman-layout-elk`: it takes a provider-neutral
//! [`Graph`] and answers a [`LayoutResult`].
//! What changes sits underneath.
//! The upstream crate drives `merman-elk-layered`, a translation of Eclipse ELK source under
//! EPL-2.0 alone, and EPL-2.0 does not combine with GPL-family copyleft.
//! This crate drives `elk-layout` instead, a pure-Rust ELK port that ships
//! `EPL-2.0 OR GPL-3.0-or-later`, so a GPL-3.0 or AGPL-3.0 consumer takes the GPL arm and
//! combines legally (GPL-3.0 section 13).
//!
//! The transport is ELK JSON, the same document that `elkjs` reads.
//! [`graph_to_json`] writes it, `elk_layout::layout_value` lays it out, and
//! [`json_to_layout_result`] reads the geometry back.
//!
//! Coordinates leave this crate absolute: a node reports its center and an edge reports its
//! route in the root coordinate system, because the merman renderers place every element in one
//! finished diagram.
//! ELK hands a child back in its container's system and hangs an edge route off the node named by
//! the edge's `container` field, so the walk adds the ancestor origins back.
//!
//! One budget caveat: ELK lays a graph out in one call and offers no interruption hook, so a
//! [`WorkControl`] here checks the budget before and after that call, never inside it.

mod model;
pub use model::*;

use std::{collections::HashMap, num::NonZeroU64};

use serde_json::{json, Map, Value};

/// The slack one layout charges on top of its node and edge count.
const LOAD_WORK_SCALE: usize = 8;

/// The text an edge label carries into ELK.
///
/// Merman measures the label itself and paints it from its own model, so this text never reaches
/// the picture.
/// ELK places a label only when its text holds a character: an empty string leaves the label at
/// `(0, 0)` and the diagram then paints every edge label in the top-left corner.
const LABEL_PLACEHOLDER: &str = " ";

/// A nonzero random seed captured by the owner of one render or layout operation.
///
/// ELK treats `randomSeed = 0` as an unseeded request.
/// A headless layout must not turn that into ambient process randomness, so the seed comes from
/// the operation instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElkOperationSeed(NonZeroU64);

impl ElkOperationSeed {
    /// Build a token from the seed of one operation.
    #[must_use]
    pub const fn from_operation_seed(seed: NonZeroU64) -> Self {
        Self(seed)
    }

    /// The seed this token carries, as ELK's signed option.
    fn as_option_seed(self) -> i64 {
        i64::try_from(self.0.get()).unwrap_or(i64::MAX)
    }
}

/// The reason a work budget stops a layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkError {
    /// The caller cancelled the operation, or the meter refused the charge.
    Interrupted,
    /// A size estimate left the counter, so the caller must fail closed.
    ArithmeticOverflow,
}

/// A cooperative work budget.
///
/// The trait mirrors the upstream crate, so a caller compiles unchanged.
pub trait WorkControl {
    /// Refuse work before it starts.
    ///
    /// # Errors
    ///
    /// Returns [`WorkError`] when the caller must stop.
    fn check(&mut self, units: usize) -> std::result::Result<(), WorkError>;

    /// Charge work that already ran.
    ///
    /// # Errors
    ///
    /// Returns [`WorkError`] when the caller must stop.
    fn charge(&mut self, units: usize) -> std::result::Result<(), WorkError>;
}

/// A budget that never refuses work.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopWorkControl;

impl WorkControl for NoopWorkControl {
    fn check(&mut self, _units: usize) -> std::result::Result<(), WorkError> {
        Ok(())
    }

    fn charge(&mut self, _units: usize) -> std::result::Result<(), WorkError> {
        Ok(())
    }
}

/// Everything one layout can fail on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The graph holds a shape this provider cannot express.
    #[error("ELK layout cannot express the graph: {0}")]
    Unsupported(String),
    /// ELK rejected the document or failed inside its own pipeline.
    #[error("ELK layout failed: {0}")]
    Provider(String),
    /// ELK answered a document this crate cannot read.
    #[error("ELK returned no readable layout: {0}")]
    Output(String),
    /// The hierarchy nests deeper than this adapter accepts.
    #[error("ELK layout does not accept a hierarchy deeper than {limit} containers")]
    TooDeep { limit: usize },
    /// The work budget stopped the layout.
    #[error("ELK layout stopped on the work budget")]
    Work(WorkError),
}

impl From<WorkError> for Error {
    fn from(error: WorkError) -> Self {
        Self::Work(error)
    }
}

impl Error {
    /// The work failure behind this error, when a budget caused it.
    #[must_use]
    pub const fn work_error(&self) -> Option<WorkError> {
        match self {
            Self::Work(error) => Some(*error),
            Self::Unsupported(_) | Self::Provider(_) | Self::Output(_) | Self::TooDeep { .. } => {
                None
            }
        }
    }
}

/// The answer of one layout call.
pub type Result<T> = std::result::Result<T, Error>;

/// Lay `graph` out with ELK and answer absolute geometry.
///
/// A `randomSeed` of `0` in [`LayeredOptions`] resolves to `1` here, because a seedless layout
/// would read ambient randomness.
///
/// # Errors
///
/// Returns [`Error`] when ELK rejects the document or the document cannot express the graph.
pub fn layout(graph: &Graph) -> Result<LayoutResult> {
    layout_with_work_control(graph, &mut NoopWorkControl)
}

/// Lay `graph` out with a work budget.
///
/// # Errors
///
/// Returns [`Error`] on a provider failure or when the budget stops the layout.
pub fn layout_with_work_control(
    graph: &Graph,
    work_control: &mut dyn WorkControl,
) -> Result<LayoutResult> {
    layout_with_seed_and_work_control(graph, None, work_control)
}

/// Lay `graph` out with an operation seed, which replaces ELK's `randomSeed = 0` sentinel.
///
/// # Errors
///
/// Returns [`Error`] on a provider failure.
pub fn layout_with_operation_seed(graph: &Graph, seed: ElkOperationSeed) -> Result<LayoutResult> {
    layout_with_seed_and_work_control(graph, Some(seed), &mut NoopWorkControl)
}

/// Lay `graph` out with an operation seed and a work budget.
///
/// # Errors
///
/// Returns [`Error`] on a provider failure or when the budget stops the layout.
pub fn layout_with_operation_seed_and_work_control(
    graph: &Graph,
    seed: ElkOperationSeed,
    work_control: &mut dyn WorkControl,
) -> Result<LayoutResult> {
    layout_with_seed_and_work_control(graph, Some(seed), work_control)
}

fn layout_with_seed_and_work_control(
    graph: &Graph,
    seed: Option<ElkOperationSeed>,
    work_control: &mut dyn WorkControl,
) -> Result<LayoutResult> {
    check_hierarchy_depth(graph)?;
    let estimate = estimate_work(graph)?;
    work_control.check(estimate)?;
    let request = graph_to_json(graph, seed);
    if std::env::var_os("MERMAN_ELK_DUMP_REQUEST").is_some() {
        eprintln!(
            "ELK request: {}",
            serde_json::to_string_pretty(&request).unwrap_or_default()
        );
    }
    let answer = elk_layout::layout_value(request, "{}")
        .map_err(|error| Error::Provider(error.to_string()))?;
    if std::env::var_os("MERMAN_ELK_DUMP_ANSWER").is_some() {
        eprintln!(
            "ELK answer: {}",
            serde_json::to_string_pretty(&answer).unwrap_or_default()
        );
    }
    let result = json_to_layout_result(&answer)?;
    work_control.charge(estimate)?;
    Ok(result)
}

/// The deepest container nesting this adapter accepts.
///
/// `elk-layout` walks a nested hierarchy recursively, so a deeper document would exhaust a
/// small caller stack inside the third-party port instead of returning an error here.
pub const MAX_HIERARCHY_DEPTH: usize = 64;

/// Reject a hierarchy deeper than [`MAX_HIERARCHY_DEPTH`] before the port reads it.
fn check_hierarchy_depth(graph: &Graph) -> Result<()> {
    let count = graph.nodes.len();
    let mut by_id: HashMap<&str, usize> = HashMap::new();
    for (index, node) in graph.nodes.iter().enumerate() {
        by_id.entry(node.id.as_str()).or_insert(index);
    }
    let mut depths: Vec<Option<usize>> = vec![None; count];
    let mut marker: Vec<usize> = vec![usize::MAX; count];
    for start in 0..count {
        if depths[start].is_some() {
            continue;
        }
        let mut chain = Vec::new();
        let mut current = Some(start);
        while let Some(index) = current {
            if depths[index].is_some() || marker[index] == start {
                break;
            }
            marker[index] = start;
            chain.push(index);
            current = graph.nodes[index]
                .parent
                .as_deref()
                .and_then(|parent| by_id.get(parent).copied());
        }
        let base = current
            .and_then(|index| depths[index])
            .map_or(0, |known| known + 1);
        for (offset, &index) in chain.iter().rev().enumerate() {
            depths[index] = Some(base + offset);
        }
    }
    match depths.into_iter().flatten().max() {
        Some(depth) if depth > MAX_HIERARCHY_DEPTH => Err(Error::TooDeep {
            limit: MAX_HIERARCHY_DEPTH,
        }),
        _ => Ok(()),
    }
}

/// The work estimate of one graph: every node and edge, scaled up by [`LOAD_WORK_SCALE`].
fn estimate_work(graph: &Graph) -> Result<usize> {
    graph
        .nodes
        .len()
        .checked_add(graph.edges.len())
        .and_then(|total| total.checked_mul(LOAD_WORK_SCALE))
        .ok_or(Error::Work(WorkError::ArithmeticOverflow))
}

/// Write the ELK JSON document for `graph`.
///
/// The model names a parent per node, so the walk nests the flat list into `children` and hangs
/// every edge off the root: ELK then picks the nearest common container itself.
#[must_use]
pub fn graph_to_json(graph: &Graph, seed: Option<ElkOperationSeed>) -> Value {
    let root_id = graph_id_or_default(&graph.id);
    let children_by_parent = children_by_parent(graph);
    let mut object = Map::new();
    object.insert("id".into(), Value::String(root_id.clone()));
    object.insert(
        "layoutOptions".into(),
        Value::Object(root_options(graph, seed)),
    );
    let children = children_to_json(graph, &children_by_parent);
    if !children.is_empty() {
        object.insert("children".into(), Value::Array(children));
    }
    let edges = graph.edges.iter().map(edge_to_json).collect::<Vec<_>>();
    if !edges.is_empty() {
        object.insert("edges".into(), Value::Array(edges));
    }
    Value::Object(object)
}

fn graph_id_or_default(id: &str) -> String {
    if id.is_empty() {
        "root".to_owned()
    } else {
        id.to_owned()
    }
}

/// Group the node indices of `graph` by their parent id.
fn children_by_parent(graph: &Graph) -> HashMap<Option<&str>, Vec<usize>> {
    let mut map: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
    for (index, node) in graph.nodes.iter().enumerate() {
        map.entry(node.parent.as_deref()).or_default().push(index);
    }
    map
}

/// Nest every node under its parent without recursion.
///
/// A hierarchy deeper than a few dozen levels would exhaust a small worker stack if the walk
/// recursed, so this visits each node twice: once to reach its children, once to assemble it
/// after them.
fn children_to_json(
    graph: &Graph,
    children_by_parent: &HashMap<Option<&str>, Vec<usize>>,
) -> Vec<Value> {
    let mut built: Vec<Option<Value>> = graph.nodes.iter().map(|_| None).collect();
    let mut visiting = vec![false; graph.nodes.len()];
    let roots = children_by_parent.get(&None).cloned().unwrap_or_default();
    for &root in &roots {
        let mut pending = vec![(root, false)];
        while let Some((index, expanded)) = pending.pop() {
            if built[index].is_some() {
                continue;
            }
            if !expanded {
                if visiting[index] {
                    continue;
                }
                visiting[index] = true;
                pending.push((index, true));
                let children = child_indices(children_by_parent, &graph.nodes[index].id);
                for child in children.into_iter().rev() {
                    pending.push((child, false));
                }
                continue;
            }
            let children = child_indices(children_by_parent, &graph.nodes[index].id)
                .into_iter()
                .filter_map(|child| built[child].take())
                .collect();
            built[index] = Some(node_to_json(graph, children_by_parent, index, children));
        }
    }
    roots
        .into_iter()
        .filter_map(|root| built[root].take())
        .collect()
}

fn child_indices(children_by_parent: &HashMap<Option<&str>, Vec<usize>>, id: &str) -> Vec<usize> {
    children_by_parent
        .get(&Some(id))
        .cloned()
        .unwrap_or_default()
}

fn node_has_children(children_by_parent: &HashMap<Option<&str>, Vec<usize>>, id: &str) -> bool {
    children_by_parent
        .get(&Some(id))
        .is_some_and(|children| !children.is_empty())
}

fn node_to_json(
    graph: &Graph,
    children_by_parent: &HashMap<Option<&str>, Vec<usize>>,
    index: usize,
    children: Vec<Value>,
) -> Value {
    let node = &graph.nodes[index];
    let has_children = node_has_children(children_by_parent, &node.id);
    let mut object = Map::new();
    object.insert("id".into(), Value::String(node.id.clone()));
    if !has_children {
        object.insert("width".into(), Value::from(node.width));
        object.insert("height".into(), Value::from(node.height));
    }
    if has_children {
        object.insert("children".into(), Value::Array(children));
    }
    if let Some(label) = node.label.filter(|_| node.kind == NodeKind::Group) {
        object.insert(
            "labels".into(),
            Value::Array(vec![json!({
                "id": format!("{}__title", node.id),
                "text": node.label_text.clone().unwrap_or_default(),
                "width": label.width,
                "height": label.height,
                "layoutOptions": { "elk.nodeLabels.placement": "[H_LEFT, V_TOP, INSIDE]" },
            })]),
        );
    }
    let options = node_options(graph, node, has_children);
    if !options.is_empty() {
        object.insert("layoutOptions".into(), Value::Object(options));
    }
    Value::Object(object)
}

fn node_options(graph: &Graph, node: &Node, has_children: bool) -> Map<String, Value> {
    let mut options = Map::new();
    if let Some(direction) = node.direction {
        options.insert(
            "elk.direction".into(),
            Value::String(direction_name(direction).into()),
        );
    }
    if let Some(handling) = node.hierarchy_handling {
        options.insert(
            "elk.hierarchyHandling".into(),
            Value::String(hierarchy_name(handling).into()),
        );
    }
    if let Some(algorithm) = node.container.algorithm {
        options.insert(
            "elk.algorithm".into(),
            Value::String(algorithm_name(algorithm).into()),
        );
    }
    if let Some(constraint) = node.layer_constraint {
        options.insert(
            "elk.layered.layering.layerConstraint".into(),
            Value::String(layer_constraint_name(constraint).into()),
        );
    }
    if let Some(alignment) = node.port_alignment {
        options.insert(
            "elk.portAlignment.default".into(),
            Value::String(port_alignment_name(alignment).into()),
        );
    }
    if has_children {
        options.insert(
            "elk.padding".into(),
            Value::String(padding_string(container_padding(graph, node))),
        );
        options.insert(
            "elk.spacing.nodeNode".into(),
            Value::from(graph.spacing.node_node),
        );
        options.insert(
            "elk.layered.spacing.nodeNodeBetweenLayers".into(),
            Value::from(graph.spacing.layer_layer),
        );
    }
    options
}

/// The container padding a group asks for, with the title band added on top.
fn container_padding(graph: &Graph, node: &Node) -> [f64; 4] {
    let spacing = graph.spacing;
    let mut top = spacing.group_padding_y;
    if let Some(label) = node.label.filter(|_| node.kind == NodeKind::Group) {
        top += label.height + spacing.group_label_gap;
    }
    [
        top,
        spacing.group_padding_x,
        spacing.group_padding_y,
        spacing.group_padding_x,
    ]
}

fn padding_string([top, left, bottom, right]: [f64; 4]) -> String {
    format!("[top={top},left={left},bottom={bottom},right={right}]")
}

fn root_options(graph: &Graph, seed: Option<ElkOperationSeed>) -> Map<String, Value> {
    let mut options = Map::new();
    let layered = &graph.options.layered;
    let spacing = graph.spacing;
    options.insert(
        "elk.algorithm".into(),
        Value::String(algorithm_name(graph.options.algorithm).into()),
    );
    options.insert(
        "elk.direction".into(),
        Value::String(direction_name(graph.direction).into()),
    );
    options.insert(
        "elk.edgeRouting".into(),
        Value::String(edge_routing_name(layered.edge_routing).into()),
    );
    options.insert(
        "elk.hierarchyHandling".into(),
        Value::String(hierarchy_name(layered.hierarchy_handling).into()),
    );
    let random_seed = if layered.random_seed == 0 {
        seed.map_or(1, ElkOperationSeed::as_option_seed)
    } else {
        i64::from(layered.random_seed)
    };
    options.insert("elk.randomSeed".into(), Value::from(random_seed));
    options.insert(
        "elk.spacing.nodeNode".into(),
        Value::from(spacing.node_node),
    );
    options.insert(
        "elk.layered.spacing.nodeNodeBetweenLayers".into(),
        Value::from(spacing.layer_layer),
    );
    options.insert(
        "elk.layered.cycleBreaking.strategy".into(),
        Value::String(cycle_breaking_name(layered.cycle_breaking).into()),
    );
    options.insert(
        "elk.layered.layering.strategy".into(),
        Value::String(layering_name(layered.layering).into()),
    );
    options.insert(
        "elk.layered.nodePlacement.strategy".into(),
        Value::String(node_placement_name(layered.node_placement).into()),
    );
    options.insert(
        "elk.layered.nodePlacement.bk.fixedAlignment".into(),
        Value::String(node_placement_alignment_name(layered.node_placement_alignment).into()),
    );
    options.insert(
        "elk.layered.considerModelOrder.strategy".into(),
        Value::String(
            if layered.consider_model_order {
                model_order_name(layered.model_order)
            } else {
                "NONE"
            }
            .to_owned(),
        ),
    );
    options.insert(
        "elk.layered.mergeEdges".into(),
        Value::Bool(layered.merge_edges),
    );
    options.insert(
        "elk.layered.mergeHierarchyEdges".into(),
        Value::Bool(layered.merge_hierarchy_edges),
    );
    options.insert(
        "elk.layered.unnecessaryBendpoints".into(),
        Value::Bool(layered.unnecessary_bendpoints),
    );
    options.insert(
        "elk.insideSelfLoops.activate".into(),
        Value::Bool(layered.inside_self_loops_activate),
    );
    options.insert(
        "elk.layered.selfLoopDistribution.strategy".into(),
        Value::String(self_loop_distribution_name(layered.self_loop_distribution).into()),
    );
    options.insert(
        "elk.layered.selfLoopOrdering.strategy".into(),
        Value::String(self_loop_ordering_name(layered.self_loop_ordering).into()),
    );
    if layered.force_node_model_order {
        options.insert(
            "elk.layered.considerModelOrder.noModelOrder".into(),
            Value::Bool(false),
        );
    }
    if matches!(layered.layering, LayeringStrategy::CoffmanGraham)
        && layered.layering_layer_bound > 0
    {
        options.insert(
            "elk.layered.layering.coffmanGraham.layerBound".into(),
            Value::from(i64::from(layered.layering_layer_bound)),
        );
    }
    options
}

fn label_key_suffix(key: TerminalLabelKey) -> &'static str {
    match key {
        TerminalLabelKey::StartLeft => "sl",
        TerminalLabelKey::StartRight => "sr",
        TerminalLabelKey::EndLeft => "el",
        TerminalLabelKey::EndRight => "er",
    }
}

fn edge_to_json(edge: &Edge) -> Value {
    let mut labels = Vec::new();
    if let Some(label) = edge.label {
        labels.push(json!({
            "id": format!("{}__c", edge.id),
            "text": LABEL_PLACEHOLDER,
            "width": label.width,
            "height": label.height,
            "layoutOptions": { "elk.edgeLabels.placement": "CENTER" },
        }));
    }
    for terminal in &edge.terminal_labels {
        let placement = if terminal.key.at_start() {
            "TAIL"
        } else {
            "HEAD"
        };
        labels.push(json!({
            "id": format!("{}__{}", edge.id, label_key_suffix(terminal.key)),
            "text": LABEL_PLACEHOLDER,
            "width": terminal.label.width,
            "height": terminal.label.height,
            "layoutOptions": {
                "elk.edgeLabels.placement": placement,
                "elk.edgeLabels.inline": false,
            },
        }));
    }
    let mut object = Map::new();
    object.insert("id".into(), Value::String(edge.id.clone()));
    object.insert(
        "sources".into(),
        Value::Array(vec![Value::String(edge.source.clone())]),
    );
    object.insert(
        "targets".into(),
        Value::Array(vec![Value::String(edge.target.clone())]),
    );
    if !labels.is_empty() {
        object.insert("labels".into(), Value::Array(labels));
    }
    Value::Object(object)
}

/// Read ELK's answer into absolute geometry.
///
/// # Errors
///
/// Returns [`Error::Output`] when the document holds no `children` array at the root.
pub fn json_to_layout_result(value: &Value) -> Result<LayoutResult> {
    let mut result = LayoutResult::default();
    let mut origins: HashMap<String, Point> = HashMap::new();
    let root_id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("root")
        .to_owned();
    origins.insert(root_id.clone(), Point { x: 0.0, y: 0.0 });
    collect_nodes(value, &root_id, &mut result, &mut origins);
    collect_edges(value, &root_id, &origins, &mut result);
    Ok(result)
}

fn point_at(value: &Value, key: &str) -> Option<Point> {
    let point = value.get(key)?;
    Some(Point {
        x: point.get("x").and_then(Value::as_f64)?,
        y: point.get("y").and_then(Value::as_f64)?,
    })
}

/// Walk every container once, keeping preorder output and absolute origins.
///
/// The walk holds its own stack: an answered hierarchy can be as deep as the request, and
/// recursion there would let one document exhaust a small worker stack.
fn collect_nodes(
    scope: &Value,
    scope_id: &str,
    result: &mut LayoutResult,
    origins: &mut HashMap<String, Point>,
) {
    let mut pending: Vec<(&Value, Point, String)> = Vec::new();
    let zero = Point { x: 0.0, y: 0.0 };
    for child in children_of(scope).iter().rev() {
        pending.push((child, zero, scope_id.to_owned()));
    }
    while let Some((child, origin, _)) = pending.pop() {
        let Some(id) = child.get("id").and_then(Value::as_str) else {
            continue;
        };
        let width = child.get("width").and_then(Value::as_f64).unwrap_or(0.0);
        let height = child.get("height").and_then(Value::as_f64).unwrap_or(0.0);
        let left = origin.x + child.get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let top = origin.y + child.get("y").and_then(Value::as_f64).unwrap_or(0.0);
        origins.insert(id.to_owned(), Point { x: left, y: top });
        result.nodes.push(NodeLayout {
            id: id.to_owned(),
            x: left + width / 2.0,
            y: top + height / 2.0,
            width,
            height,
        });
        let child_origin = Point { x: left, y: top };
        for grandchild in children_of(child).iter().rev() {
            pending.push((grandchild, child_origin, id.to_owned()));
        }
    }
}

fn children_of(scope: &Value) -> &[Value] {
    scope
        .get("children")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn collect_scope_edges(
    scope: &Value,
    scope_id: &str,
    origins: &HashMap<String, Point>,
    result: &mut LayoutResult,
) {
    if let Some(edges) = scope.get("edges").and_then(Value::as_array) {
        for edge in edges {
            let Some(id) = edge.get("id").and_then(Value::as_str) else {
                continue;
            };
            let container = edge
                .get("container")
                .and_then(Value::as_str)
                .unwrap_or(scope_id);
            let offset = origins
                .get(container)
                .copied()
                .unwrap_or(Point { x: 0.0, y: 0.0 });
            let mut points = Vec::new();
            if let Some(sections) = edge.get("sections").and_then(Value::as_array) {
                for section in sections {
                    if let Some(start) = point_at(section, "startPoint") {
                        push_point(&mut points, translate(start, offset));
                    }
                    if let Some(bends) = section.get("bendPoints").and_then(Value::as_array) {
                        for bend in bends {
                            if let (Some(x), Some(y)) = (
                                bend.get("x").and_then(Value::as_f64),
                                bend.get("y").and_then(Value::as_f64),
                            ) {
                                push_point(
                                    &mut points,
                                    Point {
                                        x: x + offset.x,
                                        y: y + offset.y,
                                    },
                                );
                            }
                        }
                    }
                    if let Some(end) = point_at(section, "endPoint") {
                        push_point(&mut points, translate(end, offset));
                    }
                }
            }
            let mut labels = Vec::new();
            if let Some(edge_labels) = edge.get("labels").and_then(Value::as_array) {
                for label in edge_labels {
                    let Some(label_id) = label.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(suffix) = label_id.rsplit("__").next() else {
                        continue;
                    };
                    let Some(terminal) = terminal_from_suffix(suffix) else {
                        continue;
                    };
                    let Some(x) = label.get("x").and_then(Value::as_f64) else {
                        continue;
                    };
                    let Some(y) = label.get("y").and_then(Value::as_f64) else {
                        continue;
                    };
                    labels.push(EdgeLabelLayout {
                        terminal,
                        x: x + offset.x,
                        y: y + offset.y,
                        width: label.get("width").and_then(Value::as_f64).unwrap_or(0.0),
                        height: label.get("height").and_then(Value::as_f64).unwrap_or(0.0),
                    });
                }
            }
            result.edges.push(EdgeLayout {
                id: id.to_owned(),
                points,
                labels,
            });
        }
    }
}

/// Visit every container's edges with an explicit stack for the same reason as [`collect_nodes`].
fn collect_edges(
    scope: &Value,
    scope_id: &str,
    origins: &HashMap<String, Point>,
    result: &mut LayoutResult,
) {
    let mut pending: Vec<(&Value, String)> = vec![(scope, scope_id.to_owned())];
    while let Some((current, current_id)) = pending.pop() {
        collect_scope_edges(current, &current_id, origins, result);
        for child in children_of(current).iter().rev() {
            if let Some(id) = child.get("id").and_then(Value::as_str) {
                pending.push((child, id.to_owned()));
            }
        }
    }
}

fn terminal_from_suffix(suffix: &str) -> Option<Option<TerminalLabelKey>> {
    match suffix {
        "c" => Some(None),
        other => TerminalLabelKey::ALL
            .into_iter()
            .find(|key| label_key_suffix(*key) == other)
            .map(Some),
    }
}

fn translate(point: Point, offset: Point) -> Point {
    Point {
        x: point.x + offset.x,
        y: point.y + offset.y,
    }
}

fn push_point(points: &mut Vec<Point>, point: Point) {
    if points.last() != Some(&point) {
        points.push(point);
    }
}

fn algorithm_name(algorithm: Algorithm) -> &'static str {
    match algorithm {
        Algorithm::Layered => "layered",
        Algorithm::Box => "box",
        Algorithm::Rectpacking => "rectpacking",
        Algorithm::Force => "force",
        Algorithm::Stress => "stress",
        Algorithm::MrTree => "mrtree",
        Algorithm::Radial => "radial",
        Algorithm::SporeOverlap => "sporeOverlap",
    }
}

const fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Left => "LEFT",
        Direction::Right => "RIGHT",
        Direction::Up => "UP",
        Direction::Down => "DOWN",
    }
}

const fn edge_routing_name(edge_routing: EdgeRouting) -> &'static str {
    match edge_routing {
        EdgeRouting::Orthogonal => "ORTHOGONAL",
        EdgeRouting::Polyline => "POLYLINE",
    }
}

const fn hierarchy_name(hierarchy: HierarchyHandling) -> &'static str {
    match hierarchy {
        HierarchyHandling::IncludeChildren => "INCLUDE_CHILDREN",
        HierarchyHandling::SeparateChildren => "SEPARATE_CHILDREN",
    }
}

const fn cycle_breaking_name(strategy: CycleBreakingStrategy) -> &'static str {
    match strategy {
        CycleBreakingStrategy::ModelOrder => "MODEL_ORDER",
        CycleBreakingStrategy::Greedy => "GREEDY",
        CycleBreakingStrategy::DepthFirst => "DEPTH_FIRST",
        CycleBreakingStrategy::Interactive => "INTERACTIVE",
        CycleBreakingStrategy::GreedyModelOrder => "GREEDY_MODEL_ORDER",
    }
}

const fn layering_name(strategy: LayeringStrategy) -> &'static str {
    match strategy {
        LayeringStrategy::NetworkSimplex => "NETWORK_SIMPLEX",
        LayeringStrategy::LongestPath => "LONGEST_PATH",
        LayeringStrategy::LongestPathSource => "LONGEST_PATH_SOURCE",
        LayeringStrategy::CoffmanGraham => "COFFMAN_GRAHAM",
        LayeringStrategy::MinWidth => "MIN_WIDTH",
        LayeringStrategy::StretchWidth => "STRETCH_WIDTH",
        LayeringStrategy::Interactive => "INTERACTIVE",
    }
}

const fn node_placement_name(strategy: NodePlacementStrategy) -> &'static str {
    match strategy {
        NodePlacementStrategy::Simple => "SIMPLE",
        NodePlacementStrategy::Interactive => "INTERACTIVE",
        NodePlacementStrategy::NetworkSimplex => "NETWORK_SIMPLEX",
        NodePlacementStrategy::LinearSegments => "LINEAR_SEGMENTS",
        NodePlacementStrategy::BrandesKoepf => "BRANDES_KOEPF",
    }
}

const fn node_placement_alignment_name(alignment: NodePlacementAlignment) -> &'static str {
    match alignment {
        NodePlacementAlignment::None => "NONE",
        NodePlacementAlignment::LeftUp => "LEFTUP",
        NodePlacementAlignment::RightUp => "RIGHTUP",
        NodePlacementAlignment::LeftDown => "LEFTDOWN",
        NodePlacementAlignment::RightDown => "RIGHTDOWN",
        NodePlacementAlignment::Balanced => "BALANCED",
    }
}

const fn model_order_name(strategy: ModelOrderStrategy) -> &'static str {
    match strategy {
        ModelOrderStrategy::None => "NONE",
        ModelOrderStrategy::NodesAndEdges => "NODES_AND_EDGES",
        ModelOrderStrategy::PreferEdges => "PREFER_EDGES",
        ModelOrderStrategy::PreferNodes => "PREFER_NODES",
    }
}

const fn self_loop_distribution_name(strategy: SelfLoopDistributionStrategy) -> &'static str {
    match strategy {
        SelfLoopDistributionStrategy::North => "NORTH",
        SelfLoopDistributionStrategy::Equally => "EQUALLY",
        SelfLoopDistributionStrategy::NorthSouth => "NORTH_SOUTH",
    }
}

const fn self_loop_ordering_name(strategy: SelfLoopOrderingStrategy) -> &'static str {
    match strategy {
        SelfLoopOrderingStrategy::Stacked => "STACKED",
        SelfLoopOrderingStrategy::ReverseStacked => "REVERSE_STACKED",
        SelfLoopOrderingStrategy::Sequenced => "SEQUENCED",
    }
}

const fn layer_constraint_name(constraint: LayerConstraint) -> &'static str {
    match constraint {
        LayerConstraint::First => "FIRST",
        LayerConstraint::FirstSeparate => "FIRST_SEPARATE",
        LayerConstraint::Last => "LAST",
        LayerConstraint::LastSeparate => "LAST_SEPARATE",
    }
}

const fn port_alignment_name(alignment: PortAlignment) -> &'static str {
    match alignment {
        PortAlignment::Distributed => "DISTRIBUTED",
        PortAlignment::Justified => "JUSTIFIED",
        PortAlignment::Begin => "BEGIN",
        PortAlignment::Center => "CENTER",
        PortAlignment::End => "END",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: &str) -> Node {
        Node {
            id: id.to_owned(),
            kind: NodeKind::Leaf,
            label_text: None,
            container: ContainerNodeOptions::default(),
            width: 100.0,
            height: 40.0,
            parent: None,
            direction: None,
            hierarchy_handling: None,
            layer_constraint: None,
            port_alignment: None,
            label: None,
        }
    }

    fn two_node_graph() -> Graph {
        Graph {
            id: "root".to_owned(),
            direction: Direction::Down,
            spacing: Spacing::default(),
            options: LayoutOptions::default(),
            nodes: vec![leaf("a"), leaf("b")],
            edges: vec![Edge {
                id: "e".to_owned(),
                source: "a".to_owned(),
                target: "b".to_owned(),
                label: Some(Label {
                    width: 30.0,
                    height: 14.0,
                }),
                terminal_labels: Vec::new(),
                minlen: 1,
                inside_self_loops_yo: false,
            }],
        }
    }

    /// A chain of `depth` nested groups with one leaf at the bottom.
    fn deep_hierarchy(depth: usize) -> Graph {
        let mut graph = two_node_graph();
        graph.edges.clear();
        let mut nodes = Vec::with_capacity(depth + 1);
        for level in 0..depth {
            let mut group = leaf(&format!("g{level}"));
            group.kind = NodeKind::Group;
            group.parent = (level > 0).then(|| format!("g{}", level - 1));
            nodes.push(group);
        }
        let mut child = leaf("leaf");
        child.parent = Some(format!("g{}", depth - 1));
        nodes.push(child);
        graph.nodes = nodes;
        graph
    }

    /// A layered layout places the second node below the first and routes one edge between them.
    #[test]
    fn layered_layout_answers_absolute_geometry() {
        let result = layout(&two_node_graph()).expect("layout");
        let a = result.nodes.iter().find(|node| node.id == "a").expect("a");
        let b = result.nodes.iter().find(|node| node.id == "b").expect("b");
        assert!(b.y > a.y, "b sits below a: {b:?} vs {a:?}");
        assert!((a.x - b.x).abs() < 1.0, "one column: {a:?} {b:?}");
        let edge = result.edges.iter().find(|edge| edge.id == "e").expect("e");
        assert!(edge.points.len() >= 2, "route points: {edge:?}");
        let start = edge.points.first().expect("start");
        let end = edge.points.last().expect("end");
        assert!(start.y < end.y, "the route flows down: {edge:?}");
        assert_eq!(edge.labels.len(), 1, "one center label: {edge:?}");
    }

    /// A container owns its children, and the child center lands inside the container box.
    #[test]
    fn nested_layout_keeps_children_inside_the_container() {
        let mut graph = two_node_graph();
        graph.nodes[0].kind = NodeKind::Group;
        graph.nodes[1].parent = Some("a".to_owned());
        graph.edges.clear();
        let result = layout(&graph).expect("layout");
        let group = result.nodes.iter().find(|node| node.id == "a").expect("a");
        let child = result.nodes.iter().find(|node| node.id == "b").expect("b");
        assert!(group.width >= 100.0 && group.height >= 40.0, "{group:?}");
        assert!(
            child.x > group.x - group.width / 2.0,
            "{child:?} in {group:?}"
        );
    }

    /// The deepest accepted hierarchy does not overflow a small worker stack.
    #[test]
    fn deep_hierarchy_layout_is_small_stack_safe() {
        let depth = MAX_HIERARCHY_DEPTH;
        let graph = deep_hierarchy(depth);
        let result = std::thread::Builder::new()
            .name("elk-deep-hierarchy-small-stack".to_owned())
            .stack_size(512 * 1024)
            .spawn(move || layout(&graph))
            .unwrap()
            .join()
            .expect("deep hierarchy must not overflow the worker stack")
            .expect("layout");
        assert_eq!(result.nodes.len(), depth + 1);
    }

    /// A hierarchy past the accepted depth stops before the port walks it.
    #[test]
    fn deeper_hierarchy_is_rejected() {
        let graph = deep_hierarchy(MAX_HIERARCHY_DEPTH + 1);
        let error = layout(&graph).expect_err("too deep");
        assert!(
            matches!(error, Error::TooDeep { limit } if limit == MAX_HIERARCHY_DEPTH),
            "{error:?}"
        );
    }

    /// A seed of `0` never reaches ELK as an unseeded request.
    #[test]
    fn zero_seed_resolves_from_the_operation() {
        let mut graph = two_node_graph();
        graph.options.layered.random_seed = 0;
        let seed = ElkOperationSeed::from_operation_seed(NonZeroU64::MIN);
        let request = graph_to_json(&graph, Some(seed));
        let options = request.get("layoutOptions").expect("options");
        assert_eq!(
            options.get("elk.randomSeed").and_then(Value::as_i64),
            Some(1)
        );
    }
}
