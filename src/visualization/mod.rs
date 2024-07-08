// © 2023, ETH Zurich
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

pub mod drawer;
pub mod mir_graph;

use crate::{
    borrows::domain::{
        Borrow, BorrowKind, BorrowsState, MaybeOldPlace, PlaceSnapshot, RegionAbstraction,
    },
    free_pcs::{CapabilityKind, CapabilityLocal, CapabilitySummary},
    rustc_interface,
    utils::{Place, PlaceRepacker},
};
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    fs::File,
    io::{self, Write},
    ops::Deref,
    rc::Rc,
};

use rustc_interface::{
    borrowck::{
        borrow_set::BorrowSet,
        consumers::{
            calculate_borrows_out_of_scope_at_location, BorrowIndex, Borrows, LocationTable,
            PoloniusInput, PoloniusOutput, RegionInferenceContext,
        },
    },
    data_structures::fx::{FxHashMap, FxIndexMap},
    dataflow::{Analysis, ResultsCursor},
    index::IndexVec,
    middle::{
        mir::{
            self, Body, Local, Location, PlaceElem, Promoted, TerminatorKind, UnwindAction,
            VarDebugInfo, RETURN_PLACE,
        },
        ty::{self, GenericArgsRef, ParamEnv, RegionVid, TyCtxt},
    },
};

pub fn place_id<'tcx>(place: &Place<'tcx>) -> String {
    format!("{:?}", place)
}

struct GraphDrawer {
    file: File,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
struct NodeId(usize);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "n{}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct GraphNode {
    id: NodeId,
    node_type: NodeType,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum NodeType {
    PlaceNode {
        label: String,
        capability: Option<CapabilityKind>,
        location: Option<Location>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ReferenceEdgeType {
    RustcBorrow(BorrowIndex, RegionVid),
    PCS,
}

impl std::fmt::Display for ReferenceEdgeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RustcBorrow(borrow_index, region_vid) => {
                write!(f, "{:?}: {:?}", borrow_index, region_vid)
            }
            Self::PCS => write!(f, "PCS"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum GraphEdge {
    ReborrowEdge {
        borrowed_place: NodeId,
        assigned_place: NodeId,
    },
    ReferenceEdge {
        borrowed_place: NodeId,
        assigned_place: NodeId,
        edge_type: ReferenceEdgeType,
    },
    ProjectionEdge {
        source: NodeId,
        target: NodeId,
    },
}

struct Graph {
    nodes: Vec<GraphNode>,
    edges: HashSet<GraphEdge>,
}

impl Graph {
    fn new(nodes: Vec<GraphNode>, edges: HashSet<GraphEdge>) -> Self {
        Self { nodes, edges }
    }
}

pub fn get_source_name_from_local(local: &Local, debug_info: &[VarDebugInfo]) -> Option<String> {
    if local.as_usize() == 0 {
        return None;
    }
    debug_info.get(&local.as_usize() - 1).map(|source_info| {
        let name = source_info.name.as_str();
        let mut shadow_count = 0;
        for i in 0..local.as_usize() - 1 {
            if debug_info[i].name.as_str() == name {
                shadow_count += 1;
            }
        }
        if shadow_count == 0 {
            format!("{}", name)
        } else {
            format!("{}$shadow{}", name, shadow_count)
        }
    })
}

pub fn get_source_name_from_place<'tcx>(
    local: Local,
    projection: &[PlaceElem<'tcx>],
    debug_info: &[VarDebugInfo],
) -> Option<String> {
    get_source_name_from_local(&local, debug_info).map(|mut name| {
        let mut iter = projection.iter().peekable();
        while let Some(elem) = iter.next() {
            match elem {
                mir::ProjectionElem::Deref => {
                    if iter.peek().is_some() {
                        name = format!("(*{})", name);
                    } else {
                        name = format!("*{}", name);
                    }
                }
                mir::ProjectionElem::Field(field, _) => {
                    name = format!("{}.{}", name, field.as_usize());
                }
                mir::ProjectionElem::Index(_) => todo!(),
                mir::ProjectionElem::ConstantIndex {
                    offset,
                    min_length,
                    from_end,
                } => todo!(),
                mir::ProjectionElem::Subslice { from, to, from_end } => todo!(),
                mir::ProjectionElem::Downcast(d, v) => {
                    name = format!("downcast {:?} as {:?}", name, d);
                }
                mir::ProjectionElem::OpaqueCast(_) => todo!(),
            }
        }
        name
    })
}

struct GraphConstructor<'a, 'tcx> {
    summary: &'a CapabilitySummary<'tcx>,
    repacker: Rc<PlaceRepacker<'a, 'tcx>>,
    borrows_domain: &'a BorrowsState<'tcx>,
    borrow_set: &'a BorrowSet<'tcx>,
    inserted_nodes: Vec<(Place<'tcx>, Option<Location>)>,
    nodes: Vec<GraphNode>,
    edges: HashSet<GraphEdge>,
    rank: HashMap<NodeId, usize>,
}

impl<'a, 'tcx> GraphConstructor<'a, 'tcx> {
    fn new(
        summary: &'a CapabilitySummary<'tcx>,
        repacker: Rc<PlaceRepacker<'a, 'tcx>>,
        borrows_domain: &'a BorrowsState<'tcx>,
        borrow_set: &'a BorrowSet<'tcx>,
    ) -> Self {
        Self {
            summary,
            repacker,
            borrows_domain,
            borrow_set,
            inserted_nodes: vec![],
            nodes: vec![],
            edges: HashSet::new(),
            rank: HashMap::new(),
        }
    }

    fn existing_node_id(&self, place: Place<'tcx>, location: Option<Location>) -> Option<NodeId> {
        self.inserted_nodes
            .iter()
            .position(|(p, n)| *p == place && *n == location)
            .map(|idx| NodeId(idx))
    }

    fn node_id(&mut self, place: Place<'tcx>, location: Option<Location>) -> NodeId {
        if let Some(idx) = self.existing_node_id(place, location) {
            idx
        } else {
            self.inserted_nodes.push((place, location));
            NodeId(self.inserted_nodes.len() - 1)
        }
    }

    fn rank(&self, node: NodeId) -> usize {
        *self.rank.get(&node).unwrap_or(&usize::MAX)
    }

    fn insert_node(&mut self, node: GraphNode) {
        if !self.nodes.contains(&node) {
            self.nodes.push(node);
        }
    }

    fn insert_place_node(
        &mut self,
        place: Place<'tcx>,
        location: Option<Location>,
        kind: Option<CapabilityKind>,
    ) -> NodeId {
        if let Some(node_id) = self.existing_node_id(place, location) {
            return node_id;
        }
        let id = self.node_id(place, location);
        let label = get_source_name_from_place(
            place.local,
            place.projection,
            &self.repacker.body().var_debug_info,
        )
        .unwrap_or_else(|| format!("{:?}: {}", place, place.ty(*self.repacker).ty));
        let node = GraphNode {
            id,
            node_type: NodeType::PlaceNode {
                label,
                capability: kind,
                location,
            },
        };
        self.insert_node(node);
        self.rank.insert(id, place.local.as_usize());
        id
    }

    fn insert_place_and_previous_projections(
        &mut self,
        place: Place<'tcx>,
        location: Option<Location>,
        kind: Option<CapabilityKind>,
    ) -> NodeId {
        let node = self.insert_place_node(place, location, kind);
        if location.is_some() {
            return node;
        }
        let mut projection = place.projection;
        let mut last_node = node;
        while !projection.is_empty() {
            projection = &projection[..projection.len() - 1];
            let place = Place::new(place.local, &projection);
            let node = self.insert_place_node(place, None, None);
            self.edges.insert(GraphEdge::ProjectionEdge {
                source: node,
                target: last_node,
            });
            last_node = node.clone();
        }
        node
    }

    fn insert_maybe_old_place(&mut self, place: MaybeOldPlace<'tcx>) -> NodeId {
        match place {
            MaybeOldPlace::Current { place } => self.insert_place_and_previous_projections(
                place,
                None,
                self.capability_for_place(place),
            ),
            MaybeOldPlace::OldPlace(snapshot_place) => self.insert_snapshot_place(snapshot_place),
        }
    }

    fn insert_snapshot_place(&mut self, place: PlaceSnapshot<'tcx>) -> NodeId {
        let (at, cap) = if !self.borrows_domain.is_current(&place, self.repacker.body()) {
            (Some(place.at), None)
        } else {
            (None, self.capability_for_place(place.place))
        };
        self.insert_place_and_previous_projections(place.place, at, cap)
    }

    fn capability_for_place(&self, place: Place<'tcx>) -> Option<CapabilityKind> {
        match self.summary.get(place.local) {
            Some(CapabilityLocal::Allocated(projections)) => {
                projections.deref().get(&place).cloned()
            }
            _ => None,
        }
    }

    fn construct_graph(mut self) -> Graph {
        for (local, capability) in self.summary.iter().enumerate() {
            match capability {
                CapabilityLocal::Unallocated => {}
                CapabilityLocal::Allocated(projections) => {
                    for (place, kind) in projections.iter() {
                        self.insert_place_and_previous_projections(*place, None, Some(*kind));
                    }
                }
            }
        }
        for borrow in &self.borrows_domain.borrows {
            let borrowed_place = self.insert_snapshot_place(borrow.borrowed_place);
            let assigned_place = self.insert_snapshot_place(borrow.assigned_place);
            match borrow.kind {
                BorrowKind::Rustc(borrow_index) => {
                    let borrow_data = &self.borrow_set[borrow_index];
                    self.edges.insert(GraphEdge::ReferenceEdge {
                        borrowed_place,
                        assigned_place,
                        edge_type: ReferenceEdgeType::RustcBorrow(borrow_index, borrow_data.region),
                    });
                }
                BorrowKind::PCS { .. } => {
                    self.edges.insert(GraphEdge::ReferenceEdge {
                        borrowed_place,
                        assigned_place,
                        edge_type: ReferenceEdgeType::PCS,
                    });
                }
            }
        }
        for reborrow in self.borrows_domain.reborrows.iter() {
            let borrowed_place = self.insert_maybe_old_place(reborrow.blocked_place);
            let assigned_place = self.insert_maybe_old_place(reborrow.assigned_place);
            self.edges.insert(GraphEdge::ReborrowEdge {
                borrowed_place,
                assigned_place,
            });
        }

        let mut before_places: HashSet<(Place<'tcx>, Location)> = HashSet::new();
        for borrow in &self.borrows_domain.borrows {
            if !self
                .borrows_domain
                .is_current(&borrow.assigned_place, self.repacker.body())
            {
                before_places.insert((borrow.assigned_place.place, borrow.assigned_place.at));
            }
            if !self
                .borrows_domain
                .is_current(&borrow.borrowed_place, self.repacker.body())
            {
                before_places.insert((borrow.borrowed_place.place, borrow.borrowed_place.at));
            }
        }
        for (place, location) in before_places.iter() {
            for (place2, location2) in before_places.iter() {
                if location == location2 && place2.is_deref_of(*place) {
                    let source = self.node_id(*place, Some(*location));
                    let target = self.node_id(*place2, Some(*location));
                    self.edges
                        .insert(GraphEdge::ProjectionEdge { source, target });
                }
            }
        }

        let mut nodes = self.nodes.clone().into_iter().collect::<Vec<_>>();
        nodes.sort_by(|a, b| self.rank(a.id).cmp(&self.rank(b.id)));
        Graph::new(nodes, self.edges)
    }
}

pub fn generate_dot_graph<'a, 'tcx: 'a>(
    location: Location,
    repacker: Rc<PlaceRepacker<'a, 'tcx>>,
    summary: &CapabilitySummary<'tcx>,
    borrows_domain: &BorrowsState<'tcx>,
    borrow_set: &BorrowSet<'tcx>,
    input_facts: &PoloniusInput,
    file_path: &str,
) -> io::Result<()> {
    let constructor = GraphConstructor::new(summary, repacker, borrows_domain, borrow_set);
    let graph = constructor.construct_graph();
    let mut drawer = GraphDrawer::new(file_path);
    drawer.draw(graph)

    // for (idx, region_abstraction) in borrows_domain.region_abstractions.iter().enumerate() {
    //     let ra_node_label = format!("ra{}", idx);
    //     writeln!(
    //         drawer.file,
    //         "    \"{}\" [label=\"{}\", shape=egg];",
    //         ra_node_label, ra_node_label
    //     )?;
    //     for loan_in in &region_abstraction.loans_in {
    //         drawer.add_place_if_necessary((*loan_in).into())?;
    //         dot_edge(
    //             &mut drawer.file,
    //             &place_id(&(*loan_in).into()),
    //             &ra_node_label,
    //             "loan_in",
    //             false,
    //         )?;
    //     }
    //     for loan_out in &region_abstraction.loans_out {
    //         drawer.add_place_if_necessary((*loan_out).into())?;
    //         dot_edge(
    //             &mut drawer.file,
    //             &ra_node_label,
    //             &place_id(&(*loan_out).into()),
    //             "loan_out",
    //             false,
    //         )?;
    //     }
    // }
}
