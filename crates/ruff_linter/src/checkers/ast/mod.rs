//! [`Checker`] for AST-based lint rules.
//!
//! The [`Checker`] is responsible for traversing over the AST, building up the [`SemanticModel`],
//! and running any enabled [`Rule`]s at the appropriate place and time.
//!
//! The [`Checker`] is structured as a single pass over the AST that proceeds in "evaluation" order.
//! That is: the [`Checker`] typically iterates over nodes in the order in which they're evaluated
//! by the Python interpreter. This includes, e.g., deferring function body traversal until after
//! parent scopes have been fully traversed. Individual rules may also perform internal traversals
//! of the AST.
//!
//! While the [`Checker`] is typically passed by mutable reference to the individual lint rule
//! implementations, most of its constituent components are intended to be treated immutably, with
//! the exception of the [`Diagnostic`] vector, which is intended to be mutated by the individual
//! lint rules. In the future, this should be formalized in the API.
//!
//! The individual [`Visitor`] implementations within the [`Checker`] typically proceed in four
//! steps:
//!
//! 1. Binding: Bind any names introduced by the current node.
//! 2. Traversal: Recurse into the children of the current node.
//! 3. Clean-up: Perform any necessary clean-up after the current node has been fully traversed.
//! 4. Analysis: Run any relevant lint rules on the current node.
//!
//! The first three steps together compose the semantic analysis phase, while the last step
//! represents the lint-rule analysis phase. In the future, these steps may be separated into
//! distinct passes over the AST.

use std::path::Path;

use itertools::Itertools;
use log::debug;
use ruff_python_ast::{
    self as ast, Comprehension, ElifElseClause, ExceptHandler, Expr, ExprContext, Keyword,
    MatchCase, Parameter, ParameterWithDefault, Parameters, Pattern, Stmt, Suite, UnaryOp,
};
use ruff_text_size::{Ranged, TextRange, TextSize};

use ruff_diagnostics::{Diagnostic, IsolationLevel};
use ruff_notebook::{CellOffsets, NotebookIndex};
use ruff_python_ast::all::{extract_all_names, DunderAllFlags};
use ruff_python_ast::helpers::{
    collect_import_from_member, extract_handled_exceptions, is_docstring_stmt, to_module_path,
};
use ruff_python_ast::identifier::Identifier;
use ruff_python_ast::name::QualifiedName;
use ruff_python_ast::str::trailing_quote;
use ruff_python_ast::visitor::{walk_except_handler, walk_f_string_element, walk_pattern, Visitor};
use ruff_python_ast::{helpers, str, visitor, PySourceType};
use ruff_python_codegen::{Generator, Quote, Stylist};
use ruff_python_index::Indexer;
use ruff_python_parser::typing::{parse_type_annotation, AnnotationKind};
use ruff_python_semantic::analyze::{imports, typing, visibility};
use ruff_python_semantic::{
    BindingFlags, BindingId, BindingKind, Exceptions, Export, FromImport, Globals, Import, Module,
    ModuleKind, NodeId, ScopeId, ScopeKind, SemanticModel, SemanticModelFlags, StarImport,
    SubmoduleImport,
};
use ruff_python_stdlib::builtins::{IPYTHON_BUILTINS, MAGIC_GLOBALS, PYTHON_BUILTINS};
use ruff_source_file::{Locator, OneIndexed, SourceRow};

use crate::checkers::ast::annotation::AnnotationContext;
use crate::docstrings::extraction::ExtractionTarget;
use crate::importer::Importer;
use crate::noqa::NoqaMapping;
use crate::registry::Rule;
use crate::rules::{flake8_pyi, flake8_type_checking, pyflakes, pyupgrade};
use crate::settings::{flags, LinterSettings};
use crate::{docstrings, noqa};

mod analyze;
mod annotation;
mod deferred;

/// State representing whether a docstring is expected or not for the next statement.
#[derive(Default, Debug, Copy, Clone, PartialEq)]
enum DocstringState {
    /// The next statement is expected to be a docstring, but not necessarily so.
    ///
    /// For example, in the following code:
    ///
    /// ```python
    /// class Foo:
    ///     pass
    ///
    ///
    /// def bar(x, y):
    ///     """Docstring."""
    ///     return x +  y
    /// ```
    ///
    /// For `Foo`, the state is expected when the checker is visiting the class
    /// body but isn't going to be present. While, for `bar` function, the docstring
    /// is expected and present.
    #[default]
    Expected,
    Other,
}

impl DocstringState {
    /// Returns `true` if the next statement is expected to be a docstring.
    const fn is_expected(self) -> bool {
        matches!(self, DocstringState::Expected)
    }
}

pub(crate) struct Checker<'a> {
    /// The [`Path`] to the file under analysis.
    path: &'a Path,
    /// The [`Path`] to the package containing the current file.
    package: Option<&'a Path>,
    /// The module representation of the current file (e.g., `foo.bar`).
    module_path: Option<&'a [String]>,
    /// The [`PySourceType`] of the current file.
    pub(crate) source_type: PySourceType,
    /// The [`CellOffsets`] for the current file, if it's a Jupyter notebook.
    cell_offsets: Option<&'a CellOffsets>,
    /// The [`NotebookIndex`] for the current file, if it's a Jupyter notebook.
    notebook_index: Option<&'a NotebookIndex>,
    /// The [`flags::Noqa`] for the current analysis (i.e., whether to respect suppression
    /// comments).
    noqa: flags::Noqa,
    /// The [`NoqaMapping`] for the current analysis (i.e., the mapping from line number to
    /// suppression commented line number).
    noqa_line_for: &'a NoqaMapping,
    /// The [`LinterSettings`] for the current analysis, including the enabled rules.
    pub(crate) settings: &'a LinterSettings,
    /// The [`Locator`] for the current file, which enables extraction of source code from byte
    /// offsets.
    locator: &'a Locator<'a>,
    /// The [`Stylist`] for the current file, which detects the current line ending, quote, and
    /// indentation style.
    stylist: &'a Stylist<'a>,
    /// The [`Indexer`] for the current file, which contains the offsets of all comments and more.
    indexer: &'a Indexer,
    /// The [`Importer`] for the current file, which enables importing of other modules.
    importer: Importer<'a>,
    /// The [`SemanticModel`], built up over the course of the AST traversal.
    semantic: SemanticModel<'a>,
    /// A set of deferred nodes to be visited after the current traversal (e.g., function bodies).
    visit: deferred::Visit<'a>,
    /// A set of deferred nodes to be analyzed after the AST traversal (e.g., `for` loops).
    analyze: deferred::Analyze,
    /// The cumulative set of diagnostics computed across all lint rules.
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// The list of names already seen by flake8-bugbear diagnostics, to avoid duplicate violations..
    pub(crate) flake8_bugbear_seen: Vec<TextRange>,
    /// The end offset of the last visited statement.
    last_stmt_end: TextSize,
    /// A state describing if a docstring is expected or not.
    docstring_state: DocstringState,
}

impl<'a> Checker<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        settings: &'a LinterSettings,
        noqa_line_for: &'a NoqaMapping,
        noqa: flags::Noqa,
        path: &'a Path,
        package: Option<&'a Path>,
        module: Module<'a>,
        locator: &'a Locator,
        stylist: &'a Stylist,
        indexer: &'a Indexer,
        importer: Importer<'a>,
        source_type: PySourceType,
        cell_offsets: Option<&'a CellOffsets>,
        notebook_index: Option<&'a NotebookIndex>,
    ) -> Checker<'a> {
        Checker {
            settings,
            noqa_line_for,
            noqa,
            path,
            package,
            module_path: module.path(),
            source_type,
            locator,
            stylist,
            indexer,
            importer,
            semantic: SemanticModel::new(&settings.typing_modules, path, module),
            visit: deferred::Visit::default(),
            analyze: deferred::Analyze::default(),
            diagnostics: Vec::default(),
            flake8_bugbear_seen: Vec::default(),
            cell_offsets,
            notebook_index,
            last_stmt_end: TextSize::default(),
            docstring_state: DocstringState::default(),
        }
    }
}

impl<'a> Checker<'a> {
    /// Return `true` if a [`Rule`] is disabled by a `noqa` directive.
    pub(crate) fn rule_is_ignored(&self, code: Rule, offset: TextSize) -> bool {
        // TODO(charlie): `noqa` directives are mostly enforced in `check_lines.rs`.
        // However, in rare cases, we need to check them here. For example, when
        // removing unused imports, we create a single fix that's applied to all
        // unused members on a single import. We need to pre-emptively omit any
        // members from the fix that will eventually be excluded by a `noqa`.
        // Unfortunately, we _do_ want to register a `Diagnostic` for each
        // eventually-ignored import, so that our `noqa` counts are accurate.
        if !self.noqa.to_bool() {
            return false;
        }
        noqa::rule_is_ignored(code, offset, self.noqa_line_for, self.locator)
    }

    /// Create a [`Generator`] to generate source code based on the current AST state.
    pub(crate) fn generator(&self) -> Generator {
        Generator::new(
            self.stylist.indentation(),
            self.f_string_quote_style().unwrap_or(self.stylist.quote()),
            self.stylist.line_ending(),
        )
    }

    /// Returns the appropriate quoting for f-string by reversing the one used outside of
    /// the f-string.
    ///
    /// If the current expression in the context is not an f-string, returns ``None``.
    pub(crate) fn f_string_quote_style(&self) -> Option<Quote> {
        if !self.semantic.in_f_string() {
            return None;
        }

        // Find the quote character used to start the containing f-string.
        let expr = self.semantic.current_expression()?;
        let string_range = self.indexer.fstring_ranges().innermost(expr.start())?;
        let trailing_quote = trailing_quote(self.locator.slice(string_range))?;

        // Invert the quote character, if it's a single quote.
        match trailing_quote {
            "'" => Some(Quote::Double),
            "\"" => Some(Quote::Single),
            _ => None,
        }
    }

    /// Returns the [`SourceRow`] for the given offset.
    pub(crate) fn compute_source_row(&self, offset: TextSize) -> SourceRow {
        #[allow(deprecated)]
        let line = self.locator.compute_line_index(offset);

        if let Some(notebook_index) = self.notebook_index {
            let cell = notebook_index.cell(line).unwrap_or(OneIndexed::MIN);
            let line = notebook_index.cell_row(line).unwrap_or(OneIndexed::MIN);
            SourceRow::Notebook { cell, line }
        } else {
            SourceRow::SourceFile { line }
        }
    }

    /// The [`Locator`] for the current file, which enables extraction of source code from byte
    /// offsets.
    pub(crate) const fn locator(&self) -> &'a Locator<'a> {
        self.locator
    }

    /// The [`Stylist`] for the current file, which detects the current line ending, quote, and
    /// indentation style.
    pub(crate) const fn stylist(&self) -> &'a Stylist<'a> {
        self.stylist
    }

    /// The [`Indexer`] for the current file, which contains the offsets of all comments and more.
    pub(crate) const fn indexer(&self) -> &'a Indexer {
        self.indexer
    }

    /// The [`Importer`] for the current file, which enables importing of other modules.
    pub(crate) const fn importer(&self) -> &Importer<'a> {
        &self.importer
    }

    /// The [`SemanticModel`], built up over the course of the AST traversal.
    pub(crate) const fn semantic(&self) -> &SemanticModel<'a> {
        &self.semantic
    }

    /// The [`Path`] to the file under analysis.
    pub(crate) const fn path(&self) -> &'a Path {
        self.path
    }

    /// The [`Path`] to the package containing the current file.
    pub(crate) const fn package(&self) -> Option<&'a Path> {
        self.package
    }

    /// The [`CellOffsets`] for the current file, if it's a Jupyter notebook.
    pub(crate) const fn cell_offsets(&self) -> Option<&'a CellOffsets> {
        self.cell_offsets
    }

    /// Returns whether the given rule should be checked.
    #[inline]
    pub(crate) const fn enabled(&self, rule: Rule) -> bool {
        self.settings.rules.enabled(rule)
    }

    /// Returns whether any of the given rules should be checked.
    #[inline]
    pub(crate) const fn any_enabled(&self, rules: &[Rule]) -> bool {
        self.settings.rules.any_enabled(rules)
    }

    /// Returns the [`IsolationLevel`] to isolate fixes for a given node.
    ///
    /// The primary use-case for fix isolation is to ensure that we don't delete all statements
    /// in a given indented block, which would cause a syntax error. We therefore need to ensure
    /// that we delete at most one statement per indented block per fixer pass. Fix isolation should
    /// thus be applied whenever we delete a statement, but can otherwise be omitted.
    pub(crate) fn isolation(node_id: Option<NodeId>) -> IsolationLevel {
        node_id
            .map(|node_id| IsolationLevel::Group(node_id.into()))
            .unwrap_or_default()
    }
}

impl<'a> Visitor<'a> for Checker<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        // Step 0: Pre-processing
        self.semantic.push_node(stmt);

        // For Jupyter Notebooks, we'll reset the `IMPORT_BOUNDARY` flag when
        // we encounter a cell boundary.
        if self.source_type.is_ipynb()
            && self.semantic.at_top_level()
            && self.semantic.seen_import_boundary()
            && self.cell_offsets.is_some_and(|cell_offsets| {
                cell_offsets.has_cell_boundary(TextRange::new(self.last_stmt_end, stmt.start()))
            })
        {
            self.semantic.flags -= SemanticModelFlags::IMPORT_BOUNDARY;
        }

        // Track whether we've seen module docstrings, non-imports, etc.
        match stmt {
            Stmt::Expr(ast::StmtExpr { value, .. })
                if !self.semantic.seen_module_docstring_boundary()
                    && value.is_string_literal_expr() =>
            {
                self.semantic.flags |= SemanticModelFlags::MODULE_DOCSTRING_BOUNDARY;
            }
            Stmt::ImportFrom(ast::StmtImportFrom { module, names, .. }) => {
                self.semantic.flags |= SemanticModelFlags::MODULE_DOCSTRING_BOUNDARY;

                // Allow __future__ imports until we see a non-__future__ import.
                if let Some("__future__") = module.as_deref() {
                    if names
                        .iter()
                        .any(|alias| alias.name.as_str() == "annotations")
                    {
                        self.semantic.flags |= SemanticModelFlags::FUTURE_ANNOTATIONS;
                    }
                } else {
                    self.semantic.flags |= SemanticModelFlags::FUTURES_BOUNDARY;
                }
            }
            Stmt::Import(_) => {
                self.semantic.flags |= SemanticModelFlags::MODULE_DOCSTRING_BOUNDARY;
                self.semantic.flags |= SemanticModelFlags::FUTURES_BOUNDARY;
            }
            _ => {
                self.semantic.flags |= SemanticModelFlags::MODULE_DOCSTRING_BOUNDARY;
                self.semantic.flags |= SemanticModelFlags::FUTURES_BOUNDARY;
                if !(self.semantic.seen_import_boundary()
                    || helpers::is_assignment_to_a_dunder(stmt)
                    || helpers::in_nested_block(self.semantic.current_statements())
                    || imports::is_matplotlib_activation(stmt, self.semantic())
                    || imports::is_sys_path_modification(stmt, self.semantic())
                    || (self.settings.preview.is_enabled()
                        && imports::is_os_environ_modification(stmt, self.semantic())))
                {
                    self.semantic.flags |= SemanticModelFlags::IMPORT_BOUNDARY;
                }
            }
        }

        // Store the flags prior to any further descent, so that we can restore them after visiting
        // the node.
        let flags_snapshot = self.semantic.flags;

        // Update the semantic model if it is in a docstring. This should be done after the
        // flags snapshot to ensure that it gets reset once the statement is analyzed.
        if self.docstring_state.is_expected() {
            if is_docstring_stmt(stmt) {
                self.semantic.flags |= SemanticModelFlags::DOCSTRING;
            }
            // Reset the state irrespective of whether the statement is a docstring or not.
            self.docstring_state = DocstringState::Other;
        }

        // Step 1: Binding
        match stmt {
            Stmt::AugAssign(ast::StmtAugAssign {
                target,
                op: _,
                value: _,
                range: _,
            }) => {
                self.handle_node_load(target);
            }
            Stmt::Import(ast::StmtImport { names, range: _ }) => {
                if self.semantic.at_top_level() {
                    self.importer.visit_import(stmt);
                }

                for alias in names {
                    // Given `import foo.bar`, `module` would be "foo", and `call_path` would be
                    // `["foo", "bar"]`.
                    let module = alias.name.split('.').next().unwrap();

                    // Mark the top-level module as "seen" by the semantic model.
                    self.semantic.add_module(module);

                    if alias.asname.is_none() && alias.name.contains('.') {
                        let qualified_name = QualifiedName::user_defined(&alias.name);
                        self.add_binding(
                            module,
                            alias.identifier(),
                            BindingKind::SubmoduleImport(SubmoduleImport {
                                qualified_name: Box::new(qualified_name),
                            }),
                            BindingFlags::EXTERNAL,
                        );
                    } else {
                        let mut flags = BindingFlags::EXTERNAL;
                        if alias.asname.is_some() {
                            flags |= BindingFlags::ALIAS;
                        }
                        if alias
                            .asname
                            .as_ref()
                            .is_some_and(|asname| asname.as_str() == alias.name.as_str())
                        {
                            flags |= BindingFlags::EXPLICIT_EXPORT;
                        }

                        let name = alias.asname.as_ref().unwrap_or(&alias.name);
                        let qualified_name = QualifiedName::user_defined(&alias.name);
                        self.add_binding(
                            name,
                            alias.identifier(),
                            BindingKind::Import(Import {
                                qualified_name: Box::new(qualified_name),
                            }),
                            flags,
                        );
                    }
                }
            }
            Stmt::ImportFrom(ast::StmtImportFrom {
                names,
                module,
                level,
                range: _,
            }) => {
                if self.semantic.at_top_level() {
                    self.importer.visit_import(stmt);
                }

                let module = module.as_deref();
                let level = *level;

                // Mark the top-level module as "seen" by the semantic model.
                if level.map_or(true, |level| level == 0) {
                    if let Some(module) = module.and_then(|module| module.split('.').next()) {
                        self.semantic.add_module(module);
                    }
                }

                for alias in names {
                    if let Some("__future__") = module {
                        let name = alias.asname.as_ref().unwrap_or(&alias.name);
                        self.add_binding(
                            name,
                            alias.identifier(),
                            BindingKind::FutureImport,
                            BindingFlags::empty(),
                        );
                    } else if &alias.name == "*" {
                        self.semantic
                            .current_scope_mut()
                            .add_star_import(StarImport { level, module });
                    } else {
                        let mut flags = BindingFlags::EXTERNAL;
                        if alias.asname.is_some() {
                            flags |= BindingFlags::ALIAS;
                        }
                        if alias
                            .asname
                            .as_ref()
                            .is_some_and(|asname| asname.as_str() == alias.name.as_str())
                        {
                            flags |= BindingFlags::EXPLICIT_EXPORT;
                        }

                        // Given `from foo import bar`, `name` would be "bar" and `qualified_name` would
                        // be "foo.bar". Given `from foo import bar as baz`, `name` would be "baz"
                        // and `qualified_name` would be "foo.bar".
                        let name = alias.asname.as_ref().unwrap_or(&alias.name);

                        // Attempt to resolve any relative imports; but if we don't know the current
                        // module path, or the relative import extends beyond the package root,
                        // fallback to a literal representation (e.g., `[".", "foo"]`).
                        let qualified_name = collect_import_from_member(level, module, &alias.name);
                        self.add_binding(
                            name,
                            alias.identifier(),
                            BindingKind::FromImport(FromImport {
                                qualified_name: Box::new(qualified_name),
                            }),
                            flags,
                        );
                    }
                }
            }
            Stmt::Global(ast::StmtGlobal { names, range: _ }) => {
                if !self.semantic.scope_id.is_global() {
                    for name in names {
                        if let Some(binding_id) = self.semantic.global_scope().get(name) {
                            // Mark the binding in the global scope as "rebound" in the current scope.
                            self.semantic
                                .add_rebinding_scope(binding_id, self.semantic.scope_id);
                        }

                        // Add a binding to the current scope.
                        let binding_id = self.semantic.push_binding(
                            name.range(),
                            BindingKind::Global,
                            BindingFlags::GLOBAL,
                        );
                        let scope = self.semantic.current_scope_mut();
                        scope.add(name, binding_id);
                    }
                }
            }
            Stmt::Nonlocal(ast::StmtNonlocal { names, range: _ }) => {
                if !self.semantic.scope_id.is_global() {
                    for name in names {
                        if let Some((scope_id, binding_id)) = self.semantic.nonlocal(name) {
                            // Mark the binding as "used".
                            self.semantic.add_local_reference(binding_id, name.range());

                            // Mark the binding in the enclosing scope as "rebound" in the current
                            // scope.
                            self.semantic
                                .add_rebinding_scope(binding_id, self.semantic.scope_id);

                            // Add a binding to the current scope.
                            let binding_id = self.semantic.push_binding(
                                name.range(),
                                BindingKind::Nonlocal(scope_id),
                                BindingFlags::NONLOCAL,
                            );
                            let scope = self.semantic.current_scope_mut();
                            scope.add(name, binding_id);
                        }
                    }
                }
            }
            _ => {}
        }

        // Step 2: Traversal
        match stmt {
            Stmt::FunctionDef(
                function_def @ ast::StmtFunctionDef {
                    body,
                    parameters,
                    decorator_list,
                    returns,
                    type_params,
                    ..
                },
            ) => {
                // Visit the decorators and arguments, but avoid the body, which will be
                // deferred.
                for decorator in decorator_list {
                    self.visit_decorator(decorator);
                }

                // Function annotations are always evaluated at runtime, unless future annotations
                // are enabled.
                let annotation =
                    AnnotationContext::from_function(function_def, &self.semantic, self.settings);

                // The first parameter may be a single dispatch.
                let mut singledispatch =
                    flake8_type_checking::helpers::is_singledispatch_implementation(
                        function_def,
                        self.semantic(),
                    );

                self.semantic.push_scope(ScopeKind::Type);

                if let Some(type_params) = type_params {
                    self.visit_type_params(type_params);
                }

                for parameter_with_default in parameters
                    .posonlyargs
                    .iter()
                    .chain(&parameters.args)
                    .chain(&parameters.kwonlyargs)
                {
                    if let Some(expr) = &parameter_with_default.parameter.annotation {
                        if singledispatch {
                            self.visit_runtime_required_annotation(expr);
                        } else {
                            match annotation {
                                AnnotationContext::RuntimeRequired => {
                                    self.visit_runtime_required_annotation(expr);
                                }
                                AnnotationContext::RuntimeEvaluated => {
                                    self.visit_runtime_evaluated_annotation(expr);
                                }
                                AnnotationContext::TypingOnly => {
                                    self.visit_annotation(expr);
                                }
                            }
                        };
                    }
                    if let Some(expr) = &parameter_with_default.default {
                        self.visit_expr(expr);
                    }
                    singledispatch = false;
                }
                if let Some(arg) = &parameters.vararg {
                    if let Some(expr) = &arg.annotation {
                        match annotation {
                            AnnotationContext::RuntimeRequired => {
                                self.visit_runtime_required_annotation(expr);
                            }
                            AnnotationContext::RuntimeEvaluated => {
                                self.visit_runtime_evaluated_annotation(expr);
                            }
                            AnnotationContext::TypingOnly => {
                                self.visit_annotation(expr);
                            }
                        }
                    }
                }
                if let Some(arg) = &parameters.kwarg {
                    if let Some(expr) = &arg.annotation {
                        match annotation {
                            AnnotationContext::RuntimeRequired => {
                                self.visit_runtime_required_annotation(expr);
                            }
                            AnnotationContext::RuntimeEvaluated => {
                                self.visit_runtime_evaluated_annotation(expr);
                            }
                            AnnotationContext::TypingOnly => {
                                self.visit_annotation(expr);
                            }
                        }
                    }
                }
                for expr in returns {
                    match annotation {
                        AnnotationContext::RuntimeRequired => {
                            self.visit_runtime_required_annotation(expr);
                        }
                        AnnotationContext::RuntimeEvaluated => {
                            self.visit_runtime_evaluated_annotation(expr);
                        }
                        AnnotationContext::TypingOnly => {
                            self.visit_annotation(expr);
                        }
                    }
                }

                let definition = docstrings::extraction::extract_definition(
                    ExtractionTarget::Function(function_def),
                    self.semantic.definition_id,
                    &self.semantic.definitions,
                );
                self.semantic.push_definition(definition);
                self.semantic.push_scope(ScopeKind::Function(function_def));
                self.semantic.flags -= SemanticModelFlags::EXCEPTION_HANDLER;

                self.visit.functions.push(self.semantic.snapshot());

                // Extract any global bindings from the function body.
                if let Some(globals) = Globals::from_body(body) {
                    self.semantic.set_globals(globals);
                }
            }
            Stmt::ClassDef(
                class_def @ ast::StmtClassDef {
                    body,
                    arguments,
                    decorator_list,
                    type_params,
                    ..
                },
            ) => {
                for decorator in decorator_list {
                    self.visit_decorator(decorator);
                }

                self.semantic.push_scope(ScopeKind::Type);

                if let Some(type_params) = type_params {
                    self.visit_type_params(type_params);
                }

                if let Some(arguments) = arguments {
                    self.visit_arguments(arguments);
                }

                let definition = docstrings::extraction::extract_definition(
                    ExtractionTarget::Class(class_def),
                    self.semantic.definition_id,
                    &self.semantic.definitions,
                );
                self.semantic.push_definition(definition);
                self.semantic.push_scope(ScopeKind::Class(class_def));
                self.semantic.flags -= SemanticModelFlags::EXCEPTION_HANDLER;

                // Extract any global bindings from the class body.
                if let Some(globals) = Globals::from_body(body) {
                    self.semantic.set_globals(globals);
                }

                // Set the docstring state before visiting the class body.
                self.docstring_state = DocstringState::Expected;
                self.visit_body(body);
            }
            Stmt::TypeAlias(ast::StmtTypeAlias {
                range: _,
                name,
                type_params,
                value,
            }) => {
                self.semantic.push_scope(ScopeKind::Type);
                if let Some(type_params) = type_params {
                    self.visit_type_params(type_params);
                }
                self.visit
                    .type_param_definitions
                    .push((value, self.semantic.snapshot()));
                self.semantic.pop_scope();
                self.visit_expr(name);
            }
            Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                ..
            }) => {
                let mut handled_exceptions = Exceptions::empty();
                for type_ in extract_handled_exceptions(handlers) {
                    if let Some(qualified_name) = self.semantic.resolve_qualified_name(type_) {
                        match qualified_name.segments() {
                            ["", "NameError"] => {
                                handled_exceptions |= Exceptions::NAME_ERROR;
                            }
                            ["", "ModuleNotFoundError"] => {
                                handled_exceptions |= Exceptions::MODULE_NOT_FOUND_ERROR;
                            }
                            ["", "ImportError"] => {
                                handled_exceptions |= Exceptions::IMPORT_ERROR;
                            }
                            _ => {}
                        }
                    }
                }

                // Iterate over the `body`, then the `handlers`, then the `orelse`, then the
                // `finalbody`, but treat the body and the `orelse` as a single branch for
                // flow analysis purposes.
                let branch = self.semantic.push_branch();
                self.semantic.handled_exceptions.push(handled_exceptions);
                self.visit_body(body);
                self.semantic.handled_exceptions.pop();
                self.semantic.pop_branch();

                for except_handler in handlers {
                    self.semantic.push_branch();
                    self.visit_except_handler(except_handler);
                    self.semantic.pop_branch();
                }

                self.semantic.set_branch(branch);
                self.visit_body(orelse);
                self.semantic.pop_branch();

                self.semantic.push_branch();
                self.visit_body(finalbody);
                self.semantic.pop_branch();
            }
            Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                annotation,
                value,
                ..
            }) => {
                match AnnotationContext::from_model(&self.semantic, self.settings) {
                    AnnotationContext::RuntimeRequired => {
                        self.visit_runtime_required_annotation(annotation);
                    }
                    AnnotationContext::RuntimeEvaluated => {
                        self.visit_runtime_evaluated_annotation(annotation);
                    }
                    AnnotationContext::TypingOnly
                        if flake8_type_checking::helpers::is_dataclass_meta_annotation(
                            annotation,
                            self.semantic(),
                        ) =>
                    {
                        if let Expr::Subscript(subscript) = &**annotation {
                            // Ex) `InitVar[str]`
                            self.visit_runtime_required_annotation(&subscript.value);
                            self.visit_annotation(&subscript.slice);
                        } else {
                            // Ex) `InitVar`
                            self.visit_runtime_required_annotation(annotation);
                        }
                    }
                    AnnotationContext::TypingOnly => self.visit_annotation(annotation),
                }

                if let Some(expr) = value {
                    if self.semantic.match_typing_expr(annotation, "TypeAlias") {
                        self.visit_type_definition(expr);
                    } else {
                        self.visit_expr(expr);
                    }
                }
                self.visit_expr(target);
            }
            Stmt::Assert(ast::StmtAssert {
                test,
                msg,
                range: _,
            }) => {
                self.visit_boolean_test(test);
                if let Some(expr) = msg {
                    self.visit_expr(expr);
                }
            }
            Stmt::While(ast::StmtWhile {
                test,
                body,
                orelse,
                range: _,
            }) => {
                self.visit_boolean_test(test);
                self.visit_body(body);
                self.visit_body(orelse);
            }
            Stmt::If(
                stmt_if @ ast::StmtIf {
                    test,
                    body,
                    elif_else_clauses,
                    range: _,
                },
            ) => {
                self.visit_boolean_test(test);

                self.semantic.push_branch();
                if typing::is_type_checking_block(stmt_if, &self.semantic) {
                    if self.semantic.at_top_level() {
                        self.importer.visit_type_checking_block(stmt);
                    }
                    self.visit_type_checking_block(body);
                } else {
                    self.visit_body(body);
                }
                self.semantic.pop_branch();

                for clause in elif_else_clauses {
                    self.semantic.push_branch();
                    self.visit_elif_else_clause(clause);
                    self.semantic.pop_branch();
                }
            }
            _ => visitor::walk_stmt(self, stmt),
        };

        // Step 3: Clean-up
        match stmt {
            Stmt::FunctionDef(ast::StmtFunctionDef { name, .. }) => {
                let scope_id = self.semantic.scope_id;
                self.analyze.scopes.push(scope_id);
                self.semantic.pop_scope(); // Function scope
                self.semantic.pop_definition();
                self.semantic.pop_scope(); // Type parameter scope
                self.add_binding(
                    name,
                    stmt.identifier(),
                    BindingKind::FunctionDefinition(scope_id),
                    BindingFlags::empty(),
                );
            }
            Stmt::ClassDef(ast::StmtClassDef { name, .. }) => {
                let scope_id = self.semantic.scope_id;
                self.analyze.scopes.push(scope_id);
                self.semantic.pop_scope(); // Class scope
                self.semantic.pop_definition();
                self.semantic.pop_scope(); // Type parameter scope
                self.add_binding(
                    name,
                    stmt.identifier(),
                    BindingKind::ClassDefinition(scope_id),
                    BindingFlags::empty(),
                );
            }
            _ => {}
        }

        // Step 4: Analysis
        analyze::statement(stmt, self);

        self.semantic.flags = flags_snapshot;
        self.semantic.pop_node();
        self.last_stmt_end = stmt.end();
    }

    fn visit_annotation(&mut self, expr: &'a Expr) {
        let flags_snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::TYPING_ONLY_ANNOTATION;
        self.visit_type_definition(expr);
        self.semantic.flags = flags_snapshot;
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        // Step 0: Pre-processing
        if !self.semantic.in_typing_literal()
            && !self.semantic.in_deferred_type_definition()
            && self.semantic.in_type_definition()
            && self.semantic.future_annotations()
        {
            if let Expr::StringLiteral(ast::ExprStringLiteral { value, .. }) = expr {
                self.visit.string_type_definitions.push((
                    expr.range(),
                    value.to_str(),
                    self.semantic.snapshot(),
                ));
            } else {
                self.visit
                    .future_type_definitions
                    .push((expr, self.semantic.snapshot()));
            }
            return;
        }

        self.semantic.push_node(expr);

        // Store the flags prior to any further descent, so that we can restore them after visiting
        // the node.
        let flags_snapshot = self.semantic.flags;

        // If we're in a boolean test (e.g., the `test` of a `Stmt::If`), but now within a
        // subexpression (e.g., `a` in `f(a)`), then we're no longer in a boolean test.
        if !matches!(
            expr,
            Expr::BoolOp(_)
                | Expr::UnaryOp(ast::ExprUnaryOp {
                    op: UnaryOp::Not,
                    ..
                })
        ) {
            self.semantic.flags -= SemanticModelFlags::BOOLEAN_TEST;
        }

        // Step 1: Binding
        match expr {
            Expr::Call(ast::ExprCall {
                func,
                arguments: _,
                range: _,
            }) => {
                if let Expr::Name(ast::ExprName { id, ctx, range: _ }) = func.as_ref() {
                    if id == "locals" && ctx.is_load() {
                        let scope = self.semantic.current_scope_mut();
                        scope.set_uses_locals();
                    }
                }
            }
            Expr::Name(ast::ExprName { id, ctx, range: _ }) => match ctx {
                ExprContext::Load => self.handle_node_load(expr),
                ExprContext::Store => self.handle_node_store(id, expr),
                ExprContext::Del => self.handle_node_delete(expr),
            },
            _ => {}
        }

        // Step 2: Traversal
        match expr {
            Expr::ListComp(ast::ExprListComp {
                elt,
                generators,
                range: _,
            })
            | Expr::SetComp(ast::ExprSetComp {
                elt,
                generators,
                range: _,
            })
            | Expr::Generator(ast::ExprGenerator {
                elt,
                generators,
                range: _,
                parenthesized: _,
            }) => {
                self.visit_generators(generators);
                self.visit_expr(elt);
            }
            Expr::DictComp(ast::ExprDictComp {
                key,
                value,
                generators,
                range: _,
            }) => {
                self.visit_generators(generators);
                self.visit_expr(key);
                self.visit_expr(value);
            }
            Expr::Lambda(
                lambda @ ast::ExprLambda {
                    parameters,
                    body: _,
                    range: _,
                },
            ) => {
                // Visit the default arguments, but avoid the body, which will be deferred.
                if let Some(parameters) = parameters {
                    for ParameterWithDefault {
                        default,
                        parameter: _,
                        range: _,
                    } in parameters
                        .posonlyargs
                        .iter()
                        .chain(&parameters.args)
                        .chain(&parameters.kwonlyargs)
                    {
                        if let Some(expr) = &default {
                            self.visit_expr(expr);
                        }
                    }
                }

                self.semantic.push_scope(ScopeKind::Lambda(lambda));
                self.visit.lambdas.push(self.semantic.snapshot());
                self.analyze.lambdas.push(self.semantic.snapshot());
            }
            Expr::If(ast::ExprIf {
                test,
                body,
                orelse,
                range: _,
            }) => {
                self.visit_boolean_test(test);
                self.visit_expr(body);
                self.visit_expr(orelse);
            }
            Expr::Call(ast::ExprCall {
                func,
                arguments,
                range: _,
            }) => {
                self.visit_expr(func);

                let callable =
                    self.semantic
                        .resolve_qualified_name(func)
                        .and_then(|qualified_name| {
                            if self
                                .semantic
                                .match_typing_qualified_name(&qualified_name, "cast")
                            {
                                Some(typing::Callable::Cast)
                            } else if self
                                .semantic
                                .match_typing_qualified_name(&qualified_name, "NewType")
                            {
                                Some(typing::Callable::NewType)
                            } else if self
                                .semantic
                                .match_typing_qualified_name(&qualified_name, "TypeVar")
                            {
                                Some(typing::Callable::TypeVar)
                            } else if self
                                .semantic
                                .match_typing_qualified_name(&qualified_name, "NamedTuple")
                            {
                                Some(typing::Callable::NamedTuple)
                            } else if self
                                .semantic
                                .match_typing_qualified_name(&qualified_name, "TypedDict")
                            {
                                Some(typing::Callable::TypedDict)
                            } else if matches!(
                                qualified_name.segments(),
                                [
                                    "mypy_extensions",
                                    "Arg"
                                        | "DefaultArg"
                                        | "NamedArg"
                                        | "DefaultNamedArg"
                                        | "VarArg"
                                        | "KwArg"
                                ]
                            ) {
                                Some(typing::Callable::MypyExtension)
                            } else if matches!(qualified_name.segments(), ["", "bool"]) {
                                Some(typing::Callable::Bool)
                            } else {
                                None
                            }
                        });
                match callable {
                    Some(typing::Callable::Bool) => {
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            self.visit_boolean_test(arg);
                        }
                        for arg in args {
                            self.visit_expr(arg);
                        }
                    }
                    Some(typing::Callable::Cast) => {
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            self.visit_type_definition(arg);
                        }
                        for arg in args {
                            self.visit_expr(arg);
                        }
                    }
                    Some(typing::Callable::NewType) => {
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            self.visit_non_type_definition(arg);
                        }
                        for arg in args {
                            self.visit_type_definition(arg);
                        }
                    }
                    Some(typing::Callable::TypeVar) => {
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            self.visit_non_type_definition(arg);
                        }
                        for arg in args {
                            self.visit_type_definition(arg);
                        }
                        for keyword in arguments.keywords.iter() {
                            let Keyword {
                                arg,
                                value,
                                range: _,
                            } = keyword;
                            if let Some(id) = arg {
                                if id.as_str() == "bound" {
                                    self.visit_type_definition(value);
                                } else {
                                    self.visit_non_type_definition(value);
                                }
                            }
                        }
                    }
                    Some(typing::Callable::NamedTuple) => {
                        // Ex) NamedTuple("a", [("a", int)])
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            self.visit_non_type_definition(arg);
                        }

                        for arg in args {
                            match arg {
                                // Ex) NamedTuple("a", [("a", int)])
                                Expr::List(ast::ExprList { elts, .. })
                                | Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                                    for elt in elts {
                                        match elt {
                                            Expr::List(ast::ExprList { elts, .. })
                                            | Expr::Tuple(ast::ExprTuple { elts, .. })
                                                if elts.len() == 2 =>
                                            {
                                                self.visit_non_type_definition(&elts[0]);
                                                self.visit_type_definition(&elts[1]);
                                            }
                                            _ => {
                                                self.visit_non_type_definition(elt);
                                            }
                                        }
                                    }
                                }
                                _ => self.visit_non_type_definition(arg),
                            }
                        }

                        for keyword in arguments.keywords.iter() {
                            let Keyword { arg, value, .. } = keyword;
                            match (arg.as_ref(), value) {
                                // Ex) NamedTuple("a", **{"a": int})
                                (None, Expr::Dict(ast::ExprDict { keys, values, .. })) => {
                                    for (key, value) in keys.iter().zip(values) {
                                        if let Some(key) = key.as_ref() {
                                            self.visit_non_type_definition(key);
                                            self.visit_type_definition(value);
                                        } else {
                                            self.visit_non_type_definition(value);
                                        }
                                    }
                                }
                                // Ex) NamedTuple("a", **obj)
                                (None, _) => {
                                    self.visit_non_type_definition(value);
                                }
                                // Ex) NamedTuple("a", a=int)
                                _ => {
                                    self.visit_type_definition(value);
                                }
                            }
                        }
                    }
                    Some(typing::Callable::TypedDict) => {
                        // Ex) TypedDict("a", {"a": int})
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            self.visit_non_type_definition(arg);
                        }
                        for arg in args {
                            if let Expr::Dict(ast::ExprDict {
                                keys,
                                values,
                                range: _,
                            }) = arg
                            {
                                for key in keys.iter().flatten() {
                                    self.visit_non_type_definition(key);
                                }
                                for value in values {
                                    self.visit_type_definition(value);
                                }
                            } else {
                                self.visit_non_type_definition(arg);
                            }
                        }

                        // Ex) TypedDict("a", a=int)
                        for keyword in arguments.keywords.iter() {
                            let Keyword { value, .. } = keyword;
                            self.visit_type_definition(value);
                        }
                    }
                    Some(typing::Callable::MypyExtension) => {
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            // Ex) DefaultNamedArg(bool | None, name="some_prop_name")
                            self.visit_type_definition(arg);

                            for arg in args {
                                self.visit_non_type_definition(arg);
                            }
                            for keyword in arguments.keywords.iter() {
                                let Keyword { value, .. } = keyword;
                                self.visit_non_type_definition(value);
                            }
                        } else {
                            // Ex) DefaultNamedArg(type="bool", name="some_prop_name")
                            for keyword in arguments.keywords.iter() {
                                let Keyword {
                                    value,
                                    arg,
                                    range: _,
                                } = keyword;
                                if arg.as_ref().is_some_and(|arg| arg == "type") {
                                    self.visit_type_definition(value);
                                } else {
                                    self.visit_non_type_definition(value);
                                }
                            }
                        }
                    }
                    None => {
                        // If we're in a type definition, we need to treat the arguments to any
                        // other callables as non-type definitions (i.e., we don't want to treat
                        // any strings as deferred type definitions).
                        for arg in arguments.args.iter() {
                            self.visit_non_type_definition(arg);
                        }
                        for keyword in arguments.keywords.iter() {
                            let Keyword { value, .. } = keyword;
                            self.visit_non_type_definition(value);
                        }
                    }
                }
            }
            Expr::Subscript(ast::ExprSubscript {
                value,
                slice,
                ctx,
                range: _,
            }) => {
                // Only allow annotations in `ExprContext::Load`. If we have, e.g.,
                // `obj["foo"]["bar"]`, we need to avoid treating the `obj["foo"]`
                // portion as an annotation, despite having `ExprContext::Load`. Thus, we track
                // the `ExprContext` at the top-level.
                if self.semantic.in_subscript() {
                    visitor::walk_expr(self, expr);
                } else if matches!(ctx, ExprContext::Store | ExprContext::Del) {
                    self.semantic.flags |= SemanticModelFlags::SUBSCRIPT;
                    visitor::walk_expr(self, expr);
                } else {
                    self.visit_expr(value);

                    match typing::match_annotated_subscript(
                        value,
                        &self.semantic,
                        self.settings.typing_modules.iter().map(String::as_str),
                        &self.settings.pyflakes.extend_generics,
                    ) {
                        // Ex) Literal["Class"]
                        Some(typing::SubscriptKind::Literal) => {
                            self.semantic.flags |= SemanticModelFlags::TYPING_LITERAL;

                            self.visit_expr(slice);
                            self.visit_expr_context(ctx);
                        }
                        // Ex) Optional[int]
                        Some(typing::SubscriptKind::Generic) => {
                            self.visit_type_definition(slice);
                            self.visit_expr_context(ctx);
                        }
                        // Ex) Annotated[int, "Hello, world!"]
                        Some(typing::SubscriptKind::PEP593Annotation) => {
                            // First argument is a type (including forward references); the
                            // rest are arbitrary Python objects.
                            if let Expr::Tuple(ast::ExprTuple {
                                elts,
                                ctx,
                                range: _,
                                parenthesized: _,
                            }) = slice.as_ref()
                            {
                                let mut iter = elts.iter();
                                if let Some(expr) = iter.next() {
                                    self.visit_expr(expr);
                                }
                                for expr in iter {
                                    self.visit_non_type_definition(expr);
                                }
                                self.visit_expr_context(ctx);
                            } else {
                                debug!("Found non-Expr::Tuple argument to PEP 593 Annotation.");
                            }
                        }
                        None => {
                            self.visit_expr(slice);
                            self.visit_expr_context(ctx);
                        }
                    }
                }
            }
            Expr::StringLiteral(ast::ExprStringLiteral { value, .. }) => {
                if self.semantic.in_type_definition() && !self.semantic.in_typing_literal() {
                    self.visit.string_type_definitions.push((
                        expr.range(),
                        value.to_str(),
                        self.semantic.snapshot(),
                    ));
                }
            }
            Expr::FString(_) => {
                self.semantic.flags |= SemanticModelFlags::F_STRING;
                visitor::walk_expr(self, expr);
            }
            Expr::Named(ast::ExprNamed {
                target,
                value,
                range: _,
            }) => {
                self.visit_expr(value);

                self.semantic.flags |= SemanticModelFlags::NAMED_EXPRESSION_ASSIGNMENT;
                self.visit_expr(target);
            }
            _ => visitor::walk_expr(self, expr),
        }

        // Step 3: Clean-up
        match expr {
            Expr::Lambda(_)
            | Expr::Generator(_)
            | Expr::ListComp(_)
            | Expr::DictComp(_)
            | Expr::SetComp(_) => {
                self.analyze.scopes.push(self.semantic.scope_id);
                self.semantic.pop_scope();
            }
            _ => {}
        };

        // Step 4: Analysis
        analyze::expression(expr, self);
        match expr {
            Expr::StringLiteral(string_literal) => {
                analyze::string_like(string_literal.into(), self);
            }
            Expr::BytesLiteral(bytes_literal) => analyze::string_like(bytes_literal.into(), self),
            _ => {}
        }

        self.semantic.flags = flags_snapshot;
        self.semantic.pop_node();
    }

    fn visit_except_handler(&mut self, except_handler: &'a ExceptHandler) {
        let flags_snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::EXCEPTION_HANDLER;

        // Step 1: Binding
        let binding = match except_handler {
            ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler {
                type_: _,
                name,
                body: _,
                range: _,
            }) => {
                if let Some(name) = name {
                    // Store the existing binding, if any.
                    let binding_id = self.semantic.lookup_symbol(name.as_str());

                    // Add the bound exception name to the scope.
                    self.add_binding(
                        name.as_str(),
                        name.range(),
                        BindingKind::BoundException,
                        BindingFlags::empty(),
                    );

                    Some((name, binding_id))
                } else {
                    None
                }
            }
        };

        // Step 2: Traversal
        walk_except_handler(self, except_handler);

        // Step 3: Clean-up
        if let Some((name, binding_id)) = binding {
            self.add_binding(
                name.as_str(),
                name.range(),
                BindingKind::UnboundException(binding_id),
                BindingFlags::empty(),
            );
        }

        // Step 4: Analysis
        analyze::except_handler(except_handler, self);

        self.semantic.flags = flags_snapshot;
    }

    fn visit_parameters(&mut self, parameters: &'a Parameters) {
        // Step 1: Binding.
        // Bind, but intentionally avoid walking default expressions, as we handle them
        // upstream.
        for parameter_with_default in &parameters.posonlyargs {
            self.visit_parameter(&parameter_with_default.parameter);
        }
        for parameter_with_default in &parameters.args {
            self.visit_parameter(&parameter_with_default.parameter);
        }
        if let Some(arg) = &parameters.vararg {
            self.visit_parameter(arg);
        }
        for parameter_with_default in &parameters.kwonlyargs {
            self.visit_parameter(&parameter_with_default.parameter);
        }
        if let Some(arg) = &parameters.kwarg {
            self.visit_parameter(arg);
        }

        // Step 4: Analysis
        analyze::parameters(parameters, self);
    }

    fn visit_parameter(&mut self, parameter: &'a Parameter) {
        // Step 1: Binding.
        // Bind, but intentionally avoid walking the annotation, as we handle it
        // upstream.
        self.add_binding(
            &parameter.name,
            parameter.identifier(),
            BindingKind::Argument,
            BindingFlags::empty(),
        );

        // Step 4: Analysis
        analyze::parameter(parameter, self);
    }

    fn visit_pattern(&mut self, pattern: &'a Pattern) {
        // Step 1: Binding
        if let Pattern::MatchAs(ast::PatternMatchAs {
            name: Some(name), ..
        })
        | Pattern::MatchStar(ast::PatternMatchStar {
            name: Some(name),
            range: _,
        })
        | Pattern::MatchMapping(ast::PatternMatchMapping {
            rest: Some(name), ..
        }) = pattern
        {
            self.add_binding(
                name,
                name.range(),
                BindingKind::Assignment,
                BindingFlags::empty(),
            );
        }

        // Step 2: Traversal
        walk_pattern(self, pattern);
    }

    fn visit_body(&mut self, body: &'a [Stmt]) {
        // Step 4: Analysis
        analyze::suite(body, self);

        // Step 2: Traversal
        for stmt in body {
            self.visit_stmt(stmt);
        }
    }

    fn visit_match_case(&mut self, match_case: &'a MatchCase) {
        self.visit_pattern(&match_case.pattern);
        if let Some(expr) = &match_case.guard {
            self.visit_boolean_test(expr);
        }

        self.semantic.push_branch();
        self.visit_body(&match_case.body);
        self.semantic.pop_branch();
    }

    fn visit_type_param(&mut self, type_param: &'a ast::TypeParam) {
        // Step 1: Binding
        match type_param {
            ast::TypeParam::TypeVar(ast::TypeParamTypeVar { name, range, .. })
            | ast::TypeParam::TypeVarTuple(ast::TypeParamTypeVarTuple { name, range })
            | ast::TypeParam::ParamSpec(ast::TypeParamParamSpec { name, range }) => {
                self.add_binding(
                    name.as_str(),
                    *range,
                    BindingKind::TypeParam,
                    BindingFlags::empty(),
                );
            }
        }
        // Step 2: Traversal
        if let ast::TypeParam::TypeVar(ast::TypeParamTypeVar {
            bound: Some(bound), ..
        }) = type_param
        {
            self.visit
                .type_param_definitions
                .push((bound, self.semantic.snapshot()));
        }
    }

    fn visit_f_string_element(&mut self, f_string_element: &'a ast::FStringElement) {
        // Step 2: Traversal
        walk_f_string_element(self, f_string_element);

        // Step 4: Analysis
        if let Some(literal) = f_string_element.as_literal() {
            analyze::string_like(literal.into(), self);
        }
    }
}

impl<'a> Checker<'a> {
    /// Visit a [`Module`]. Returns `true` if the module contains a module-level docstring.
    fn visit_module(&mut self, python_ast: &'a Suite) {
        analyze::module(python_ast, self);
    }

    /// Visit a list of [`Comprehension`] nodes, assumed to be the comprehensions that compose a
    /// generator expression, like a list or set comprehension.
    fn visit_generators(&mut self, generators: &'a [Comprehension]) {
        let mut iterator = generators.iter();

        let Some(generator) = iterator.next() else {
            unreachable!("Generator expression must contain at least one generator");
        };

        let flags = self.semantic.flags;

        // Generators are compiled as nested functions. (This may change with PEP 709.)
        // As such, the `iter` of the first generator is evaluated in the outer scope, while all
        // subsequent nodes are evaluated in the inner scope.
        //
        // For example, given:
        // ```python
        // class A:
        //     T = range(10)
        //
        //     L = [x for x in T for y in T]
        // ```
        //
        // Conceptually, this is compiled as:
        // ```python
        // class A:
        //     T = range(10)
        //
        //     def foo(x=T):
        //         def bar(y=T):
        //             pass
        //         return bar()
        //     foo()
        // ```
        //
        // Following Python's scoping rules, the `T` in `x=T` is thus evaluated in the outer scope,
        // while all subsequent reads and writes are evaluated in the inner scope. In particular,
        // `x` is local to `foo`, and the `T` in `y=T` skips the class scope when resolving.
        self.visit_expr(&generator.iter);
        self.semantic.push_scope(ScopeKind::Generator);

        self.semantic.flags = flags | SemanticModelFlags::COMPREHENSION_ASSIGNMENT;
        self.visit_expr(&generator.target);
        self.semantic.flags = flags;

        for expr in &generator.ifs {
            self.visit_boolean_test(expr);
        }

        for generator in iterator {
            self.visit_expr(&generator.iter);

            self.semantic.flags = flags | SemanticModelFlags::COMPREHENSION_ASSIGNMENT;
            self.visit_expr(&generator.target);
            self.semantic.flags = flags;

            for expr in &generator.ifs {
                self.visit_boolean_test(expr);
            }
        }

        // Step 4: Analysis
        for generator in generators {
            analyze::comprehension(generator, self);
        }
    }

    /// Visit an body of [`Stmt`] nodes within a type-checking block.
    fn visit_type_checking_block(&mut self, body: &'a [Stmt]) {
        let snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::TYPE_CHECKING_BLOCK;
        self.visit_body(body);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as a runtime-evaluated type annotation.
    fn visit_runtime_evaluated_annotation(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::RUNTIME_EVALUATED_ANNOTATION;
        self.visit_type_definition(expr);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as a runtime-required type annotation.
    fn visit_runtime_required_annotation(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::RUNTIME_REQUIRED_ANNOTATION;
        self.visit_type_definition(expr);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as a type definition.
    fn visit_type_definition(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::TYPE_DEFINITION;
        self.visit_expr(expr);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as _not_ a type definition.
    fn visit_non_type_definition(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        self.semantic.flags -= SemanticModelFlags::TYPE_DEFINITION;
        self.visit_expr(expr);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as a boolean test. This is useful for detecting whether an
    /// expressions return value is significant, or whether the calling context only relies on
    /// its truthiness.
    fn visit_boolean_test(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::BOOLEAN_TEST;
        self.visit_expr(expr);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`ElifElseClause`]
    fn visit_elif_else_clause(&mut self, clause: &'a ElifElseClause) {
        if let Some(test) = &clause.test {
            self.visit_boolean_test(test);
        }
        self.visit_body(&clause.body);
    }

    /// Add a [`Binding`] to the current scope, bound to the given name.
    fn add_binding(
        &mut self,
        name: &'a str,
        range: TextRange,
        kind: BindingKind<'a>,
        flags: BindingFlags,
    ) -> BindingId {
        // Determine the scope to which the binding belongs.
        // Per [PEP 572](https://peps.python.org/pep-0572/#scope-of-the-target), named
        // expressions in generators and comprehensions bind to the scope that contains the
        // outermost comprehension.
        let scope_id = if kind.is_named_expr_assignment() {
            self.semantic
                .scopes
                .ancestor_ids(self.semantic.scope_id)
                .find_or_last(|scope_id| !self.semantic.scopes[*scope_id].kind.is_generator())
                .unwrap_or(self.semantic.scope_id)
        } else {
            self.semantic.scope_id
        };

        // Create the `Binding`.
        let binding_id = self.semantic.push_binding(range, kind, flags);

        // If the name is private, mark is as such.
        if name.starts_with('_') {
            self.semantic.bindings[binding_id].flags |= BindingFlags::PRIVATE_DECLARATION;
        }

        // If there's an existing binding in this scope, copy its references.
        if let Some(shadowed_id) = self.semantic.scopes[scope_id].get(name) {
            // If this is an annotation, and we already have an existing value in the same scope,
            // don't treat it as an assignment, but track it as a delayed annotation.
            if self.semantic.binding(binding_id).kind.is_annotation() {
                self.semantic
                    .add_delayed_annotation(shadowed_id, binding_id);
                return binding_id;
            }

            // Avoid shadowing builtins.
            let shadowed = &self.semantic.bindings[shadowed_id];
            if !matches!(
                shadowed.kind,
                BindingKind::Builtin | BindingKind::Deletion | BindingKind::UnboundException(_)
            ) {
                let references = shadowed.references.clone();
                let is_global = shadowed.is_global();
                let is_nonlocal = shadowed.is_nonlocal();

                // If the shadowed binding was global, then this one is too.
                if is_global {
                    self.semantic.bindings[binding_id].flags |= BindingFlags::GLOBAL;
                }

                // If the shadowed binding was non-local, then this one is too.
                if is_nonlocal {
                    self.semantic.bindings[binding_id].flags |= BindingFlags::NONLOCAL;
                }

                self.semantic.bindings[binding_id].references = references;
            }
        } else if let Some(shadowed_id) = self
            .semantic
            .scopes
            .ancestors(scope_id)
            .skip(1)
            .filter(|scope| scope.kind.is_function() || scope.kind.is_module())
            .find_map(|scope| scope.get(name))
        {
            // Otherwise, if there's an existing binding in a parent scope, mark it as shadowed.
            self.semantic
                .shadowed_bindings
                .insert(binding_id, shadowed_id);
        }

        // Add the binding to the scope.
        let scope = &mut self.semantic.scopes[scope_id];
        scope.add(name, binding_id);

        binding_id
    }

    fn bind_builtins(&mut self) {
        for builtin in PYTHON_BUILTINS
            .iter()
            .chain(MAGIC_GLOBALS.iter())
            .chain(
                self.source_type
                    .is_ipynb()
                    .then_some(IPYTHON_BUILTINS)
                    .into_iter()
                    .flatten(),
            )
            .copied()
            .chain(self.settings.builtins.iter().map(String::as_str))
        {
            // Add the builtin to the scope.
            let binding_id = self.semantic.push_builtin();
            let scope = self.semantic.global_scope_mut();
            scope.add(builtin, binding_id);
        }
    }

    fn handle_node_load(&mut self, expr: &Expr) {
        let Expr::Name(expr) = expr else {
            return;
        };
        self.semantic.resolve_load(expr);
    }

    fn handle_node_store(&mut self, id: &'a str, expr: &Expr) {
        let parent = self.semantic.current_statement();

        let mut flags = BindingFlags::empty();
        if helpers::is_unpacking_assignment(parent, expr) {
            flags.insert(BindingFlags::UNPACKED_ASSIGNMENT);
        }

        // Match the left-hand side of an annotated assignment, like `x` in `x: int`.
        if matches!(
            parent,
            Stmt::AnnAssign(ast::StmtAnnAssign { value: None, .. })
        ) && !self.semantic.in_annotation()
        {
            self.add_binding(id, expr.range(), BindingKind::Annotation, flags);
            return;
        }

        // A binding within a `for` must be a loop variable, as in:
        // ```python
        // for x in range(10):
        //     ...
        // ```
        if parent.is_for_stmt() {
            self.add_binding(id, expr.range(), BindingKind::LoopVar, flags);
            return;
        }

        // A binding within a `with` must be an item, as in:
        // ```python
        // with open("file.txt") as fp:
        //     ...
        // ```
        if parent.is_with_stmt() {
            self.add_binding(id, expr.range(), BindingKind::WithItemVar, flags);
            return;
        }

        let scope = self.semantic.current_scope();

        if scope.kind.is_module()
            && match parent {
                Stmt::Assign(ast::StmtAssign { targets, .. }) => {
                    if let Some(Expr::Name(ast::ExprName { id, .. })) = targets.first() {
                        id == "__all__"
                    } else {
                        false
                    }
                }
                Stmt::AugAssign(ast::StmtAugAssign { target, .. }) => {
                    if let Expr::Name(ast::ExprName { id, .. }) = target.as_ref() {
                        id == "__all__"
                    } else {
                        false
                    }
                }
                Stmt::AnnAssign(ast::StmtAnnAssign { target, .. }) => {
                    if let Expr::Name(ast::ExprName { id, .. }) = target.as_ref() {
                        id == "__all__"
                    } else {
                        false
                    }
                }
                _ => false,
            }
        {
            let (all_names, all_flags) =
                extract_all_names(parent, |name| self.semantic.is_builtin(name));

            if all_flags.intersects(DunderAllFlags::INVALID_OBJECT) {
                flags |= BindingFlags::INVALID_ALL_OBJECT;
            }
            if all_flags.intersects(DunderAllFlags::INVALID_FORMAT) {
                flags |= BindingFlags::INVALID_ALL_FORMAT;
            }

            self.add_binding(
                id,
                expr.range(),
                BindingKind::Export(Export {
                    names: all_names.into_boxed_slice(),
                }),
                flags,
            );
            return;
        }

        // If the expression is the left-hand side of a walrus operator, then it's a named
        // expression assignment, as in:
        // ```python
        // if (x := 10) > 5:
        //     ...
        // ```
        if self.semantic.in_named_expression_assignment() {
            self.add_binding(id, expr.range(), BindingKind::NamedExprAssignment, flags);
            return;
        }

        // If the expression is part of a comprehension target, then it's a comprehension variable
        // assignment, as in:
        // ```python
        // [x for x in range(10)]
        // ```
        if self.semantic.in_comprehension_assignment() {
            self.add_binding(id, expr.range(), BindingKind::ComprehensionVar, flags);
            return;
        }

        self.add_binding(id, expr.range(), BindingKind::Assignment, flags);
    }

    fn handle_node_delete(&mut self, expr: &'a Expr) {
        let Expr::Name(ast::ExprName { id, .. }) = expr else {
            return;
        };

        self.semantic.resolve_del(id, expr.range());

        if helpers::on_conditional_branch(&mut self.semantic.current_statements()) {
            return;
        }

        // Create a binding to model the deletion.
        let binding_id =
            self.semantic
                .push_binding(expr.range(), BindingKind::Deletion, BindingFlags::empty());
        let scope = self.semantic.current_scope_mut();
        scope.add(id, binding_id);
    }

    fn visit_deferred_future_type_definitions(&mut self) {
        let snapshot = self.semantic.snapshot();
        while !self.visit.future_type_definitions.is_empty() {
            let type_definitions = std::mem::take(&mut self.visit.future_type_definitions);
            for (expr, snapshot) in type_definitions {
                self.semantic.restore(snapshot);

                self.semantic.flags |= SemanticModelFlags::TYPE_DEFINITION
                    | SemanticModelFlags::FUTURE_TYPE_DEFINITION;
                self.visit_expr(expr);
            }
        }
        self.semantic.restore(snapshot);
    }

    fn visit_deferred_type_param_definitions(&mut self) {
        let snapshot = self.semantic.snapshot();
        while !self.visit.type_param_definitions.is_empty() {
            let type_params = std::mem::take(&mut self.visit.type_param_definitions);
            for (type_param, snapshot) in type_params {
                self.semantic.restore(snapshot);

                self.semantic.flags |=
                    SemanticModelFlags::TYPE_PARAM_DEFINITION | SemanticModelFlags::TYPE_DEFINITION;
                self.visit_expr(type_param);
            }
        }
        self.semantic.restore(snapshot);
    }

    fn visit_deferred_string_type_definitions(&mut self, allocator: &'a typed_arena::Arena<Expr>) {
        let snapshot = self.semantic.snapshot();
        while !self.visit.string_type_definitions.is_empty() {
            let type_definitions = std::mem::take(&mut self.visit.string_type_definitions);
            for (range, value, snapshot) in type_definitions {
                if let Ok((expr, kind)) =
                    parse_type_annotation(value, range, self.locator.contents())
                {
                    let expr = allocator.alloc(expr);

                    self.semantic.restore(snapshot);

                    if self.semantic.in_annotation() && self.semantic.future_annotations() {
                        if self.enabled(Rule::QuotedAnnotation) {
                            pyupgrade::rules::quoted_annotation(self, value, range);
                        }
                    }
                    if self.source_type.is_stub() {
                        if self.enabled(Rule::QuotedAnnotationInStub) {
                            flake8_pyi::rules::quoted_annotation_in_stub(self, value, range);
                        }
                    }

                    let type_definition_flag = match kind {
                        AnnotationKind::Simple => SemanticModelFlags::SIMPLE_STRING_TYPE_DEFINITION,
                        AnnotationKind::Complex => {
                            SemanticModelFlags::COMPLEX_STRING_TYPE_DEFINITION
                        }
                    };

                    self.semantic.flags |=
                        SemanticModelFlags::TYPE_DEFINITION | type_definition_flag;
                    self.visit_expr(expr);
                } else {
                    if self.enabled(Rule::ForwardAnnotationSyntaxError) {
                        self.diagnostics.push(Diagnostic::new(
                            pyflakes::rules::ForwardAnnotationSyntaxError {
                                body: value.to_string(),
                            },
                            range,
                        ));
                    }
                }
            }
        }
        self.semantic.restore(snapshot);
    }

    fn visit_deferred_functions(&mut self) {
        let snapshot = self.semantic.snapshot();
        while !self.visit.functions.is_empty() {
            let deferred_functions = std::mem::take(&mut self.visit.functions);
            for snapshot in deferred_functions {
                self.semantic.restore(snapshot);

                let Stmt::FunctionDef(ast::StmtFunctionDef {
                    body, parameters, ..
                }) = self.semantic.current_statement()
                else {
                    unreachable!("Expected Stmt::FunctionDef")
                };

                self.visit_parameters(parameters);
                // Set the docstring state before visiting the function body.
                self.docstring_state = DocstringState::Expected;
                self.visit_body(body);
            }
        }
        self.semantic.restore(snapshot);
    }

    /// Visit all deferred lambdas. Returns a list of snapshots, such that the caller can restore
    /// the semantic model to the state it was in before visiting the deferred lambdas.
    fn visit_deferred_lambdas(&mut self) {
        let snapshot = self.semantic.snapshot();
        while !self.visit.lambdas.is_empty() {
            let lambdas = std::mem::take(&mut self.visit.lambdas);
            for snapshot in lambdas {
                self.semantic.restore(snapshot);

                let Some(Expr::Lambda(ast::ExprLambda {
                    parameters,
                    body,
                    range: _,
                })) = self.semantic.current_expression()
                else {
                    unreachable!("Expected Expr::Lambda");
                };

                if let Some(parameters) = parameters {
                    self.visit_parameters(parameters);
                }
                self.visit_expr(body);
            }
        }
        self.semantic.restore(snapshot);
    }

    /// Recursively visit all deferred AST nodes, including lambdas, functions, and type
    /// annotations.
    fn visit_deferred(&mut self, allocator: &'a typed_arena::Arena<Expr>) {
        while !self.visit.is_empty() {
            self.visit_deferred_functions();
            self.visit_deferred_type_param_definitions();
            self.visit_deferred_lambdas();
            self.visit_deferred_future_type_definitions();
            self.visit_deferred_string_type_definitions(allocator);
        }
    }

    /// Run any lint rules that operate over the module exports (i.e., members of `__all__`).
    fn visit_exports(&mut self) {
        let snapshot = self.semantic.snapshot();

        let exports: Vec<(&str, TextRange)> = self
            .semantic
            .global_scope()
            .get_all("__all__")
            .map(|binding_id| &self.semantic.bindings[binding_id])
            .filter_map(|binding| match &binding.kind {
                BindingKind::Export(Export { names }) => {
                    Some(names.iter().map(|name| (*name, binding.range())))
                }
                _ => None,
            })
            .flatten()
            .collect();

        for (name, range) in exports {
            if let Some(binding_id) = self.semantic.global_scope().get(name) {
                // Mark anything referenced in `__all__` as used.
                // TODO(charlie): `range` here should be the range of the name in `__all__`, not
                // the range of `__all__` itself.
                self.semantic.add_global_reference(binding_id, range);
            } else {
                if self.semantic.global_scope().uses_star_imports() {
                    if self.enabled(Rule::UndefinedLocalWithImportStarUsage) {
                        self.diagnostics.push(Diagnostic::new(
                            pyflakes::rules::UndefinedLocalWithImportStarUsage {
                                name: (*name).to_string(),
                            },
                            range,
                        ));
                    }
                } else {
                    if self.enabled(Rule::UndefinedExport) {
                        if !self.path.ends_with("__init__.py") {
                            self.diagnostics.push(Diagnostic::new(
                                pyflakes::rules::UndefinedExport {
                                    name: (*name).to_string(),
                                },
                                range,
                            ));
                        }
                    }
                }
            }
        }

        self.semantic.restore(snapshot);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn check_ast(
    python_ast: &Suite,
    locator: &Locator,
    stylist: &Stylist,
    indexer: &Indexer,
    noqa_line_for: &NoqaMapping,
    settings: &LinterSettings,
    noqa: flags::Noqa,
    path: &Path,
    package: Option<&Path>,
    source_type: PySourceType,
    cell_offsets: Option<&CellOffsets>,
    notebook_index: Option<&NotebookIndex>,
) -> Vec<Diagnostic> {
    let module_path = package.and_then(|package| to_module_path(package, path));
    let module = Module {
        kind: if path.ends_with("__init__.py") {
            ModuleKind::Package
        } else {
            ModuleKind::Module
        },
        source: if let Some(module_path) = module_path.as_ref() {
            visibility::ModuleSource::Path(module_path)
        } else {
            visibility::ModuleSource::File(path)
        },
        python_ast,
    };

    let mut checker = Checker::new(
        settings,
        noqa_line_for,
        noqa,
        path,
        package,
        module,
        locator,
        stylist,
        indexer,
        Importer::new(python_ast, locator, stylist),
        source_type,
        cell_offsets,
        notebook_index,
    );
    checker.bind_builtins();

    // Iterate over the AST.
    checker.visit_module(python_ast);
    checker.visit_body(python_ast);

    // Visit any deferred syntax nodes. Take care to visit in order, such that we avoid adding
    // new deferred nodes after visiting nodes of that kind. For example, visiting a deferred
    // function can add a deferred lambda, but the opposite is not true.
    let allocator = typed_arena::Arena::new();
    checker.visit_deferred(&allocator);
    checker.visit_exports();

    // Check docstrings, bindings, and unresolved references.
    analyze::deferred_lambdas(&mut checker);
    analyze::deferred_for_loops(&mut checker);
    analyze::definitions(&mut checker);
    analyze::bindings(&mut checker);
    analyze::unresolved_references(&mut checker);

    // Reset the scope to module-level, and check all consumed scopes.
    checker.semantic.scope_id = ScopeId::global();
    checker.analyze.scopes.push(ScopeId::global());
    analyze::deferred_scopes(&mut checker);

    checker.diagnostics
}
