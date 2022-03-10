// -*- coding: utf-8 -*-
// ------------------------------------------------------------------------------------------------
// Copyright © 2022, stack-graphs authors.
// Licensed under either of Apache License, Version 2.0, or MIT license, at your option.
// Please see the LICENSE-APACHE or LICENSE-MIT files in this distribution for license details.
// ------------------------------------------------------------------------------------------------

//! Defines assertions that can be run against a stack graph.

use itertools::Itertools;
use lsp_positions::Position;

use crate::arena::Handle;
use crate::graph::File;
use crate::graph::Node;
use crate::graph::StackGraph;
use crate::graph::Symbol;
use crate::paths::Path;
use crate::paths::Paths;

/// A stack graph assertion
#[derive(Debug, Clone)]
pub enum Assertion {
    Defined {
        source: AssertionSource,
        targets: Vec<AssertionTarget>,
    },
}

/// Source position of an assertion
#[derive(Debug, Clone)]
pub struct AssertionSource {
    pub file: Handle<File>,
    pub position: Position,
}

/// Target line of an assertion
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AssertionTarget {
    pub file: Handle<File>,
    pub line: usize,
}

impl AssertionTarget {
    /// Checks if the target matches the node corresponding to the handle in the given graph.
    pub fn matches_node(&self, node: Handle<Node>, graph: &StackGraph) -> bool {
        let file = graph[node].file().unwrap();
        let si = graph.source_info(node).unwrap();
        let start_line = si.span.start.line;
        let end_line = si.span.end.line;
        file == self.file && start_line <= self.line && self.line <= end_line
    }
}

/// Error describing assertion failures.
#[derive(Clone)]
pub enum AssertionError {
    NoReferences {
        source: AssertionSource,
    },
    IncorrectDefinitions {
        source: AssertionSource,
        symbols: Vec<Handle<Symbol>>,
        missing_targets: Vec<AssertionTarget>,
        unexpected_paths: Vec<Path>,
    },
}

impl Assertion {
    /// Run this assertion against the given graph, using the given paths object for path search.
    pub fn run(&self, graph: &StackGraph, paths: &mut Paths) -> Result<(), AssertionError> {
        match self {
            Assertion::Defined {
                source,
                targets: expected_targets,
            } => {
                let references = graph
                    .nodes_for_file(source.file)
                    .filter(|n| {
                        graph[*n].is_reference()
                            && graph
                                .source_info(*n)
                                .map(|s| s.span.contains(&source.position))
                                .unwrap_or(false)
                    })
                    .collect::<Vec<_>>();
                if references.is_empty() {
                    Err(AssertionError::NoReferences {
                        source: source.clone(),
                    })
                } else {
                    let mut actual_paths = Vec::new();
                    paths.find_all_paths(graph, references.clone(), |g, _ps, p| {
                        if p.is_complete(g) {
                            actual_paths.push(p);
                        }
                    });
                    let missing_targets = expected_targets
                        .iter()
                        .filter(|t| {
                            !actual_paths
                                .iter()
                                .any(|p| t.matches_node(p.end_node, graph))
                        })
                        .cloned()
                        .unique()
                        .collect::<Vec<_>>();
                    let unexpected_paths = actual_paths
                        .iter()
                        .filter(|p| {
                            !expected_targets
                                .iter()
                                .any(|t| t.matches_node(p.end_node, graph))
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if missing_targets.is_empty() && unexpected_paths.is_empty() {
                        Ok(())
                    } else {
                        let symbols = references
                            .iter()
                            .map(|r| graph[*r].symbol().unwrap())
                            .unique()
                            .collect::<Vec<_>>();
                        Err(AssertionError::IncorrectDefinitions {
                            source: source.clone(),
                            symbols,
                            missing_targets,
                            unexpected_paths,
                        })
                    }
                }
            }
        }
    }
}
