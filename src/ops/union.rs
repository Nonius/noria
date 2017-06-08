use std::collections::HashMap;
use std::sync;

use flow::prelude::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Emit {
    AllFrom(NodeAddress),
    Project {
        emit: HashMap<NodeAddress, Vec<usize>>,
        cols: HashMap<NodeAddress, usize>,
    },
}

/// A union of a set of views.
#[derive(Debug, Serialize, Deserialize)]
pub struct Union {
    emit: Emit,
    replay_key: Option<Map<usize>>,
    replay_pieces: HashMap<DataType, Map<Records>>,
}

impl Clone for Union {
    fn clone(&self) -> Self {
        Union {
            emit: self.emit.clone(),
            // nothing can have been received yet
            replay_key: None,
            replay_pieces: HashMap::new(),
        }
    }
}

impl Union {
    /// Construct a new union operator.
    ///
    /// When receiving an update from node `a`, a union will emit the columns selected in `emit[a]`.
    /// `emit` only supports omitting columns, not rearranging them.
    pub fn new(emit: HashMap<NodeAddress, Vec<usize>>) -> Union {
        assert!(!emit.is_empty());
        for emit in emit.values() {
            let mut last = &emit[0];
            for i in emit {
                if i < last {
                    unimplemented!();
                }
                last = i;
            }
        }
        Union {
            emit: Emit::Project {
                emit,
                cols: HashMap::new(),
            },
            replay_key: None,
            replay_pieces: HashMap::new(),
        }
    }

    /// Construct a new union operator meant to de-shard a sharded data-flow subtree.
    pub fn new_deshard(parent: NodeAddress) -> Union {
        Union {
            emit: Emit::AllFrom(parent),
            replay_key: None,
            replay_pieces: HashMap::new(),
        }
    }
}

impl Ingredient for Union {
    fn take(&mut self) -> NodeOperator {
        Clone::clone(self).into()
    }

    fn ancestors(&self) -> Vec<NodeAddress> {
        match self.emit {
            Emit::AllFrom(p) => vec![p],
            Emit::Project { ref emit, .. } => emit.keys().cloned().collect(),
        }
    }

    fn should_materialize(&self) -> bool {
        false
    }

    fn will_query(&self, _: bool) -> bool {
        false
    }

    fn on_connected(&mut self, g: &Graph) {
        if let Emit::Project {
                   ref mut cols,
                   ref emit,
               } = self.emit {
            cols.extend(emit.keys().map(|&n| (n, g[*n.as_global()].fields().len())));
        }
    }

    fn on_commit(&mut self, _: NodeAddress, remap: &HashMap<NodeAddress, NodeAddress>) {
        match self.emit {
            Emit::Project {
                ref mut emit,
                ref mut cols,
            } => {
                for (from, to) in remap {
                    if from == to {
                        continue;
                    }

                    if let Some(e) = emit.remove(from) {
                        assert!(emit.insert(*to, e).is_none());
                    }
                    if let Some(e) = cols.remove(from) {
                        assert!(cols.insert(*to, e).is_none());
                    }
                }
            }
            Emit::AllFrom(ref mut p) => {
                *p = remap[p];
            }
        }
    }

    fn on_input(&mut self,
                from: NodeAddress,
                rs: Records,
                _: &mut Tracer,
                _: &DomainNodes,
                _: &StateMap)
                -> ProcessingResult {
        match self.emit {
            Emit::AllFrom(_) => {
                ProcessingResult {
                    results: rs,
                    misses: Vec::new(),
                }
            }
            Emit::Project { ref emit, .. } => {

                let rs = rs.into_iter()
                    .map(move |rec| {
                        let (r, pos) = rec.extract();

                        // yield selected columns for this source
                        // TODO: if emitting all in same order then avoid clone
                        let res = emit[&from].iter().map(|&col| r[col].clone()).collect();

                        // return new row with appropriate sign
                        if pos {
                            Record::Positive(sync::Arc::new(res))
                        } else {
                            Record::Negative(sync::Arc::new(res))
                        }
                    })
                    .collect();
                ProcessingResult {
                    results: rs,
                    misses: Vec::new(),
                }
            }
        }
    }

    fn on_input_raw(&mut self,
                    from: NodeAddress,
                    rs: Records,
                    tracer: &mut Tracer,
                    is_replay_of: Option<(usize, DataType)>,
                    nshards: usize,
                    n: &DomainNodes,
                    s: &StateMap)
                    -> RawProcessingResult {
        // NOTE: in the special case of us being a shard merge node (i.e., when
        // self.emit.is_empty()), `from` will *actually* hold the shard index of
        // the sharded egress that sent us this record. this should make everything
        // below just work out.
        match is_replay_of {
            None => {
                if self.replay_key.is_none() || self.replay_pieces.is_empty() {
                    // no replay going on, so we're done.
                    return RawProcessingResult::Regular(self.on_input(from, rs, tracer, n, s));
                }

                // partial replays are flowing through us, and at least one piece is being waited
                // for. we need to keep track of any records that succeed a replay piece (and thus
                // aren't included in it) before the other pieces come in. note that it's perfectly
                // safe for us to also forward them, since they'll just be dropped when they miss
                // in the downstream node. in fact, we *must* forward them, becuase there may be
                // *other* nodes downstream that do *not* have holes for the key in question.
                for r in &rs {
                    let k = self.replay_key.as_ref().unwrap()[from.as_local()];
                    if let Some(ref mut pieces) = self.replay_pieces.get_mut(&r[k]) {
                        if let Some(ref mut rs) = pieces.get_mut(from.as_local()) {
                            // we've received a replay piece from this ancestor already for this
                            // key, and are waiting for replay pieces from other ancestors. we need
                            // to incorporate this record into the replay piece so that it doesn't
                            // end up getting lost.
                            rs.push(r.clone());
                        } else {
                            // we haven't received a replay piece for this key from this ancestor
                            // yet, so we know that the eventual replay piece must include this
                            // record.
                        }
                    } else {
                        // we're not waiting on replay pieces for this key
                    }
                }

                RawProcessingResult::Regular(self.on_input(from, rs, tracer, n, s))
            }
            Some((key_col, key_val)) => {
                if self.replay_key.is_none() {
                    // the replay key is for our *output* column
                    // which might translate to different columns in our inputs
                    match self.emit {
                        Emit::AllFrom(_) => {
                            self.replay_key =
                                Some(Some((*from.as_local(), key_col)).into_iter().collect());
                        }
                        Emit::Project { ref emit, .. } => {
                            self.replay_key =
                                Some(emit.iter()
                                         .map(|(src, emit)| (*src.as_local(), emit[key_col]))
                                         .collect());
                        }
                    }
                }

                let finished = {
                    // store this replay piece
                    let pieces = self.replay_pieces
                        .entry(key_val.clone())
                        .or_insert_with(Map::new);
                    // there better be only one replay from each ancestor
                    assert!(!pieces.contains_key(from.as_local()));
                    pieces.insert(*from.as_local(), rs);
                    // does this release the replay?
                    match self.emit {
                        Emit::AllFrom(_) => pieces.len() == nshards,
                        Emit::Project { ref emit, .. } => pieces.len() == emit.len(),
                    }
                };

                if finished {
                    // yes! construct the final replay records.
                    // TODO: should we use a stolen tracer if none is given?
                    let rs = self.replay_pieces
                        .remove(&key_val)
                        .unwrap()
                        .into_iter()
                        .flat_map(|(from, rs)| self.on_input(from, rs, tracer, n, s).results)
                        .collect();

                    RawProcessingResult::ReplayPiece(rs)
                } else {
                    // no. need to keep buffering (and emit nothing)
                    RawProcessingResult::Captured
                }
            }
        }
    }

    fn suggest_indexes(&self, _: NodeAddress) -> HashMap<NodeAddress, Vec<usize>> {
        // index nothing (?)
        HashMap::new()
    }

    fn resolve(&self, col: usize) -> Option<Vec<(NodeAddress, usize)>> {
        match self.emit {
            Emit::AllFrom(p) => Some(vec![(p, col)]),
            Emit::Project { ref emit, .. } => {
                Some(emit.iter().map(|(src, emit)| (*src, emit[col])).collect())
            }
        }
    }

    fn description(&self) -> String {
        // Ensure we get a consistent output by sorting.
        match self.emit {
            Emit::AllFrom(_) => "⊍".to_string(),
            Emit::Project { ref emit, .. } => {
                let mut emit = emit.iter().collect::<Vec<_>>();
                emit.sort();
                emit.iter()
                    .map(|&(src, emit)| {
                             let cols = emit.iter()
                                 .map(|e| e.to_string())
                                 .collect::<Vec<_>>()
                                 .join(", ");
                             format!("{}:[{}]", src, cols)
                         })
                    .collect::<Vec<_>>()
                    .join(" ⋃ ")
            }
        }
    }
    fn parent_columns(&self, col: usize) -> Vec<(NodeAddress, Option<usize>)> {
        match self.emit {
            Emit::AllFrom(p) => vec![(p, Some(col))],
            Emit::Project { ref emit, .. } => {
                emit.iter()
                    .map(|(src, emit)| (*src, Some(emit[col])))
                    .collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ops;

    fn setup() -> (ops::test::MockGraph, NodeAddress, NodeAddress) {
        let mut g = ops::test::MockGraph::new();
        let l = g.add_base("left", &["l0", "l1"]);
        let r = g.add_base("right", &["r0", "r1", "r2"]);

        let mut emits = HashMap::new();
        emits.insert(l, vec![0, 1]);
        emits.insert(r, vec![0, 2]);
        g.set_op("union", &["u0", "u1"], Union::new(emits), false);

        let (l, r) = (g.to_local(l), g.to_local(r));
        (g, l, r)
    }

    #[test]
    fn it_describes() {
        let (u, l, r) = setup();
        assert_eq!(u.node().description(),
                   format!("{}:[0, 1] ⋃ {}:[0, 2]", l, r));
    }

    #[test]
    fn it_works() {
        let (mut u, l, r) = setup();

        // forward from left should emit original record
        let left = vec![1.into(), "a".into()];
        assert_eq!(u.one_row(l, left.clone(), false), vec![left].into());

        // forward from right should emit subset record
        let right = vec![1.into(), "skipped".into(), "x".into()];
        assert_eq!(u.one_row(r, right.clone(), false),
                   vec![vec![1.into(), "x".into()]].into());
    }

    #[test]
    fn it_suggests_indices() {
        use std::collections::HashMap;
        let (u, _, _) = setup();
        let me = NodeAddress::mock_global(1.into());
        assert_eq!(u.node().suggest_indexes(me), HashMap::new());
    }

    #[test]
    fn it_resolves() {
        let (u, l, r) = setup();
        let r0 = u.node().resolve(0);
        assert!(r0.as_ref().unwrap().iter().any(|&(n, c)| n == l && c == 0));
        assert!(r0.as_ref().unwrap().iter().any(|&(n, c)| n == r && c == 0));
        let r1 = u.node().resolve(1);
        assert!(r1.as_ref().unwrap().iter().any(|&(n, c)| n == l && c == 1));
        assert!(r1.as_ref().unwrap().iter().any(|&(n, c)| n == r && c == 2));
    }
}
