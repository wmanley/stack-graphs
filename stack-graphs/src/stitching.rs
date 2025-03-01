// -*- coding: utf-8 -*-
// ------------------------------------------------------------------------------------------------
// Copyright © 2021, stack-graphs authors.
// Licensed under either of Apache License, Version 2.0, or MIT license, at your option.
// Please see the LICENSE-APACHE or LICENSE-MIT files in this distribution for license details.
// ------------------------------------------------------------------------------------------------

//! Partial paths can be "stitched together" to produce name-binding paths.
//!
//! The "path stitching" algorithm defined in this module is how we take a collection of [partial
//! paths][] and use them to build up name-binding paths.  Our conjecture is that by building paths
//! this way, we can precompute a useful amount of work at _index time_ (when we construct the
//! partial paths), to reduce the amount of work that needs to be done at _query time_ (when those
//! partial paths are stitched together into paths).
//!
//! Complicating this story is that for large codebases (especially those with many upstream and
//! downstream dependencies), there is a _very_ large set of partial paths available to us.  We
//! want to be able to load those in _lazily_, during the execution of the path-stitching
//! algorithm.
//!
//! The [`Database`][] and [`PathStitcher`][] types provide this functionality.  `Database`
//! manages a collection of partial paths that have been loaded into this process from some
//! external data store.  `PathStitcher` implements the path-stitching algorithm in _phases_.
//! During each phase, we will process a set of (possibly incomplete) paths, looking in the
//! `Database` for the set of partial paths that are compatible with those paths.  It is your
//! responsibility to make sure that the database contains all of possible extensions of the paths
//! that we're going to process in that phase.  For the first phase, you already know which
//! paths you're starting the search from, and must make sure that the database starts out
//! containing the possible extensions of those "seed" paths.  For subsequent phases, you get to
//! see which paths will be processed in the _next_ phase as part of handling the _current_ phase.
//! This gives you the opporunity to load additional partial paths into the `Database` before
//! allowing the next phase to proceed.
//!
//! [partial paths]: ../partial/index.html
//! [`Database`]: struct.Database.html
//! [`PathStitcher`]: struct.PathStitcher.html

use std::collections::HashMap;
use std::collections::VecDeque;
#[cfg(feature = "copious-debugging")]
use std::fmt::Display;

use crate::arena::Arena;
use crate::arena::Handle;
use crate::arena::HandleSet;
use crate::arena::List;
use crate::arena::ListArena;
use crate::arena::ListCell;
use crate::arena::SupplementalArena;
use crate::cycles::Appendables;
use crate::cycles::AppendingCycleDetector;
use crate::cycles::SimilarPathDetector;
use crate::graph::Edge;
use crate::graph::File;
use crate::graph::Node;
use crate::graph::StackGraph;
use crate::graph::Symbol;
use crate::partial::Cyclicity;
use crate::partial::PartialPath;
use crate::partial::PartialPaths;
use crate::partial::PartialSymbolStack;
use crate::paths::PathResolutionError;
use crate::CancellationError;
use crate::CancellationFlag;

//-------------------------------------------------------------------------------------------------
// Appendable

/// Something that can be appended to a partial path.
pub trait Appendable {
    /// Append this appendable to the given path. Resolving jump nodes and renaming unused_variables
    /// is part of the responsibility of this method.
    fn append_to(
        &self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        path: &mut PartialPath,
    ) -> Result<(), PathResolutionError>;

    /// Return the start node.
    fn start_node(&self) -> Handle<Node>;

    /// Return the end node.
    fn end_node(&self) -> Handle<Node>;

    /// Return a Display implementation.
    fn display<'a>(
        &'a self,
        graph: &'a StackGraph,
        partials: &'a mut PartialPaths,
    ) -> Box<dyn std::fmt::Display + 'a>;
}

impl Appendable for Edge {
    fn append_to(
        &self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        path: &mut PartialPath,
    ) -> Result<(), PathResolutionError> {
        path.resolve_to_node(graph, partials, self.source)?;
        path.append(graph, partials, *self)
    }

    fn start_node(&self) -> Handle<Node> {
        self.source
    }

    fn end_node(&self) -> Handle<Node> {
        self.sink
    }

    fn display<'a>(
        &'a self,
        graph: &'a StackGraph,
        _partials: &'a mut PartialPaths,
    ) -> Box<dyn std::fmt::Display + 'a> {
        Box::new(format!(
            "{} -> {}",
            self.source.display(graph),
            self.sink.display(graph)
        ))
    }
}

impl Appendable for PartialPath {
    fn append_to(
        &self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        path: &mut PartialPath,
    ) -> Result<(), PathResolutionError> {
        path.resolve_to_node(graph, partials, self.start_node)?;
        path.ensure_no_overlapping_variables(partials, self);
        path.concatenate(graph, partials, self)?;
        Ok(())
    }

    fn start_node(&self) -> Handle<Node> {
        self.start_node
    }

    fn end_node(&self) -> Handle<Node> {
        self.end_node
    }

    fn display<'a>(
        &'a self,
        graph: &'a StackGraph,
        partials: &'a mut PartialPaths,
    ) -> Box<dyn std::fmt::Display + 'a> {
        Box::new(self.display(graph, partials))
    }
}

//-------------------------------------------------------------------------------------------------
// ToAppendable

/// A trait to be implemented on types such as [`Database`][] that allow converting handles
/// to appendables.
///
/// It is very similar to the [`std::ops::Index`] trait, but returns a reference instead
/// of a value, such that an efficient identifity implementation is possible, that doesn't
/// require cloning values.
pub trait ToAppendable<H, A>
where
    A: Appendable,
{
    fn get_appendable<'a>(&'a self, handle: &'a H) -> &'a A;
}

//-------------------------------------------------------------------------------------------------
// Candidates

/// A trait to support finding candidates for partial path extension. Requires an accompanying
/// [`ToAppendable`] implementation to convert the candidate handles into [`Appendable`]s.
pub trait Candidates<H> {
    fn find_candidates<R>(
        &mut self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        path: &PartialPath,
        result: &mut R,
    ) where
        R: std::iter::Extend<H>;
}

//-------------------------------------------------------------------------------------------------
// FileEdges

/// Acts as a database of the edges in the graph.
pub struct GraphEdges(pub Option<Handle<File>>);

impl ToAppendable<Edge, Edge> for GraphEdges {
    fn get_appendable<'a>(&'a self, value: &'a Edge) -> &'a Edge {
        value
    }
}

impl Candidates<Edge> for GraphEdges {
    fn find_candidates<R>(
        &mut self,
        graph: &StackGraph,
        _partials: &mut PartialPaths,
        path: &PartialPath,
        result: &mut R,
    ) where
        R: std::iter::Extend<Edge>,
    {
        result.extend(
            graph
                .outgoing_edges(path.end_node)
                .filter(|e| self.0.map_or(true, |file| graph[e.sink].is_in_file(file))),
        );
    }
}

//-------------------------------------------------------------------------------------------------
// Databases

/// Contains a "database" of partial paths.
///
/// This type is meant to be a lazily loaded "view" into a proper storage layer.  During the
/// path-stitching algorithm, we repeatedly try to extend a currently incomplete path with any
/// partial paths that are compatible with it.  For large codebases, or projects with a large
/// number of dependencies, it can be prohibitive to load in _all_ of the partial paths up-front.
/// We've written the path-stitching algorithm so that you have a chance to only load in the
/// partial paths that are actually needed, placing them into a `Database` instance as they're
/// needed.
pub struct Database {
    pub(crate) partial_paths: Arena<PartialPath>,
    pub(crate) local_nodes: HandleSet<Node>,
    symbol_stack_keys: ListArena<Handle<Symbol>>,
    symbol_stack_key_cache: HashMap<SymbolStackCacheKey, SymbolStackKeyHandle>,
    paths_by_start_node: SupplementalArena<Node, Vec<Handle<PartialPath>>>,
    root_paths_by_precondition: SupplementalArena<SymbolStackKeyCell, Vec<Handle<PartialPath>>>,
}

impl Database {
    /// Creates a new, empty database.
    pub fn new() -> Database {
        Database {
            partial_paths: Arena::new(),
            local_nodes: HandleSet::new(),
            symbol_stack_keys: List::new_arena(),
            symbol_stack_key_cache: HashMap::new(),
            paths_by_start_node: SupplementalArena::new(),
            root_paths_by_precondition: SupplementalArena::new(),
        }
    }

    /// Clear the database.  After this, all previous handles into the database are
    /// invalid.
    #[cfg_attr(not(feature = "storage"), allow(dead_code))]
    pub(crate) fn clear(&mut self) {
        self.partial_paths.clear();
        self.local_nodes.clear();
        self.symbol_stack_keys.clear();
        self.symbol_stack_key_cache.clear();
        self.paths_by_start_node.clear();
        self.root_paths_by_precondition.clear();
    }

    /// Adds a partial path to this database.  We do not deduplicate partial paths in any way; it's
    /// your responsibility to only add each partial path once.
    pub fn add_partial_path(
        &mut self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        path: PartialPath,
    ) -> Handle<PartialPath> {
        let start_node = path.start_node;
        copious_debugging!(
            "    Add {} path to database {}",
            if graph[start_node].is_root() {
                "root"
            } else {
                "node"
            },
            path.display(graph, partials)
        );
        let symbol_stack_precondition = path.symbol_stack_precondition;
        let handle = self.partial_paths.add(path);

        // If the partial path starts at the root node, index it by its symbol stack precondition.
        if graph[start_node].is_root() {
            // The join node is root, so there's no need to use half-open symbol stacks here, as we
            // do for [`PartialPath::concatenate`][].
            let key = SymbolStackKey::from_partial_symbol_stack(
                partials,
                self,
                symbol_stack_precondition,
            );
            if !key.is_empty() {
                let key_handle = key.back_handle();
                self.root_paths_by_precondition[key_handle].push(handle);
            }
        } else {
            // Otherwise index it by its source node.
            self.paths_by_start_node[start_node].push(handle);
        }

        handle
    }

    /// Find all partial paths in this database that start at the root node, and have a symbol
    /// stack precondition that is compatible with a given symbol stack.
    #[cfg_attr(not(feature = "copious-debugging"), allow(unused_variables))]
    pub fn find_candidate_partial_paths_from_root<R>(
        &mut self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        symbol_stack: Option<SymbolStackKey>,
        result: &mut R,
    ) where
        R: std::iter::Extend<Handle<PartialPath>>,
    {
        // If the path currently ends at the root node, then we need to look up partial paths whose
        // symbol stack precondition is compatible with the path.
        match symbol_stack {
            Some(mut symbol_stack) => loop {
                copious_debugging!(
                    "      Search for symbol stack <{}>",
                    symbol_stack.display(graph, self)
                );
                let key_handle = symbol_stack.back_handle();
                if let Some(paths) = self.root_paths_by_precondition.get(key_handle) {
                    #[cfg(feature = "copious-debugging")]
                    {
                        for path in paths {
                            copious_debugging!(
                                "        Found path {}",
                                self[*path].display(graph, partials)
                            );
                        }
                    }
                    result.extend(paths.iter().copied());
                }
                if symbol_stack.pop_back(self).is_none() {
                    break;
                }
            },
            None => {
                copious_debugging!("      Search for all root paths");
                for (_, paths) in self.root_paths_by_precondition.iter() {
                    #[cfg(feature = "copious-debugging")]
                    {
                        for path in paths {
                            copious_debugging!(
                                "        Found path {}",
                                self[*path].display(graph, partials)
                            );
                        }
                    }
                    result.extend(paths.iter().copied());
                }
            }
        }
    }

    /// Find all partial paths in the database that start at the given node.  We don't filter the
    /// results any further than that, since we have to check each partial path for compatibility
    /// as we try to append it to the current incomplete path anyway, and non-root nodes will
    /// typically have a small number of outgoing edges.
    #[cfg_attr(not(feature = "copious-debugging"), allow(unused_variables))]
    pub fn find_candidate_partial_paths_from_node<R>(
        &self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        start_node: Handle<Node>,
        result: &mut R,
    ) where
        R: std::iter::Extend<Handle<PartialPath>>,
    {
        copious_debugging!("      Search for start node {}", start_node.display(graph));
        // Return all of the partial paths that start at the requested node.
        if let Some(paths) = self.paths_by_start_node.get(start_node) {
            #[cfg(feature = "copious-debugging")]
            {
                for path in paths {
                    copious_debugging!(
                        "        Found path {}",
                        self[*path].display(graph, partials)
                    );
                }
            }
            result.extend(paths.iter().copied());
        }
    }

    /// Determines which nodes in the stack graph are “local”, taking into account the partial
    /// paths in this database.
    ///
    /// A local node has no partial path that connects it to the root node in either direction.
    /// That means that it cannot participate in any paths that leave the file.
    ///
    /// This method is meant to be used at index time, to calculate the set of nodes that are local
    /// after having just calculated the set of partial paths for the file.
    pub fn find_local_nodes(&mut self) {
        // Assume that any node that is the start or end of a partial path is local to this file
        // until we see a path connecting the root node to it (in either direction).
        self.local_nodes.clear();
        for handle in self.iter_partial_paths() {
            self.local_nodes.add(self[handle].start_node);
            self.local_nodes.add(self[handle].end_node);
        }

        // The root node and jump-to-scope node are the most obvious non-local nodes.
        let mut nonlocal_start_nodes = HandleSet::new();
        let mut nonlocal_end_nodes = HandleSet::new();
        self.local_nodes.remove(StackGraph::root_node());
        nonlocal_start_nodes.add(StackGraph::root_node());
        nonlocal_end_nodes.add(StackGraph::root_node());
        self.local_nodes.remove(StackGraph::jump_to_node());
        nonlocal_start_nodes.add(StackGraph::jump_to_node());
        nonlocal_end_nodes.add(StackGraph::jump_to_node());

        // Other nodes are non-local if we see any partial path that connects it to another
        // non-local node.  Repeat until we reach a fixed point.
        let mut keep_checking = true;
        while keep_checking {
            keep_checking = false;
            for handle in self.iter_partial_paths() {
                let start_node = self[handle].start_node;
                let end_node = self[handle].end_node;

                // First check forwards paths, where non-localness propagates from the start node
                // of each path.
                let start_node_is_nonlocal = nonlocal_start_nodes.contains(start_node);
                let end_node_is_nonlocal = nonlocal_start_nodes.contains(end_node);
                if start_node_is_nonlocal && !end_node_is_nonlocal {
                    keep_checking = true;
                    nonlocal_start_nodes.add(end_node);
                    self.local_nodes.remove(end_node);
                }

                // Then check reverse paths, where non-localness propagates from the end node of
                // each path.
                let start_node_is_nonlocal = nonlocal_end_nodes.contains(start_node);
                let end_node_is_nonlocal = nonlocal_end_nodes.contains(end_node);
                if !start_node_is_nonlocal && end_node_is_nonlocal {
                    keep_checking = true;
                    nonlocal_end_nodes.add(start_node);
                    self.local_nodes.remove(start_node);
                }
            }
        }
    }

    /// Marks that a stack graph node is local.
    ///
    /// This method is meant to be used at query time.  You will have precalculated the set of
    /// local nodes for a file at index time; at query time, you will load this information from
    /// your storage layer and use this method to update our internal view of which nodes are
    /// local.
    pub fn mark_local_node(&mut self, node: Handle<Node>) {
        self.local_nodes.add(node);
    }

    /// Returns whether a node is local according to the partial paths in this database.  You must
    /// have already called [`find_local_nodes`][] or [`mark_local_node`][], depending on whether
    /// it is index time or query time.
    pub fn node_is_local(&self, node: Handle<Node>) -> bool {
        self.local_nodes.contains(node)
    }

    /// Returns an iterator over all of the handles of all of the partial paths in this database.
    /// (Note that because we're only returning _handles_, this iterator does not retain a
    /// reference to the `Database`.)
    pub fn iter_partial_paths(&self) -> impl Iterator<Item = Handle<PartialPath>> {
        self.partial_paths.iter_handles()
    }

    pub fn ensure_both_directions(&mut self, partials: &mut PartialPaths) {
        for path in self.partial_paths.iter_handles() {
            self.partial_paths
                .get_mut(path)
                .ensure_both_directions(partials);
        }
    }

    pub fn ensure_forwards(&mut self, partials: &mut PartialPaths) {
        for path in self.partial_paths.iter_handles() {
            self.partial_paths.get_mut(path).ensure_forwards(partials);
        }
    }
}

impl std::ops::Index<Handle<PartialPath>> for Database {
    type Output = PartialPath;
    #[inline(always)]
    fn index(&self, handle: Handle<PartialPath>) -> &PartialPath {
        self.partial_paths.get(handle)
    }
}

impl ToAppendable<Handle<PartialPath>, PartialPath> for Database {
    fn get_appendable<'a>(&'a self, handle: &'a Handle<PartialPath>) -> &'a PartialPath {
        &self[*handle]
    }
}

impl Candidates<Handle<PartialPath>> for Database {
    fn find_candidates<R>(
        &mut self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        path: &PartialPath,
        result: &mut R,
    ) where
        R: std::iter::Extend<Handle<PartialPath>>,
    {
        if graph[path.end_node].is_root() {
            // The join node is root, so there's no need to use half-open symbol stacks here, as we
            // do for [`PartialPath::concatenate`][].
            let key = SymbolStackKey::from_partial_symbol_stack(
                partials,
                self,
                path.symbol_stack_postcondition,
            );
            self.find_candidate_partial_paths_from_root(graph, partials, Some(key), result);
        } else {
            self.find_candidate_partial_paths_from_node(graph, partials, path.end_node, result);
        }
    }
}

/// The key type that we use to find partial paths that start from the root node and have a
/// particular symbol stack as their precondition.
#[derive(Clone, Copy)]
pub struct SymbolStackKey {
    // Note: the symbols are stored in reverse order, with the "front" of the List being the "back"
    // of the symbol stack.  That lets us easily get a handle to the back of the symbol stack, and
    // also lets us easily pops items off the back of key, which we need to do to search for all
    // prefixes of a particular symbol stack down in `find_candidate_partial_paths_from_root`.
    symbols: List<Handle<Symbol>>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct SymbolStackCacheKey {
    head: Handle<Symbol>,
    tail: SymbolStackKeyHandle,
}

type SymbolStackKeyCell = ListCell<Handle<Symbol>>;
type SymbolStackKeyHandle = Handle<SymbolStackKeyCell>;

impl SymbolStackKey {
    /// Returns an empty symbol stack key.
    fn empty() -> SymbolStackKey {
        SymbolStackKey {
            symbols: List::empty(),
        }
    }

    fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    /// Pushes a new symbol onto the back of this symbol stack key.
    fn push_back(&mut self, db: &mut Database, symbol: Handle<Symbol>) {
        let cache_key = SymbolStackCacheKey {
            head: symbol,
            tail: self.back_handle(),
        };
        if let Some(handle) = db.symbol_stack_key_cache.get(&cache_key) {
            self.symbols = List::from_handle(*handle);
            return;
        }
        // push_front because we store the key's symbols in reverse order.
        self.symbols.push_front(&mut db.symbol_stack_keys, symbol);
        let handle = self.back_handle();
        db.symbol_stack_key_cache.insert(cache_key, handle);
    }

    /// Pops a symbol from the back of this symbol stack key.
    fn pop_back(&mut self, db: &Database) -> Option<Handle<Symbol>> {
        // pop_front because we store the key's symbols in reverse order.
        self.symbols.pop_front(&db.symbol_stack_keys).copied()
    }

    /// Extracts a new symbol stack key from a partial symbol stack.
    pub fn from_partial_symbol_stack(
        partials: &mut PartialPaths,
        db: &mut Database,
        mut stack: PartialSymbolStack,
    ) -> SymbolStackKey {
        let mut result = SymbolStackKey::empty();
        while let Some(symbol) = stack.pop_front(partials) {
            result.push_back(db, symbol.symbol);
        }
        result
    }

    /// Returns a handle to the back of the symbol stack key.
    fn back_handle(self) -> SymbolStackKeyHandle {
        // Because the symbols are stored in reverse order, the handle to the "front" of the list
        // is a handle to the "back" of the key.
        self.symbols.handle()
    }

    #[cfg(feature = "copious-debugging")]
    fn display<'a>(self, graph: &'a StackGraph, db: &'a Database) -> impl Display + 'a {
        DisplaySymbolStackKey(self, graph, db)
    }
}

#[cfg(feature = "copious-debugging")]
struct DisplaySymbolStackKey<'a>(SymbolStackKey, &'a StackGraph, &'a Database);

#[cfg(feature = "copious-debugging")]
impl<'a> Display for DisplaySymbolStackKey<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // Use a recursive function to print the contents of the key out in reverse order.
        fn display_one(
            mut key: SymbolStackKey,
            graph: &StackGraph,
            db: &Database,
            f: &mut std::fmt::Formatter,
        ) -> std::fmt::Result {
            let last = match key.pop_back(db) {
                Some(last) => last,
                None => return Ok(()),
            };
            display_one(key, graph, db, f)?;
            last.display(graph).fmt(f)
        }
        display_one(self.0, self.1, self.2, f)
    }
}

//-------------------------------------------------------------------------------------------------
// Stitching partial paths together

/// Implements a phased forward partial path stitching algorithm.
///
/// Our overall goal is to start with a set of _seed_ partial paths, and to repeatedly extend each
/// partial path by concatenating another, compatible partial path onto the end of it.  (If there
/// are multiple compatible partial paths, we concatenate each of them separately, resulting in
/// more than one extension for the current path.)
///
/// We perform this processing in _phases_.  At the start of each phase, we have a _current set_ of
/// partial paths that need to be processed.  As we extend those partial paths, we add the
/// extensions to the set of partial paths to process in the _next_ phase.  Phases are processed
/// one at a time, each time you invoke the [`process_next_phase`][] method.
///
/// [`process_next_phase`]: #method.process_next_phase
///
/// After each phase has completed, you can use the [`previous_phase_partial_paths`][] method to
/// retrieve all of the partial paths that were discovered during that phase.  That gives you a
/// chance to add to the `Database` all of the other partial paths that we might need to extend
/// those partial paths with before invoking the next phase.
///
/// [`previous_phase_partial_paths`]: #method.previous_phase_partial_paths
///
/// If you don't care about this phasing nonsense, you can instead preload your `Database` with all
/// possible partial paths, and run the forward partial path stitching algorithm all the way to
/// completion, using the [`find_all_complete_partial_paths`][] method.
///
/// [`find_all_complete_partial_paths`]: #method.find_all_complete_partial_paths
pub struct ForwardPartialPathStitcher<H> {
    candidates: Vec<H>,
    queue: VecDeque<(PartialPath, AppendingCycleDetector<H>)>,
    // tracks the number of initial paths in the queue because we do not want call
    // extend_until on those
    initial_paths: usize,
    // next_iteration is a tuple of queues instead of an queue of tuples so that the path queue
    // can be cheaply exposed through the C API as a continuous memory block
    next_iteration: (VecDeque<PartialPath>, VecDeque<AppendingCycleDetector<H>>),
    appended_paths: Appendables<H>,
    similar_path_detector: Option<SimilarPathDetector<PartialPath>>,
    max_work_per_phase: usize,
    #[cfg(feature = "copious-debugging")]
    phase_number: usize,
}

impl<H> ForwardPartialPathStitcher<H> {
    /// Creates a new forward partial path stitcher that is "seeded" with a set of initial partial
    /// paths. If the sticher is used to find complete paths, it is the responsibility of the caller
    /// to ensure precondition variables are eliminated by calling [`PartialPath::eliminate_precondition_stack_variables`][].
    pub fn from_partial_paths<I>(
        _graph: &StackGraph,
        _partials: &mut PartialPaths,
        initial_partial_paths: I,
    ) -> Self
    where
        I: IntoIterator<Item = PartialPath>,
    {
        let mut appended_paths = Appendables::new();
        let next_iteration: (VecDeque<_>, VecDeque<_>) = initial_partial_paths
            .into_iter()
            .map(|p| {
                let c = AppendingCycleDetector::from(&mut appended_paths, p.clone().into());
                (p, c)
            })
            .unzip();
        Self {
            candidates: Vec::new(),
            queue: VecDeque::new(),
            initial_paths: next_iteration.0.len(),
            next_iteration,
            appended_paths,
            similar_path_detector: None,
            // By default, there's no artificial bound on the amount of work done per phase
            max_work_per_phase: usize::MAX,
            #[cfg(feature = "copious-debugging")]
            phase_number: 1,
        }
    }
}

impl<H: Clone> ForwardPartialPathStitcher<H> {
    /// Returns an iterator of all of the (possibly incomplete) partial paths that were encountered
    /// during the most recent phase of the algorithm.
    pub fn previous_phase_partial_paths(&self) -> impl Iterator<Item = &PartialPath> + '_ {
        self.next_iteration.0.iter()
    }

    /// Returns a slice of all of the (possibly incomplete) partial paths that were encountered
    /// during the most recent phase of the algorithm.
    pub fn previous_phase_partial_paths_slice(&mut self) -> &[PartialPath] {
        self.next_iteration.0.make_contiguous();
        self.next_iteration.0.as_slices().0
    }

    /// Returns a mutable slice of all of the (possibly incomplete) partial paths that were
    /// encountered during the most recent phase of the algorithm.
    pub fn previous_phase_partial_paths_slice_mut(&mut self) -> &mut [PartialPath] {
        self.next_iteration.0.make_contiguous();
        self.next_iteration.0.as_mut_slices().0
    }

    /// Sets whether similar path detection should be enabled during path stitching. Paths are similar
    /// if start and end node, and pre- and postconditions are the same. The presence of similar paths
    /// can lead to exponential blow up during path stitching. Similar path detection is disabled by
    /// default because of the associated preformance cost.
    pub fn set_similar_path_detection(&mut self, detect_similar_paths: bool) {
        if detect_similar_paths {
            self.similar_path_detector = Some(SimilarPathDetector::new());
        } else {
            self.similar_path_detector = None;
        }
    }

    /// Sets the maximum amount of work that can be performed during each phase of the algorithm.
    /// By bounding our work this way, you can ensure that it's not possible for our CPU-bound
    /// algorithm to starve any worker threads or processes that you might be using.  If you don't
    /// call this method, then we allow ourselves to process all of the extensions of all of the
    /// paths found in the previous phase, with no additional bound.
    pub fn set_max_work_per_phase(&mut self, max_work_per_phase: usize) {
        self.max_work_per_phase = max_work_per_phase;
    }

    /// Attempts to extend one partial path as part of the algorithm.  When calling this function,
    /// you are responsible for ensuring that `db` already contains all of the possible appendables
    /// that we might want to extend `partial_path` with.
    fn extend<A, Db>(
        &mut self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        db: &mut Db,
        partial_path: &PartialPath,
        cycle_detector: AppendingCycleDetector<H>,
    ) -> usize
    where
        A: Appendable,
        Db: Candidates<H> + ToAppendable<H, A>,
    {
        copious_debugging!("    Extend {}", partial_path.display(graph, partials));

        // check is path is cyclic, in which case we do not extend it
        let has_precondition_variables = partial_path.symbol_stack_precondition.has_variable()
            || partial_path.scope_stack_precondition.has_variable();
        let cycles = cycle_detector
            .is_cyclic(graph, partials, db, &mut self.appended_paths)
            .expect("cyclic test failed when stitching partial paths");
        let cyclic = match has_precondition_variables {
            // If the precondition has no variables, we allow cycles that strengthen the
            // precondition, because we know they cannot strengthen the precondition of
            // the overall path.
            false => !cycles
                .into_iter()
                .all(|c| c == Cyclicity::StrengthensPrecondition),
            // If the precondition has variables, do not allow any cycles, not even those
            // that strengthen the precondition. This is more strict than necessary. Better
            // might be to disallow precondition strengthening cycles only if they would
            // strengthen the overall path precondition.
            true => !cycles.is_empty(),
        };
        if cyclic {
            copious_debugging!("      is discontinued: cyclic");
            return 0;
        }

        // find candidates to append
        self.candidates.clear();
        db.find_candidates(graph, partials, partial_path, &mut self.candidates);

        // try to extend path with candidates
        let extension_count = self.candidates.len();
        self.next_iteration.0.reserve(extension_count);
        self.next_iteration.1.reserve(extension_count);
        for extension in &self.candidates {
            let extension_path = db.get_appendable(extension);
            copious_debugging!("      with {}", extension_path.display(graph, partials));

            let mut new_partial_path = partial_path.clone();
            let mut new_cycle_detector = cycle_detector.clone();
            // If there are errors concatenating these partial paths, or resolving the resulting
            // partial path, just skip the extension — it's not a fatal error.
            #[cfg_attr(not(feature = "copious-debugging"), allow(unused_variables))]
            {
                if let Err(err) = extension_path.append_to(graph, partials, &mut new_partial_path) {
                    copious_debugging!("        is invalid: {:?}", err);
                    continue;
                }
                copious_debugging!("        is {}", new_partial_path.display(graph, partials));
                new_cycle_detector.append(&mut self.appended_paths, extension.clone());
                if let Some(similar_path_detector) = &mut self.similar_path_detector {
                    if similar_path_detector.has_similar_path(
                        graph,
                        partials,
                        &new_partial_path,
                        |ps, left, right| left.equals(ps, right),
                    ) {
                        copious_debugging!("        is rejected: too many similar");
                        continue;
                    }
                }
            }
            self.next_iteration.0.push_back(new_partial_path);
            self.next_iteration.1.push_back(new_cycle_detector);
        }

        extension_count
    }

    /// Returns whether the algorithm has completed.
    pub fn is_complete(&self) -> bool {
        self.queue.is_empty() && self.next_iteration.0.is_empty()
    }

    /// Runs the next phase of the algorithm.  We will have built up a set of incomplete partial
    /// paths during the _previous_ phase.  Before calling this function, you must ensure that `db`
    /// contains all of the possible appendables that we might want to extend any of those
    /// candidate partial paths with.
    ///
    /// After this method returns, you can use [`previous_phase_partial_paths`][] to retrieve a
    /// list of the (possibly incomplete) partial paths that were encountered during this phase.
    ///
    /// The `extend_while` closure is used to control whether the extended paths are further extended
    /// or not. It is not called on the initial paths.
    ///
    /// [`previous_phase_partial_paths`]: #method.previous_phase_partial_paths
    pub fn process_next_phase<A, Db, E>(
        &mut self,
        graph: &StackGraph,
        partials: &mut PartialPaths,
        db: &mut Db,
        extend_while: E,
    ) where
        A: Appendable,
        Db: Candidates<H> + ToAppendable<H, A>,
        E: Fn(&StackGraph, &mut PartialPaths, &PartialPath) -> bool,
    {
        copious_debugging!("==> Start phase {}", self.phase_number);
        self.queue.extend(
            self.next_iteration
                .0
                .drain(..)
                .zip(self.next_iteration.1.drain(..)),
        );
        let mut work_performed = 0;
        while let Some((partial_path, cycle_detector)) = self.queue.pop_front() {
            copious_debugging!(
                "--> Candidate partial path {}",
                partial_path.display(graph, partials)
            );
            if self.initial_paths > 0 {
                self.initial_paths -= 1;
            } else if !extend_while(graph, partials, &partial_path) {
                copious_debugging!(
                    "    Do not extend {}",
                    partial_path.display(graph, partials)
                );
                continue;
            }
            work_performed += self.extend(graph, partials, db, &partial_path, cycle_detector);
            if work_performed >= self.max_work_per_phase {
                break;
            }
        }

        #[cfg(feature = "copious-debugging")]
        {
            if let Some(similar_path_detector) = &self.similar_path_detector {
                copious_debugging!(
                    "    Max similar path bucket size: {}",
                    similar_path_detector.max_bucket_size()
                );
            }
            copious_debugging!("==> End phase {}", self.phase_number);
            self.phase_number += 1;
        }
    }
}

impl ForwardPartialPathStitcher<Edge> {
    /// Finds a minimal set of partial paths in a file, calling the `visit` closure for each one.
    ///
    /// This function ensures that the set of visited partial paths
    ///  (a) is minimal, no path can be constructed by stitching other paths in the set, and
    ///  (b) covers all complete paths, from references to definitions, when used for path stitching
    ///
    /// This function will not return until all reachable partial paths have been processed, so
    /// your database must already contain all partial paths that might be needed.  If you have a
    /// very large stack graph stored in some other storage system, and want more control over
    /// lazily loading only the necessary pieces, then you should code up your own loop that calls
    /// [`process_next_phase`][] manually.
    ///
    /// Caveat: Edges between nodes of different files are not used. Hence the returned set of partial
    /// paths will not cover paths going through those edges.
    ///
    /// [`process_next_phase`]: #method.process_next_phase
    pub fn find_minimal_partial_path_set_in_file<F>(
        graph: &StackGraph,
        partials: &mut PartialPaths,
        file: Handle<File>,
        cancellation_flag: &dyn CancellationFlag,
        mut visit: F,
    ) -> Result<(), CancellationError>
    where
        F: FnMut(&StackGraph, &mut PartialPaths, &PartialPath),
    {
        fn as_complete_as_necessary(graph: &StackGraph, path: &PartialPath) -> bool {
            path.starts_at_endpoint(graph)
                && (path.ends_at_endpoint(graph) || graph[path.end_node].is_jump_to())
        }

        let initial_paths = graph
            .nodes_for_file(file)
            .chain(std::iter::once(StackGraph::root_node()))
            .filter(|node| graph[*node].is_endpoint())
            .map(|node| PartialPath::from_node(graph, partials, node))
            .collect::<Vec<_>>();
        let mut stitcher =
            ForwardPartialPathStitcher::from_partial_paths(graph, partials, initial_paths);
        while !stitcher.is_complete() {
            cancellation_flag.check("finding complete partial paths")?;
            stitcher.process_next_phase(
                graph,
                partials,
                &mut GraphEdges(Some(file)),
                |g, _ps, p| !as_complete_as_necessary(g, p),
            );
            for path in stitcher.previous_phase_partial_paths() {
                if as_complete_as_necessary(graph, path) {
                    visit(graph, partials, path);
                }
            }
        }
        Ok(())
    }
}

impl<H: Clone> ForwardPartialPathStitcher<H> {
    /// Finds all complete partial paths that are reachable from a set of starting nodes,
    /// building them up by stitching together partial paths from this database, and calling
    /// the `visit` closure on each one.
    ///
    /// This function will not return until all reachable partial paths have been processed, so
    /// your database must already contain all partial paths that might be needed.  If you have a
    /// very large stack graph stored in some other storage system, and want more control over
    /// lazily loading only the necessary pieces, then you should code up your own loop that calls
    /// [`process_next_phase`][] manually.
    ///
    /// [`process_next_phase`]: #method.process_next_phase
    pub fn find_all_complete_partial_paths<I, F, A, Db>(
        graph: &StackGraph,
        partials: &mut PartialPaths,
        db: &mut Db,
        starting_nodes: I,
        cancellation_flag: &dyn CancellationFlag,
        mut visit: F,
    ) -> Result<(), CancellationError>
    where
        I: IntoIterator<Item = Handle<Node>>,
        A: Appendable,
        Db: Candidates<H> + ToAppendable<H, A>,
        F: FnMut(&StackGraph, &mut PartialPaths, &PartialPath),
    {
        let initial_paths = starting_nodes
            .into_iter()
            .filter(|n| graph[*n].is_reference())
            .map(|n| {
                let mut p = PartialPath::from_node(graph, partials, n);
                p.eliminate_precondition_stack_variables(partials);
                p
            })
            .collect::<Vec<_>>();
        let mut stitcher =
            ForwardPartialPathStitcher::from_partial_paths(graph, partials, initial_paths);
        while !stitcher.is_complete() {
            cancellation_flag.check("finding complete partial paths")?;
            stitcher.process_next_phase(graph, partials, db, |_, _, _| true);
            for path in stitcher.previous_phase_partial_paths() {
                if path.is_complete(graph) {
                    visit(graph, partials, path);
                }
            }
        }
        Ok(())
    }
}
